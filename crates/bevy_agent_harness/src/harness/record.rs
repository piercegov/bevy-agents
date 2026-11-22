//! Video recording over BRP: `harness.record.start`, `harness.record.stop`,
//! then `harness.record.result+watch` (streams one result once the file is
//! finished).
//!
//! Frames are sampled by the *virtual* clock, not by wall time: one video frame
//! per `1 / fps` seconds of game time. So a paused game adds nothing to the
//! video, stepped frames (`harness.step`) come out at exactly their game-time
//! spacing however long capture and encoding take, and a game running in real
//! time is sampled at `fps` with slow frames repeated to keep playback speed
//! right. Captured frames are piped as raw RGBA into an `ffmpeg` child process.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::camera::RenderTarget;
use bevy::input::ButtonState;
use bevy::input::mouse::MouseButtonInput;
use bevy::prelude::*;
use bevy::remote::{BrpError, BrpResult};
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};
use bevy::window::PrimaryWindow;
use serde::Deserialize;
use serde_json::{Value, json};

use super::overlay::{Overlay, RIPPLE_SECS, Ripple};
use super::screenshot::{resolve_target, to_rgba};
use super::{invalid_params, parse_params};

/// Overrides the `ffmpeg` executable used for encoding.
pub const FFMPEG_ENV: &str = "BEVY_AGENT_FFMPEG";

pub const DEFAULT_FPS: f64 = 60.0;
pub const DEFAULT_MAX_SECONDS: f64 = 120.0;

/// Captured frames waiting for the encoder before the game blocks.
const QUEUE_FRAMES: usize = 8;
/// How long to wait for captures still in flight after a stop.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
/// Tolerance when mapping game time to a frame index, so `dt = 1/60` steps
/// rounded to whole nanoseconds still land on consecutive frames.
const FRAME_EPSILON_SECS: f64 = 0.0005;

type SharedResult = Arc<Mutex<Option<Result<Value, String>>>>;

enum Msg {
    Frame {
        seq: u64,
        repeat: u32,
        image: Box<Image>,
        overlay: Option<Overlay>,
    },
    /// No more frames will be requested; `total` captures were.
    Finish { total: u64 },
}

/// What to record and how.
#[derive(Debug, Clone)]
pub struct RecordOptions {
    /// Output file. The extension picks the format: mp4, mov, mkv, webm or gif.
    pub path: PathBuf,
    /// Video frames per second of game time.
    pub fps: f64,
    /// The recording stops by itself after this much game time.
    pub max_seconds: f64,
    /// Draw the mouse onto the video: a cursor arrow, a highlight while a
    /// button is held and a ripple where one is pressed. Only the video gets
    /// it; the game and its screenshots are untouched.
    pub cursor: bool,
}

impl RecordOptions {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            fps: DEFAULT_FPS,
            max_seconds: DEFAULT_MAX_SECONDS,
            cursor: false,
        }
    }

    pub fn cursor(mut self, cursor: bool) -> Self {
        self.cursor = cursor;
        self
    }
}

struct Active {
    target: RenderTarget,
    path: PathBuf,
    fps: f64,
    /// Virtual elapsed time at the first recorded frame.
    start: Option<Duration>,
    /// Video frames requested so far (repeats included).
    emitted: u64,
    /// Captures requested so far.
    seq: u64,
    max_frames: u64,
    tx: SyncSender<Msg>,
    cursor: bool,
    /// Button presses still rippling: where (as a fraction of the window) and
    /// the video frame they started on.
    ripples: Vec<([f32; 2], u64)>,
}

/// The current recording, if any, and the outcome of the last one.
#[derive(Resource, Default)]
pub struct Recorder {
    active: Option<Active>,
    last: Option<SharedResult>,
}

impl Recorder {
    pub fn is_recording(&self) -> bool {
        self.active.is_some()
    }

    /// Outcome of the most recent recording: `None` while it is still being
    /// recorded or encoded.
    pub fn result(&self) -> Option<Result<Value, String>> {
        if self.active.is_some() {
            return None;
        }
        match &self.last {
            Some(shared) => shared.lock().unwrap().clone(),
            None => Some(Err("no recording has been started".into())),
        }
    }

    /// Stop requesting frames. Returns false if nothing was being recorded.
    pub fn stop(&mut self) -> bool {
        self.finish(false)
    }

    fn finish(&mut self, discard_in_flight: bool) -> bool {
        let Some(active) = self.active.take() else {
            return false;
        };
        let total = if discard_in_flight { 0 } else { active.seq };
        let _ = active.tx.send(Msg::Finish { total });
        true
    }

    pub(crate) fn status(&self) -> Value {
        match &self.active {
            Some(a) => json!({
                "path": a.path,
                "fps": a.fps,
                "frames": a.emitted,
                "seconds": a.emitted as f64 / a.fps,
            }),
            None => Value::Null,
        }
    }
}

pub(crate) fn plugin(app: &mut App) {
    app.init_resource::<Recorder>()
        .add_systems(Last, capture_frames);
}

fn ffmpeg_program() -> String {
    std::env::var(FFMPEG_ENV).unwrap_or_else(|_| "ffmpeg".into())
}

/// Encoder arguments for the output format, chosen by file extension.
fn output_args(path: &Path) -> Result<Vec<&'static str>, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // yuv420p needs even dimensions.
    const EVEN: &str = "pad=ceil(iw/2)*2:ceil(ih/2)*2";
    let x264 = [
        "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p", "-vf", EVEN,
    ];
    Ok(match ext.as_str() {
        "mp4" | "m4v" | "mov" => {
            let mut args = x264.to_vec();
            args.extend(["-movflags", "+faststart"]);
            args
        }
        "mkv" => x264.to_vec(),
        "webm" => vec![
            "-c:v",
            "libvpx-vp9",
            "-crf",
            "32",
            "-b:v",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            EVEN,
        ],
        "gif" => vec!["-vf", "split[a][b];[a]palettegen[p];[b][p]paletteuse"],
        other => {
            return Err(format!(
                "unsupported video extension `.{other}`: use .mp4, .mov, .mkv, .webm or .gif"
            ));
        }
    })
}

/// Start recording `target` to `opts.path`.
pub fn start(world: &mut World, target: RenderTarget, opts: RecordOptions) -> Result<(), String> {
    world.init_resource::<Recorder>();
    if let Some(active) = &world.resource::<Recorder>().active {
        return Err(format!(
            "already recording to {}; stop it first",
            active.path.display()
        ));
    }
    if !(opts.fps.is_finite() && (1.0..=240.0).contains(&opts.fps)) {
        return Err("`fps` must be between 1 and 240".into());
    }
    if !(opts.max_seconds.is_finite() && opts.max_seconds > 0.0) {
        return Err("`max_seconds` must be a positive number".into());
    }
    let path = if opts.path.is_absolute() {
        opts.path
    } else {
        std::env::current_dir()
            .map(|d| d.join(&opts.path))
            .unwrap_or(opts.path)
    };
    let args = output_args(&path)?;

    let ffmpeg = ffmpeg_program();
    let found = Command::new(&ffmpeg)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !found.is_ok_and(|s| s.success()) {
        return Err(format!(
            "recording needs ffmpeg, but `{ffmpeg}` could not be run; install ffmpeg or point \
             {FFMPEG_ENV} at it"
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }

    let (tx, rx) = sync_channel(QUEUE_FRAMES);
    let shared = SharedResult::default();
    let writer = Writer {
        ffmpeg,
        args,
        path: path.clone(),
        fps: opts.fps,
        encoder: None,
        frames: 0,
        error: None,
    };
    let result = shared.clone();
    std::thread::Builder::new()
        .name("bevy-agent-record".into())
        .spawn(move || writer.run(rx, result))
        .map_err(|e| format!("starting the recording thread: {e}"))?;

    let mut recorder = world.resource_mut::<Recorder>();
    recorder.last = Some(shared);
    recorder.active = Some(Active {
        target,
        path,
        fps: opts.fps,
        start: None,
        emitted: 0,
        seq: 0,
        max_frames: (opts.max_seconds * opts.fps).ceil().max(1.0) as u64,
        tx,
        cursor: opts.cursor,
        ripples: Vec::new(),
    });
    Ok(())
}

/// Stop and wait for the file to be finished, giving up on captures that have
/// not arrived yet. For use when the app is about to exit.
pub fn finish_blocking(world: &mut World, limit: Duration) {
    let Some(mut recorder) = world.get_resource_mut::<Recorder>() else {
        return;
    };
    if !recorder.finish(true) {
        return;
    }
    let deadline = Instant::now() + limit;
    while recorder.result().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn capture_frames(
    mut commands: Commands,
    mut recorder: ResMut<Recorder>,
    time: Res<Time<Virtual>>,
    windows: Query<(&Window, Option<&PrimaryWindow>)>,
    buttons: Option<Res<ButtonInput<MouseButton>>>,
    presses: Option<MessageReader<MouseButtonInput>>,
) {
    // Read every frame, so presses from before a recording never show up in it.
    let pressed = presses.is_some_and(|mut presses| {
        presses
            .read()
            .filter(|press| press.state == ButtonState::Pressed)
            .count()
            > 0
    });
    let Some(active) = recorder.bypass_change_detection().active.as_mut() else {
        return;
    };
    // The cursor as a fraction of the window, which is what the video shows.
    let cursor = if active.cursor {
        let mut found = None;
        for (window, primary) in &windows {
            if primary.is_some() || found.is_none() {
                found = Some(window);
            }
        }
        found.and_then(|window| {
            let position = window.cursor_position()?;
            Some([position.x / window.width(), position.y / window.height()])
        })
    } else {
        None
    };
    if pressed && let Some(cursor) = cursor {
        active.ripples.push((cursor, active.emitted));
    }

    let start = *active.start.get_or_insert(time.elapsed());
    let recorded = time.elapsed().saturating_sub(start).as_secs_f64();
    // Frame 0 is the picture at the start; frame n shows game time n / fps.
    let due = ((recorded + FRAME_EPSILON_SECS) * active.fps).floor() as u64 + 1;
    let due = due.min(active.max_frames);
    if due > active.emitted {
        let first = active.emitted;
        let repeat = (due - active.emitted) as u32;
        active.emitted = due;
        let seq = active.seq;
        active.seq += 1;
        let fps = active.fps as f32;
        let overlay = active.cursor.then(|| Overlay {
            cursor,
            held: buttons
                .as_ref()
                .is_some_and(|buttons| buttons.get_pressed().next().is_some()),
            ripples: active
                .ripples
                .iter()
                .map(|&(at, started)| Ripple {
                    at,
                    age: (first - started) as f32 / fps,
                })
                .collect(),
        });
        active
            .ripples
            .retain(|&(_, started)| ((due - started) as f32 / fps) < RIPPLE_SECS);
        let tx = active.tx.clone();
        commands.spawn(Screenshot(active.target.clone())).observe(
            move |captured: On<ScreenshotCaptured>| {
                // Blocks while the encoder is behind, so no frame is dropped.
                let _ = tx.send(Msg::Frame {
                    seq,
                    repeat,
                    image: Box::new(captured.event().image.clone()),
                    overlay: overlay.clone(),
                });
            },
        );
    }
    if active.emitted >= active.max_frames {
        info!(
            "bevy_agent_harness: recording reached its time limit, finishing {}",
            active.path.display()
        );
        recorder.stop();
    }
}

struct Encoder {
    child: Child,
    stdin: ChildStdin,
    stderr: std::thread::JoinHandle<String>,
    width: u32,
    height: u32,
}

struct Writer {
    ffmpeg: String,
    args: Vec<&'static str>,
    path: PathBuf,
    fps: f64,
    encoder: Option<Encoder>,
    frames: u64,
    error: Option<String>,
}

impl Writer {
    fn run(mut self, rx: Receiver<Msg>, result: SharedResult) {
        // Captures can complete out of order; write them by sequence number.
        let mut pending: BTreeMap<u64, (u32, Box<Image>, Option<Overlay>)> = BTreeMap::new();
        let mut next = 0u64;
        let mut total: Option<u64> = None;
        loop {
            if total.is_some_and(|t| next >= t) {
                break;
            }
            let msg = match total {
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(_) => rx.recv_timeout(DRAIN_TIMEOUT),
            };
            match msg {
                Ok(Msg::Frame {
                    seq,
                    repeat,
                    image,
                    overlay,
                }) => {
                    pending.insert(seq, (repeat, image, overlay));
                }
                Ok(Msg::Finish { total: t }) => total = Some(t),
                Err(_) => break,
            }
            // A capture that never arrives must not hold up the rest.
            if pending.len() > QUEUE_FRAMES * 2 {
                next = *pending.keys().next().unwrap();
            }
            while let Some((repeat, image, overlay)) = pending.remove(&next) {
                next += 1;
                if self.error.is_none()
                    && let Err(e) = self.write(*image, repeat, overlay)
                {
                    self.error = Some(e);
                }
            }
        }
        *result.lock().unwrap() = Some(self.finish());
    }

    fn write(&mut self, image: Image, repeat: u32, overlay: Option<Overlay>) -> Result<(), String> {
        let mut rgba = to_rgba(image)?;
        if self.encoder.is_none() {
            let (width, height) = rgba.dimensions();
            self.encoder = Some(self.spawn_encoder(width, height)?);
        }
        let encoder = self.encoder.as_mut().unwrap();
        if rgba.dimensions() != (encoder.width, encoder.height) {
            // The window was resized mid-recording; the video keeps its size.
            rgba = image::imageops::resize(
                &rgba,
                encoder.width,
                encoder.height,
                image::imageops::FilterType::Triangle,
            );
        }
        // A ripple keeps moving through repeated frames; everything else is
        // drawn once.
        let mut drawn: Option<image::RgbaImage> = None;
        for k in 0..repeat {
            if let Some(overlay) = &overlay
                && (drawn.is_none() || overlay.is_animated())
            {
                let mut frame = rgba.clone();
                overlay
                    .advanced(k as f32 / self.fps as f32)
                    .draw(&mut frame);
                drawn = Some(frame);
            }
            encoder
                .stdin
                .write_all(drawn.as_ref().unwrap_or(&rgba).as_raw())
                .map_err(|e| format!("writing to ffmpeg: {e}"))?;
            self.frames += 1;
        }
        Ok(())
    }

    fn spawn_encoder(&self, width: u32, height: u32) -> Result<Encoder, String> {
        let mut child = Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba"])
            .args(["-video_size", &format!("{width}x{height}")])
            .args(["-framerate", &self.fps.to_string()])
            .args(["-i", "pipe:0"])
            .args(&self.args)
            .arg(&self.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("starting {}: {e}", self.ffmpeg))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Ok(Encoder {
            child,
            stdin,
            stderr,
            width,
            height,
        })
    }

    fn finish(self) -> Result<Value, String> {
        let Some(encoder) = self.encoder else {
            return Err(self
                .error
                .unwrap_or_else(|| "no frames were captured".into()));
        };
        let Encoder {
            mut child,
            stdin,
            stderr,
            width,
            height,
        } = encoder;
        drop(stdin);
        let status = child
            .wait()
            .map_err(|e| format!("waiting for ffmpeg: {e}"))?;
        let stderr = stderr.join().unwrap_or_default();
        if !status.success() || self.error.is_some() {
            let mut message = self
                .error
                .unwrap_or_else(|| format!("ffmpeg failed ({status})"));
            let detail = stderr.trim();
            if !detail.is_empty() {
                message = format!("{message}: {detail}");
            }
            return Err(message);
        }
        Ok(json!({
            "path": self.path,
            "width": width,
            "height": height,
            "fps": self.fps,
            "frames": self.frames,
            "seconds": self.frames as f64 / self.fps,
            "bytes": std::fs::metadata(&self.path).map(|m| m.len()).ok(),
        }))
    }
}

#[derive(Deserialize, Default)]
struct StartParams {
    /// Output file; relative paths are resolved against the game's working directory.
    path: Option<PathBuf>,
    #[serde(default)]
    fps: Option<f64>,
    #[serde(default)]
    max_seconds: Option<f64>,
    #[serde(default)]
    camera: Option<u64>,
    #[serde(default)]
    window: Option<u64>,
    /// Draw the mouse cursor and click ripples onto the video.
    #[serde(default)]
    cursor: bool,
}

/// `harness.record.start`
pub fn start_method(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: StartParams = parse_params(params)?;
    let path = p.path.ok_or_else(|| invalid_params("`path` is required"))?;
    let target = resolve_target(world, p.camera, p.window)?;
    let opts = RecordOptions {
        path,
        fps: p.fps.unwrap_or(DEFAULT_FPS),
        max_seconds: p.max_seconds.unwrap_or(DEFAULT_MAX_SECONDS),
        cursor: p.cursor,
    };
    start(world, target, opts).map_err(invalid_params)?;
    Ok(world.resource::<Recorder>().status())
}

/// `harness.record.stop`
pub fn stop_method(In(_): In<Option<Value>>, world: &mut World) -> BrpResult {
    let mut recorder = world.resource_mut::<Recorder>();
    let status = recorder.status();
    // Stopping twice, or after the time limit, is fine: the result is still there.
    if !recorder.stop() && recorder.last.is_none() {
        return Err(invalid_params("no recording has been started"));
    }
    Ok(status)
}

/// `harness.record.result+watch`
pub fn poll_result(In(_): In<Option<Value>>, recorder: Res<Recorder>) -> BrpResult<Option<Value>> {
    match recorder.result() {
        None => Ok(None),
        Some(Ok(v)) => Ok(Some(v)),
        Some(Err(e)) => Err(BrpError::internal(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert!(output_args(Path::new("a/demo.mp4")).is_ok());
        assert!(output_args(Path::new("demo.GIF")).is_ok());
        assert!(output_args(Path::new("demo.avi")).is_err());
        assert!(output_args(Path::new("demo")).is_err());
    }
}
