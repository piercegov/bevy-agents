//! Screenshots over BRP. Capture is asynchronous in Bevy, so the flow is:
//! `harness.screenshot` (returns an id) then `harness.screenshot.result+watch`
//! (streams one result when the image is ready).

use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine;
use bevy::camera::RenderTarget;
use bevy::prelude::*;
use bevy::remote::{BrpError, BrpResult};
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};
use bevy::window::{PrimaryWindow, WindowRef};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{invalid_params, parse_params};

#[derive(Debug)]
pub enum ShotState {
    Pending,
    Done(Value),
    Failed(String),
}

#[derive(Resource, Default, Clone)]
pub struct ScreenshotStore {
    inner: Arc<Mutex<StoreInner>>,
}

#[derive(Default)]
struct StoreInner {
    next_id: u64,
    shots: HashMap<u64, ShotState>,
}

impl ScreenshotStore {
    fn allocate(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.shots.insert(id, ShotState::Pending);
        id
    }
    fn set(&self, id: u64, state: ShotState) {
        self.inner.lock().unwrap().shots.insert(id, state);
    }
    fn take_if_done(&self, id: u64) -> Option<Result<Value, String>> {
        let mut inner = self.inner.lock().unwrap();
        match inner.shots.get(&id) {
            Some(ShotState::Pending) => None,
            Some(ShotState::Done(_)) | Some(ShotState::Failed(_)) => {
                match inner.shots.remove(&id) {
                    Some(ShotState::Done(v)) => Some(Ok(v)),
                    Some(ShotState::Failed(e)) => Some(Err(e)),
                    _ => unreachable!(),
                }
            }
            None => Some(Err(format!("unknown screenshot id {id}"))),
        }
    }
}

pub(crate) fn plugin(app: &mut App) {
    app.init_resource::<ScreenshotStore>();
}

#[derive(Deserialize, Default)]
struct ShotParams {
    /// Where to save the PNG. Relative paths are resolved against the game's
    /// working directory. When omitted the PNG is returned base64-encoded.
    #[serde(default)]
    path: Option<PathBuf>,
    /// Capture whatever this camera renders to (window or image target).
    #[serde(default)]
    camera: Option<u64>,
    /// Capture this window entity.
    #[serde(default)]
    window: Option<u64>,
}

/// Convert a captured `Image` to an RGBA8 `image::RgbaImage`.
pub fn to_rgba(image: Image) -> Result<image::RgbaImage, String> {
    let dynamic = image
        .try_into_dynamic()
        .map_err(|e| format!("unsupported screenshot format: {e}"))?;
    Ok(dynamic.to_rgba8())
}

/// Encode as PNG bytes.
pub fn encode_png(rgba: &image::RgbaImage) -> Result<Vec<u8>, String> {
    let mut buf = Cursor::new(Vec::new());
    rgba.write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(buf.into_inner())
}

/// Pick what to capture: an explicit camera or window, else the headless
/// offscreen image, else the primary window, else any camera rendering to an image.
pub(crate) fn resolve_target(
    world: &mut World,
    camera: Option<u64>,
    window: Option<u64>,
) -> Result<RenderTarget, BrpError> {
    if let Some(bits) = camera {
        let entity =
            Entity::try_from_bits(bits).ok_or_else(|| invalid_params("bad camera entity"))?;
        return Ok(world
            .get::<RenderTarget>(entity)
            .cloned()
            .unwrap_or_else(|| RenderTarget::Window(WindowRef::Primary)));
    }
    if let Some(bits) = window {
        let entity =
            Entity::try_from_bits(bits).ok_or_else(|| invalid_params("bad window entity"))?;
        return Ok(RenderTarget::Window(WindowRef::Entity(entity)));
    }
    if let Some(offscreen) = world.get_resource::<super::headless::OffscreenTarget>() {
        // Headless: the window has no surface; every window camera renders here.
        return Ok(RenderTarget::Image(offscreen.0.clone().into()));
    }
    let has_primary = world
        .query_filtered::<Entity, With<PrimaryWindow>>()
        .iter(world)
        .next()
        .is_some();
    if has_primary {
        return Ok(RenderTarget::Window(WindowRef::Primary));
    }
    // Headless: pick the first active camera with an image target.
    let mut q = world.query::<(&Camera, &RenderTarget)>();
    q.iter(world)
        .filter(|(c, _)| c.is_active)
        .find_map(|(_, t)| matches!(t, RenderTarget::Image(_)).then(|| t.clone()))
        .ok_or_else(|| invalid_params("no primary window and no camera rendering to an image"))
}

/// `harness.screenshot`
pub fn request(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: ShotParams = parse_params(params)?;
    let target = resolve_target(world, p.camera, p.window)?;

    let store = world.resource::<ScreenshotStore>().clone();
    let id = store.allocate();
    let path = p.path.map(|p| {
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
        }
    });

    world
        .spawn(Screenshot(target))
        .observe(move |captured: On<ScreenshotCaptured>| {
            let image = captured.event().image.clone();
            let result = (|| -> Result<Value, String> {
                let rgba = to_rgba(image)?;
                let (width, height) = rgba.dimensions();
                match &path {
                    Some(path) => {
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        rgba.save(path)
                            .map_err(|e| format!("saving {}: {e}", path.display()))?;
                        Ok(json!({ "path": path, "width": width, "height": height }))
                    }
                    None => {
                        let png = encode_png(&rgba)?;
                        Ok(json!({
                            "png_base64": base64::engine::general_purpose::STANDARD.encode(png),
                            "width": width,
                            "height": height,
                        }))
                    }
                }
            })();
            match result {
                Ok(v) => store.set(id, ShotState::Done(v)),
                Err(e) => store.set(id, ShotState::Failed(e)),
            }
        });

    Ok(json!({ "id": id }))
}

#[derive(Deserialize, Default)]
struct ResultParams {
    id: u64,
}

/// `harness.screenshot.result+watch`
pub fn poll_result(
    In(params): In<Option<Value>>,
    store: Res<ScreenshotStore>,
) -> BrpResult<Option<Value>> {
    let p: ResultParams = parse_params(params)?;
    match store.take_if_done(p.id) {
        None => Ok(None),
        Some(Ok(v)) => Ok(Some(v)),
        Some(Err(e)) => Err(BrpError::internal(e)),
    }
}
