//! Input injection. Actions are queued and delivered as ordinary Bevy input
//! messages (`KeyboardInput`, `MouseButtonInput`, `CursorMoved`, ...) at the
//! start of the next frame, so game code cannot tell them apart from real input.

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput, NativeKey};
use bevy::input::mouse::{MouseButtonInput, MouseMotion, MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::remote::BrpResult;
use bevy::window::{CursorMoved, PrimaryWindow, WindowEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::parse_params;

/// One injectable input action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputAction {
    /// Press or release a key.
    Key {
        key: KeyCode,
        state: ButtonState,
        /// Logical key; derived from `key` when omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        logical: Option<Key>,
    },
    /// Press or release a mouse button.
    MouseButton {
        button: MouseButton,
        state: ButtonState,
    },
    /// Move the cursor to a position in logical window pixels (origin top-left).
    Cursor { position: [f32; 2] },
    /// Relative mouse motion (for FPS-style camera controls).
    Motion { delta: [f32; 2] },
    /// Scroll wheel.
    Scroll {
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
        #[serde(default)]
        unit: ScrollUnit,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScrollUnit {
    #[default]
    Line,
    Pixel,
}

/// Queue of actions to deliver. `delay` is in frames; `0` means "next frame".
#[derive(Resource, Default, Debug)]
pub struct InputQueue {
    pending: Vec<(u32, InputAction)>,
}

impl InputQueue {
    /// Deliver `action` after `delay` frames (0 = next frame).
    pub fn push(&mut self, delay: u32, action: InputAction) {
        self.pending.push((delay, action));
    }

    /// Press now (next frame) and release after `hold_frames` frames.
    pub fn tap_key(&mut self, key: KeyCode, hold_frames: u32) {
        self.push(
            0,
            InputAction::Key {
                key,
                state: ButtonState::Pressed,
                logical: None,
            },
        );
        self.push(
            hold_frames.max(1),
            InputAction::Key {
                key,
                state: ButtonState::Released,
                logical: None,
            },
        );
    }

    pub fn tap_mouse(&mut self, button: MouseButton, hold_frames: u32) {
        self.push(
            0,
            InputAction::MouseButton {
                button,
                state: ButtonState::Pressed,
            },
        );
        self.push(
            hold_frames.max(1),
            InputAction::MouseButton {
                button,
                state: ButtonState::Released,
            },
        );
    }

    /// Largest delay currently queued, so callers can chain sequential actions.
    pub fn horizon(&self) -> u32 {
        self.pending.iter().map(|(d, _)| *d + 1).max().unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// How the speed of a [`Drag`] changes along its path.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Ease {
    /// Constant speed.
    Linear,
    /// Speeds up at the start and slows down at the end, like a hand does.
    #[default]
    Smooth,
}

/// A cursor movement spread over several frames: a drag when a button is held,
/// a glide when not. With more than two points the path is a smooth curve
/// through all of them.
///
/// Every frame gets a `CursorMoved` and a matching `MouseMotion`, as a real
/// mouse produces, so both cursor-based and motion-based controls react.
#[derive(Debug, Clone, PartialEq)]
pub struct Drag {
    /// Start, any waypoints, and end, in logical window pixels.
    pub points: Vec<Vec2>,
    /// Frames the movement takes.
    pub frames: u32,
    /// Button held during the movement, if any.
    pub button: Option<MouseButton>,
    pub ease: Ease,
}

impl Drag {
    /// A left-button drag from `from` to `to` over 30 frames.
    pub fn new(from: Vec2, to: Vec2) -> Self {
        Self {
            points: vec![from, to],
            frames: 30,
            button: Some(MouseButton::Left),
            ease: Ease::Smooth,
        }
    }

    /// Pass through `point` on the way (after any earlier waypoints).
    pub fn via(mut self, point: Vec2) -> Self {
        let end = self.points.len() - 1;
        self.points.insert(end, point);
        self
    }

    pub fn frames(mut self, frames: u32) -> Self {
        self.frames = frames;
        self
    }

    /// Which button to hold; `None` only moves the cursor.
    pub fn button(mut self, button: Option<MouseButton>) -> Self {
        self.button = button;
        self
    }

    pub fn ease(mut self, ease: Ease) -> Self {
        self.ease = ease;
        self
    }

    /// Frames from the first action to the last: one to arrive (and press),
    /// `frames` of movement, and one to release when a button is held.
    pub fn total_frames(&self) -> u32 {
        self.frames.max(1) + if self.button.is_some() { 2 } else { 1 }
    }

    /// The path as a dense polyline: straight for two points, otherwise a
    /// Catmull-Rom spline through every point.
    fn polyline(&self) -> Vec<Vec2> {
        let p = &self.points;
        if p.len() <= 2 {
            return p.clone();
        }
        const SAMPLES: usize = 24;
        let at = |i: isize| p[i.clamp(0, p.len() as isize - 1) as usize];
        let mut line = vec![p[0]];
        for seg in 0..p.len() as isize - 1 {
            let (p0, p1, p2, p3) = (at(seg - 1), at(seg), at(seg + 1), at(seg + 2));
            for k in 1..=SAMPLES {
                let t = k as f32 / SAMPLES as f32;
                let (t2, t3) = (t * t, t * t * t);
                line.push(
                    0.5 * ((2.0 * p1)
                        + (p2 - p0) * t
                        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
                        + (3.0 * p1 - p0 - 3.0 * p2 + p3) * t3),
                );
            }
        }
        line
    }

    /// Cursor position after each of the `frames` movement frames; the last
    /// one is exactly the end point.
    pub fn positions(&self) -> Vec<Vec2> {
        let line = self.polyline();
        let frames = self.frames.max(1);
        let Some(&end) = line.last() else {
            return Vec::new();
        };
        let mut lengths = vec![0.0f32];
        for pair in line.windows(2) {
            lengths.push(lengths.last().unwrap() + pair[0].distance(pair[1]));
        }
        let total = *lengths.last().unwrap();
        (1..=frames)
            .map(|i| {
                if i == frames || total <= f32::EPSILON {
                    return end;
                }
                let t = i as f32 / frames as f32;
                let t = match self.ease {
                    Ease::Linear => t,
                    Ease::Smooth => t * t * (3.0 - 2.0 * t),
                };
                // Walk the polyline by distance so speed does not depend on
                // how far apart the waypoints are.
                let target = t * total;
                let seg = lengths
                    .partition_point(|&l| l < target)
                    .clamp(1, line.len() - 1);
                let span = lengths[seg] - lengths[seg - 1];
                let local = if span > 0.0 {
                    (target - lengths[seg - 1]) / span
                } else {
                    0.0
                };
                line[seg - 1].lerp(line[seg], local)
            })
            .collect()
    }

    /// The queue entries for this movement, as `(delay, action)`.
    pub fn actions(&self) -> Vec<(u32, InputAction)> {
        let Some(&start) = self.points.first() else {
            return Vec::new();
        };
        let mut actions = vec![(
            0,
            InputAction::Cursor {
                position: start.into(),
            },
        )];
        if let Some(button) = self.button {
            actions.push((
                0,
                InputAction::MouseButton {
                    button,
                    state: ButtonState::Pressed,
                },
            ));
        }
        let mut previous = start;
        let mut delay = 0;
        for position in self.positions() {
            delay += 1;
            actions.push((
                delay,
                InputAction::Cursor {
                    position: position.into(),
                },
            ));
            actions.push((
                delay,
                InputAction::Motion {
                    delta: (position - previous).into(),
                },
            ));
            previous = position;
        }
        if let Some(button) = self.button {
            actions.push((
                delay + 1,
                InputAction::MouseButton {
                    button,
                    state: ButtonState::Released,
                },
            ));
        }
        actions
    }
}

pub(crate) fn plugin(app: &mut App) {
    // In `First`, ahead of picking's input pass (which runs there) and of
    // `InputSystems` in `PreUpdate`, so both see an action in the frame it is delivered.
    app.init_resource::<InputQueue>().add_systems(
        First,
        deliver_inputs.before(bevy::picking::PickingSystems::Input),
    );
}

fn deliver_inputs(
    mut queue: ResMut<InputQueue>,
    mut windows: Query<(Entity, &mut Window, Option<&PrimaryWindow>)>,
    mut keys: MessageWriter<KeyboardInput>,
    mut buttons: MessageWriter<MouseButtonInput>,
    mut cursor: MessageWriter<CursorMoved>,
    mut motion: MessageWriter<MouseMotion>,
    mut wheel: MessageWriter<MouseWheel>,
    // winit also reports every input through this combined stream, and some
    // consumers (bevy_picking, and so UI clicks) read only that.
    mut window_events: Option<ResMut<Messages<WindowEvent>>>,
) {
    let mut mirror = |event: WindowEvent| {
        if let Some(events) = window_events.as_mut() {
            events.write(event);
        }
    };
    if queue.is_empty() {
        return;
    }
    // Prefer the primary window, otherwise any window, otherwise a placeholder.
    let mut target = None;
    for (e, _, primary) in windows.iter() {
        if primary.is_some() {
            target = Some(e);
            break;
        }
        target.get_or_insert(e);
    }
    let window = target.unwrap_or(Entity::PLACEHOLDER);

    let mut due = Vec::new();
    queue.pending.retain_mut(|(delay, action)| {
        if *delay == 0 {
            due.push(action.clone());
            false
        } else {
            *delay -= 1;
            true
        }
    });

    for action in due {
        match action {
            InputAction::Key {
                key,
                state,
                logical,
            } => {
                let logical_key = logical.unwrap_or_else(|| logical_key_for(key));
                let text = match (&logical_key, state) {
                    (Key::Character(c), ButtonState::Pressed) => Some(c.clone()),
                    (Key::Space, ButtonState::Pressed) => Some(" ".into()),
                    _ => None,
                };
                let event = KeyboardInput {
                    key_code: key,
                    logical_key,
                    state,
                    text,
                    repeat: false,
                    window,
                };
                mirror(event.clone().into());
                keys.write(event);
            }
            InputAction::MouseButton { button, state } => {
                let event = MouseButtonInput {
                    button,
                    state,
                    window,
                };
                mirror(event.into());
                buttons.write(event);
            }
            InputAction::Cursor { position } => {
                let position = Vec2::from(position);
                let mut delta = None;
                if let Ok((_, mut win, _)) = windows.get_mut(window) {
                    delta = win.cursor_position().map(|prev| position - prev);
                    let scale = win.scale_factor() as f64;
                    // Not flagged as a change on purpose: bevy_winit answers a changed
                    // cursor position by warping the real OS pointer, which moves the
                    // user's mouse on X11/Windows/macOS and logs an error on Wayland.
                    // Game code learns about the move from `CursorMoved`, like a real one.
                    win.bypass_change_detection()
                        .set_physical_cursor_position(Some(position.as_dvec2() * scale));
                }
                let event = CursorMoved {
                    window,
                    position,
                    delta,
                };
                mirror(event.clone().into());
                cursor.write(event);
            }
            InputAction::Motion { delta } => {
                let event = MouseMotion {
                    delta: Vec2::from(delta),
                };
                mirror(event.into());
                motion.write(event);
            }
            InputAction::Scroll { x, y, unit } => {
                let event = MouseWheel {
                    unit: match unit {
                        ScrollUnit::Line => MouseScrollUnit::Line,
                        ScrollUnit::Pixel => MouseScrollUnit::Pixel,
                    },
                    x,
                    y,
                    window,
                    phase: bevy::input::touch::TouchPhase::Moved,
                };
                mirror(event.into());
                wheel.write(event);
            }
        }
    }
}

/// Best-effort mapping from a physical key to the logical key a US layout produces.
pub fn logical_key_for(key: KeyCode) -> Key {
    use KeyCode::*;
    let ch = |c: &str| Key::Character(c.into());
    match key {
        KeyA => ch("a"),
        KeyB => ch("b"),
        KeyC => ch("c"),
        KeyD => ch("d"),
        KeyE => ch("e"),
        KeyF => ch("f"),
        KeyG => ch("g"),
        KeyH => ch("h"),
        KeyI => ch("i"),
        KeyJ => ch("j"),
        KeyK => ch("k"),
        KeyL => ch("l"),
        KeyM => ch("m"),
        KeyN => ch("n"),
        KeyO => ch("o"),
        KeyP => ch("p"),
        KeyQ => ch("q"),
        KeyR => ch("r"),
        KeyS => ch("s"),
        KeyT => ch("t"),
        KeyU => ch("u"),
        KeyV => ch("v"),
        KeyW => ch("w"),
        KeyX => ch("x"),
        KeyY => ch("y"),
        KeyZ => ch("z"),
        Digit0 => ch("0"),
        Digit1 => ch("1"),
        Digit2 => ch("2"),
        Digit3 => ch("3"),
        Digit4 => ch("4"),
        Digit5 => ch("5"),
        Digit6 => ch("6"),
        Digit7 => ch("7"),
        Digit8 => ch("8"),
        Digit9 => ch("9"),
        Minus => ch("-"),
        Equal => ch("="),
        BracketLeft => ch("["),
        BracketRight => ch("]"),
        Backslash => ch("\\"),
        Semicolon => ch(";"),
        Quote => ch("'"),
        Comma => ch(","),
        Period => ch("."),
        Slash => ch("/"),
        Backquote => ch("`"),
        Space => Key::Space,
        Enter | NumpadEnter => Key::Enter,
        Tab => Key::Tab,
        Backspace => Key::Backspace,
        Delete => Key::Delete,
        Escape => Key::Escape,
        ArrowUp => Key::ArrowUp,
        ArrowDown => Key::ArrowDown,
        ArrowLeft => Key::ArrowLeft,
        ArrowRight => Key::ArrowRight,
        ShiftLeft | ShiftRight => Key::Shift,
        ControlLeft | ControlRight => Key::Control,
        AltLeft | AltRight => Key::Alt,
        SuperLeft | SuperRight => Key::Super,
        Home => Key::Home,
        End => Key::End,
        PageUp => Key::PageUp,
        PageDown => Key::PageDown,
        Insert => Key::Insert,
        CapsLock => Key::CapsLock,
        F1 => Key::F1,
        F2 => Key::F2,
        F3 => Key::F3,
        F4 => Key::F4,
        F5 => Key::F5,
        F6 => Key::F6,
        F7 => Key::F7,
        F8 => Key::F8,
        F9 => Key::F9,
        F10 => Key::F10,
        F11 => Key::F11,
        F12 => Key::F12,
        _ => Key::Unidentified(NativeKey::Unidentified),
    }
}

// ---- BRP handlers ---------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PressKind {
    #[default]
    Tap,
    Press,
    Release,
}

#[derive(Deserialize, Default)]
struct KeyParams {
    /// Either a single key or a list of keys, tapped in sequence.
    #[serde(default)]
    key: Option<KeyCode>,
    #[serde(default)]
    keys: Vec<KeyCode>,
    #[serde(default)]
    state: PressKind,
    /// Frames to hold for `tap`. Default 1.
    #[serde(default)]
    hold_frames: Option<u32>,
    /// Frames to wait before the first action. Default 0.
    #[serde(default)]
    delay: u32,
}

fn queued(queue: &InputQueue) -> Value {
    json!({ "queued": queue.len(), "horizon_frames": queue.horizon() })
}

/// `harness.input.key`
pub fn inject_key(In(params): In<Option<Value>>, mut queue: ResMut<InputQueue>) -> BrpResult {
    let p: KeyParams = parse_params(params)?;
    let mut keys = p.keys;
    if let Some(k) = p.key {
        keys.insert(0, k);
    }
    if keys.is_empty() {
        return Err(super::invalid_params("expected `key` or `keys`"));
    }
    let hold = p.hold_frames.unwrap_or(1).max(1);
    let mut delay = p.delay;
    for key in keys {
        match p.state {
            PressKind::Tap => {
                queue.push(
                    delay,
                    InputAction::Key {
                        key,
                        state: ButtonState::Pressed,
                        logical: None,
                    },
                );
                queue.push(
                    delay + hold,
                    InputAction::Key {
                        key,
                        state: ButtonState::Released,
                        logical: None,
                    },
                );
                // Sequential taps: leave a frame gap so `just_pressed` fires per key.
                delay += hold + 1;
            }
            PressKind::Press => {
                queue.push(
                    delay,
                    InputAction::Key {
                        key,
                        state: ButtonState::Pressed,
                        logical: None,
                    },
                );
            }
            PressKind::Release => {
                queue.push(
                    delay,
                    InputAction::Key {
                        key,
                        state: ButtonState::Released,
                        logical: None,
                    },
                );
            }
        }
    }
    Ok(queued(&queue))
}

#[derive(Deserialize)]
struct MouseParams {
    #[serde(default = "default_button")]
    button: MouseButton,
    #[serde(default)]
    state: PressKind,
    #[serde(default)]
    position: Option<[f32; 2]>,
    #[serde(default)]
    hold_frames: Option<u32>,
    #[serde(default)]
    delay: u32,
}
fn default_button() -> MouseButton {
    MouseButton::Left
}
impl Default for MouseParams {
    fn default() -> Self {
        Self {
            button: MouseButton::Left,
            state: PressKind::Tap,
            position: None,
            hold_frames: None,
            delay: 0,
        }
    }
}

/// `harness.input.mouse`
pub fn inject_mouse(In(params): In<Option<Value>>, mut queue: ResMut<InputQueue>) -> BrpResult {
    let p: MouseParams = parse_params(params)?;
    let hold = p.hold_frames.unwrap_or(1).max(1);
    if let Some(position) = p.position {
        queue.push(p.delay, InputAction::Cursor { position });
    }
    match p.state {
        PressKind::Tap => {
            queue.push(
                p.delay,
                InputAction::MouseButton {
                    button: p.button,
                    state: ButtonState::Pressed,
                },
            );
            queue.push(
                p.delay + hold,
                InputAction::MouseButton {
                    button: p.button,
                    state: ButtonState::Released,
                },
            );
        }
        PressKind::Press => {
            queue.push(
                p.delay,
                InputAction::MouseButton {
                    button: p.button,
                    state: ButtonState::Pressed,
                },
            );
        }
        PressKind::Release => {
            queue.push(
                p.delay,
                InputAction::MouseButton {
                    button: p.button,
                    state: ButtonState::Released,
                },
            );
        }
    }
    Ok(queued(&queue))
}

#[derive(Deserialize, Default)]
struct CursorParams {
    position: Option<[f32; 2]>,
    #[serde(default)]
    delta: Option<[f32; 2]>,
    #[serde(default)]
    delay: u32,
}

/// `harness.input.cursor`
pub fn inject_cursor(In(params): In<Option<Value>>, mut queue: ResMut<InputQueue>) -> BrpResult {
    let p: CursorParams = parse_params(params)?;
    if p.position.is_none() && p.delta.is_none() {
        return Err(super::invalid_params("expected `position` and/or `delta`"));
    }
    if let Some(position) = p.position {
        queue.push(p.delay, InputAction::Cursor { position });
    }
    if let Some(delta) = p.delta {
        queue.push(p.delay, InputAction::Motion { delta });
    }
    Ok(queued(&queue))
}

#[derive(Deserialize, Default)]
struct DragParams {
    /// Start; the current cursor position when omitted.
    #[serde(default)]
    from: Option<[f32; 2]>,
    to: Option<[f32; 2]>,
    /// Waypoints the path curves through.
    #[serde(default)]
    via: Vec<[f32; 2]>,
    #[serde(default)]
    frames: Option<u32>,
    /// Button to hold; omit to only move the cursor.
    #[serde(default)]
    button: Option<MouseButton>,
    #[serde(default)]
    ease: Ease,
}

/// `harness.input.drag` – move the cursor along a path over several frames,
/// optionally holding a button. When the game is paused it is stepped for
/// exactly those frames, so the movement plays out in game time (and shows
/// up in a recording) instead of over frames in which the clock stands still.
pub fn inject_drag(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: DragParams = parse_params(params)?;
    let to =
        p.to.ok_or_else(|| super::invalid_params("`to` is required"))?;
    let from = match p.from {
        Some(from) => Vec2::from(from),
        None => {
            let mut windows = world.query::<(&Window, Option<&PrimaryWindow>)>();
            let mut cursor = None;
            for (window, primary) in windows.iter(world) {
                if primary.is_some() || cursor.is_none() {
                    cursor = window.cursor_position().or(cursor);
                }
            }
            cursor.ok_or_else(|| {
                super::invalid_params("the cursor has no position yet; give `from`")
            })?
        }
    };
    let frames = p.frames.unwrap_or(30);
    if !(1..=3600).contains(&frames) {
        return Err(super::invalid_params("`frames` must be between 1 and 3600"));
    }
    let mut drag = Drag::new(from, to.into())
        .frames(frames)
        .button(p.button)
        .ease(p.ease);
    for point in p.via {
        drag = drag.via(point.into());
    }

    let mut queue = world.resource_mut::<InputQueue>();
    for (delay, action) in drag.actions() {
        queue.push(delay, action);
    }
    let mut result = queued(&queue);
    let total = drag.total_frames();
    let paused = world
        .get_resource::<Time<Virtual>>()
        .is_some_and(|t| t.is_paused());
    if paused && let Some(mut stepper) = world.get_resource_mut::<super::step::Stepper>() {
        stepper.paused = true;
        stepper.step(total, None);
        result["stepped_frames"] = json!(total);
    }
    Ok(result)
}

#[derive(Deserialize, Default)]
struct ScrollParams {
    #[serde(default)]
    x: f32,
    #[serde(default)]
    y: f32,
    #[serde(default)]
    unit: ScrollUnit,
    #[serde(default)]
    delay: u32,
}

/// `harness.input.scroll`
pub fn inject_scroll(In(params): In<Option<Value>>, mut queue: ResMut<InputQueue>) -> BrpResult {
    let p: ScrollParams = parse_params(params)?;
    queue.push(
        p.delay,
        InputAction::Scroll {
            x: p.x,
            y: p.y,
            unit: p.unit,
        },
    );
    Ok(queued(&queue))
}

#[derive(Deserialize, Default)]
struct BatchParams {
    /// `[{ "delay": 0, "action": {...} }, ...]` or bare actions delivered in successive frames.
    actions: Vec<TimedAction>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TimedAction {
    Timed { delay: u32, action: InputAction },
    Bare(InputAction),
}

/// `harness.input` – queue several raw actions at once.
pub fn inject_batch(In(params): In<Option<Value>>, mut queue: ResMut<InputQueue>) -> BrpResult {
    let p: BatchParams = parse_params(params)?;
    let mut next = 0;
    for item in p.actions {
        match item {
            TimedAction::Timed { delay, action } => queue.push(delay, action),
            TimedAction::Bare(action) => {
                queue.push(next, action);
                next += 1;
            }
        }
    }
    Ok(queued(&queue))
}
