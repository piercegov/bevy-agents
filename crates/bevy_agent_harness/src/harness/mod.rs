//! The game-side plugin. See [`AgentHarnessPlugin`].

pub mod input;
pub mod logs;
mod observe;
pub mod screenshot;
pub mod step;

use std::net::IpAddr;

use bevy::camera::RenderTarget;
use bevy::diagnostic::{DiagnosticsStore, FrameCount, FrameTimeDiagnosticsPlugin};
use bevy::prelude::*;
use bevy::remote::http::{DEFAULT_ADDR, DEFAULT_PORT, RemoteHttpPlugin};
use bevy::remote::{BrpError, BrpResult, RemotePlugin};
use bevy::window::PrimaryWindow;
use serde_json::{Value, json};

/// Adds the Bevy Remote Protocol plus the `harness.*` methods to an app.
///
/// Registered methods (all take/return JSON):
///
/// | method | params | result |
/// |---|---|---|
/// | `harness.status` | – | frame, time, pause state, entity count, windows, cameras |
/// | `harness.input` | `{ "actions": [Action, ...] }` | `{ "queued": n }` |
/// | `harness.input.key` | `{ "key": "Space", "state": "tap"\|"press"\|"release", "hold_frames"?: 1 }` | `{ "queued": n }` |
/// | `harness.input.mouse` | `{ "button": "Left", "state": "tap"\|"press"\|"release", "position"?: [x, y] }` | `{ "queued": n }` |
/// | `harness.input.cursor` | `{ "position": [x, y] }` | `{ "queued": n }` |
/// | `harness.input.scroll` | `{ "x"?: 0, "y": 1, "unit"?: "line"\|"pixel" }` | `{ "queued": n }` |
/// | `harness.logs` | `{ "since"?: seq, "level"?: "warn", "limit"?: 200, "contains"?: "..", "target"?: ".." }` | `{ "entries": [...], "next": seq, ... }` |
/// | `harness.logs.clear` | – | null |
/// | `harness.pause` / `harness.resume` | – | status |
/// | `harness.step` | `{ "frames"?: 1, "dt"?: 0.016 }` | status incl. `until_frame` |
/// | `harness.time` | `{ "speed"?: 1.0, "paused"?: bool }` | status |
/// | `harness.wait_frame+watch` | `{ "until_frame": n }` | status once the frame is reached |
/// | `harness.screenshot` | `{ "path"?: "abs.png", "camera"?: entity, "window"?: entity }` | `{ "id": n }` |
/// | `harness.screenshot.result+watch` | `{ "id": n }` | `{ "path" }` or `{ "png_base64", "width", "height" }` |
pub struct AgentHarnessPlugin {
    /// Address the BRP HTTP server binds to. Default `127.0.0.1`.
    pub address: IpAddr,
    /// Port the BRP HTTP server listens on. Default `15702`.
    pub port: u16,
    /// Set to `false` if you add your own transport (e.g. a custom `RemoteHttpPlugin`).
    pub http: bool,
    /// Maximum number of log entries kept in memory for `harness.logs`.
    pub log_capacity: usize,
}

impl Default for AgentHarnessPlugin {
    fn default() -> Self {
        Self {
            address: DEFAULT_ADDR,
            port: DEFAULT_PORT,
            http: true,
            log_capacity: 10_000,
        }
    }
}

impl AgentHarnessPlugin {
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
    pub fn with_address(mut self, address: impl Into<IpAddr>) -> Self {
        self.address = address.into();
        self
    }
    pub fn without_http(mut self) -> Self {
        self.http = false;
        self
    }
}

impl Plugin for AgentHarnessPlugin {
    fn build(&self, app: &mut App) {
        if app.is_plugin_added::<RemotePlugin>() {
            warn!(
                "AgentHarnessPlugin: RemotePlugin was already added; harness.* methods will not be \
                 registered. Remove your own RemotePlugin/RemoteHttpPlugin and let the harness add them."
            );
        } else {
            let remote = RemotePlugin::default()
                .with_watching_method_main("world.observe+watch", observe::watch)
                .with_method_main("harness.status", status)
                .with_method_main("harness.input", input::inject_batch)
                .with_method_main("harness.input.key", input::inject_key)
                .with_method_main("harness.input.mouse", input::inject_mouse)
                .with_method_main("harness.input.cursor", input::inject_cursor)
                .with_method_main("harness.input.scroll", input::inject_scroll)
                .with_method_main("harness.logs", logs::get_logs)
                .with_method_main("harness.logs.clear", logs::clear_logs)
                .with_method_main("harness.pause", step::pause)
                .with_method_main("harness.resume", step::resume)
                .with_method_main("harness.step", step::step)
                .with_method_main("harness.time", step::time)
                .with_watching_method_main("harness.wait_frame+watch", step::wait_frame)
                .with_method_main("harness.screenshot", screenshot::request)
                .with_watching_method_main(
                    "harness.screenshot.result+watch",
                    screenshot::poll_result,
                );
            app.add_plugins(remote);
            if self.http {
                app.add_plugins(
                    RemoteHttpPlugin::default()
                        .with_address(self.address)
                        .with_port(self.port),
                );
            }
        }

        if !app.is_plugin_added::<FrameTimeDiagnosticsPlugin>() {
            app.add_plugins(FrameTimeDiagnosticsPlugin::default());
        }

        logs::ensure_buffer(app, self.log_capacity);
        app.add_plugins((input::plugin, step::plugin, screenshot::plugin));

        info!(
            "bevy_agent_harness: BRP listening on http://{}:{}",
            self.address, self.port
        );
    }
}

/// Helper: parse `params` into `T`, mapping failures to a BRP invalid-params error.
pub(crate) fn parse_params<T: serde::de::DeserializeOwned + Default>(
    params: Option<Value>,
) -> BrpResult<T> {
    match params {
        None | Some(Value::Null) => Ok(T::default()),
        Some(v) => serde_json::from_value(v).map_err(|e| BrpError {
            code: bevy::remote::error_codes::INVALID_PARAMS,
            message: format!("invalid params: {e}"),
            data: None,
        }),
    }
}

pub(crate) fn invalid_params(msg: impl Into<String>) -> BrpError {
    BrpError {
        code: bevy::remote::error_codes::INVALID_PARAMS,
        message: msg.into(),
        data: None,
    }
}

/// `harness.status`
pub fn status(In(_): In<Option<Value>>, world: &mut World) -> BrpResult {
    Ok(status_value(world))
}

pub(crate) fn status_value(world: &mut World) -> Value {
    let frame = world
        .get_resource::<FrameCount>()
        .map(|f| f.0)
        .unwrap_or_default();
    let virt = world.get_resource::<Time<Virtual>>().copied();
    let real = world.get_resource::<Time<Real>>().copied();
    let stepping = world
        .get_resource::<step::Stepper>()
        .map(|s| s.remaining)
        .unwrap_or(0);
    let fps = world
        .get_resource::<DiagnosticsStore>()
        .and_then(|d| d.get(&FrameTimeDiagnosticsPlugin::FPS))
        .and_then(|d| d.smoothed());

    let mut windows = Vec::new();
    let mut q = world.query::<(Entity, &Window, Option<&PrimaryWindow>)>();
    for (e, w, primary) in q.iter(world) {
        windows.push(json!({
            "entity": e.to_bits(),
            "primary": primary.is_some(),
            "width": w.width(),
            "height": w.height(),
            "physical_width": w.physical_width(),
            "physical_height": w.physical_height(),
            "scale_factor": w.scale_factor(),
            "cursor": w.cursor_position().map(|p| [p.x, p.y]),
            "focused": w.focused,
        }));
    }

    let mut cameras = Vec::new();
    let mut q = world.query::<(Entity, &Camera, Option<&RenderTarget>, Option<&Name>)>();
    for (e, c, target, name) in q.iter(world) {
        cameras.push(json!({
            "entity": e.to_bits(),
            "name": name.map(|n| n.to_string()),
            "active": c.is_active,
            "order": c.order,
            "target": target.map(|t| format!("{t:?}")),
            "size": c.physical_target_size().map(|s| [s.x, s.y]),
        }));
    }

    json!({
        "frame": frame,
        "fps": fps,
        "elapsed": virt.map(|t| t.elapsed_secs_f64()),
        "real_elapsed": real.map(|t| t.elapsed_secs_f64()),
        "delta": virt.map(|t| t.delta_secs_f64()),
        "paused": virt.map(|t| t.is_paused()).unwrap_or(false),
        "speed": virt.map(|t| t.relative_speed_f64()),
        "stepping": stepping,
        "entities": world.entities().len(),
        "windows": windows,
        "cameras": cameras,
    })
}
