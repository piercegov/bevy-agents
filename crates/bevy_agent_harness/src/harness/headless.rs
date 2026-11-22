//! Headless mode for a real game binary: no OS window, no real input, a fixed
//! render size. The game opts in by wrapping its plugin group in
//! [`agent_plugins`]; the mode is switched on per run with the
//! `BEVY_AGENT_HEADLESS` environment variable (`bevy-agent run --headless`).

use std::time::Duration;

use bevy::app::{PluginGroupBuilder, ScheduleRunnerPlugin};
use bevy::camera::{ImageRenderTarget, RenderTarget};
use bevy::prelude::*;
use bevy::render::pipelined_rendering::PipelinedRenderingPlugin;
use bevy::render::render_resource::{TextureFormat, TextureUsages};
use bevy::window::{PrimaryWindow, WindowResolution};
use bevy::winit::WinitPlugin;

/// Set to `1`/`true` to run a game that uses [`agent_plugins`] without a window.
pub const HEADLESS_ENV: &str = "BEVY_AGENT_HEADLESS";
/// Optional `WIDTHxHEIGHT` (logical pixels) for the headless window and render target.
pub const SIZE_ENV: &str = "BEVY_AGENT_SIZE";

/// Whether this process was asked to run headless.
pub fn headless_requested() -> bool {
    std::env::var(HEADLESS_ENV).is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
}

/// Wrap the game's plugin group so it can run headless on request:
///
/// ```no_run
/// # use bevy::prelude::*;
/// # use bevy_agent_harness::{AgentHarnessPlugin, agent_plugins};
/// App::new()
///     .add_plugins(agent_plugins(DefaultPlugins))
///     .add_plugins(AgentHarnessPlugin::default())
///     .run();
/// ```
///
/// Without `BEVY_AGENT_HEADLESS` the group is returned unchanged. With it, the
/// window backend is left out, so no OS window opens and no real mouse or
/// keyboard input reaches the game; [`AgentHarnessPlugin`](crate::AgentHarnessPlugin)
/// then drives the app at 60 updates per second and renders every camera into
/// an offscreen image that `harness.screenshot` captures.
pub fn agent_plugins(group: impl PluginGroup) -> PluginGroupBuilder {
    let mut builder = group.build();
    if headless_requested() {
        if builder.contains::<WinitPlugin>() {
            builder = builder.disable::<WinitPlugin>();
        }
        if builder.contains::<PipelinedRenderingPlugin>() {
            builder = builder.disable::<PipelinedRenderingPlugin>();
        }
    }
    builder
}

/// Present while the app runs headless.
#[derive(Resource, Clone, Copy, Debug)]
pub struct Headless;

/// The image every window camera is redirected to when there is no real window.
#[derive(Resource, Clone)]
pub(crate) struct OffscreenTarget(pub(crate) Handle<Image>);

/// Create the offscreen image and redirect window cameras to it each frame.
pub(crate) fn install_offscreen_target(app: &mut App, width: u32, height: u32) -> Handle<Image> {
    let mut image = Image::new_target_texture(
        width.max(1),
        height.max(1),
        TextureFormat::Rgba8UnormSrgb,
        None,
    );
    image.texture_descriptor.usage |= TextureUsages::COPY_SRC;
    let handle = app.world_mut().resource_mut::<Assets<Image>>().add(image);
    app.insert_resource(OffscreenTarget(handle.clone()));
    app.add_systems(
        PostUpdate,
        retarget_cameras.before(bevy::camera::CameraUpdateSystems),
    );
    app.add_systems(First, mark_default_ui_camera);
    // Only when picking is part of the app (it is with `DefaultPlugins`).
    if app
        .world()
        .contains_resource::<Messages<bevy::picking::pointer::PointerInput>>()
    {
        app.add_systems(
            First,
            retarget_pointers.after(bevy::picking::PickingSystems::Input),
        );
    }
    handle
}

/// Pointer input names the window it happened in, but the cameras now render
/// to the offscreen image, so picking would never match a pointer to a camera.
/// Point window pointer input at the image instead, keeping the position.
fn retarget_pointers(
    target: Res<OffscreenTarget>,
    mut inputs: MessageMutator<bevy::picking::pointer::PointerInput>,
) {
    let Some(image) =
        RenderTarget::Image(ImageRenderTarget::from(target.0.clone())).normalize(None)
    else {
        return;
    };
    for input in inputs.read() {
        if matches!(
            input.location.target,
            bevy::camera::NormalizedRenderTarget::Window(_)
        ) {
            input.location.target = image.clone();
        }
    }
}

/// A camera that targeted a window before it was redirected offscreen.
#[derive(Component)]
struct WindowCamera;

/// The camera this module (not the game) marked as the default UI camera.
#[derive(Component)]
struct HarnessUiCamera;

fn retarget_cameras(
    target: Res<OffscreenTarget>,
    mut cameras: Query<(Entity, Option<&mut RenderTarget>), With<Camera>>,
    mut commands: Commands,
) {
    for (entity, rt) in cameras.iter_mut() {
        match rt {
            Some(mut rt) => {
                if matches!(*rt, RenderTarget::Window(_)) {
                    *rt = RenderTarget::Image(ImageRenderTarget::from(target.0.clone()));
                    commands.entity(entity).insert(WindowCamera);
                }
            }
            None => {
                commands.entity(entity).insert((
                    RenderTarget::Image(ImageRenderTarget::from(target.0.clone())),
                    WindowCamera,
                ));
            }
        }
    }
}

/// bevy_ui draws nodes without an explicit camera on "the highest-order camera
/// targeting the primary window". After redirection no camera targets a window,
/// so UI would silently disappear. Mark the camera Bevy would have chosen with
/// `IsDefaultUiCamera`, unless the game marks one itself. Done through
/// reflection so this crate does not depend on bevy_ui.
fn mark_default_ui_camera(world: &mut World) {
    use bevy::ecs::reflect::ReflectComponent;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let Some(registration) = registry.get_with_type_path("bevy_ui::ui_node::IsDefaultUiCamera")
    else {
        return;
    };
    let (Some(reflect), Some(default)) = (
        registration.data::<ReflectComponent>(),
        registration.data::<ReflectDefault>(),
    ) else {
        return;
    };
    let marker = world.components().get_id(registration.type_id());
    insert_default_ui_camera(world, reflect, default, &registry, marker);
}

fn insert_default_ui_camera(
    world: &mut World,
    reflect: &bevy::ecs::reflect::ReflectComponent,
    default: &ReflectDefault,
    registry: &bevy::reflect::TypeRegistry,
    marker: Option<bevy::ecs::component::ComponentId>,
) {
    let mut cameras = world.query::<(Entity, &Camera, Has<WindowCamera>, Has<HarnessUiCamera>)>();
    let all: Vec<(Entity, isize, bool, bool)> = cameras
        .iter(world)
        .map(|(e, c, window, ours)| (e, c.order, window, ours))
        .collect();
    let game_marked = marker.is_some_and(|marker| {
        all.iter()
            .any(|(e, _, _, ours)| !ours && world.entity(*e).contains_id(marker))
    });
    let wanted = if game_marked {
        None
    } else {
        all.iter()
            .filter(|(_, _, window, _)| *window)
            .max_by_key(|(e, order, _, _)| (*order, *e))
            .map(|(e, ..)| *e)
    };
    for (entity, _, _, ours) in all {
        let should = wanted == Some(entity);
        if ours && !should {
            let mut e = world.entity_mut(entity);
            reflect.remove(&mut e);
            e.remove::<HarnessUiCamera>();
        } else if should && !ours {
            let mut e = world.entity_mut(entity);
            reflect.insert(&mut e, default.default().as_partial_reflect(), registry);
            e.insert(HarnessUiCamera);
        }
    }
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// Called by `AgentHarnessPlugin::build`. Returns whether headless mode is active.
pub(crate) fn setup(app: &mut App) -> bool {
    if !headless_requested() {
        return false;
    }
    if app.is_plugin_added::<WinitPlugin>() {
        warn!(
            "{HEADLESS_ENV} is set but the window backend is active, so the game runs in a normal \
             window. Wrap your plugin group: `bevy_agent_harness::agent_plugins(DefaultPlugins)`."
        );
        return false;
    }

    let requested = std::env::var(SIZE_ENV).ok().and_then(|s| {
        let size = parse_size(&s);
        if size.is_none() {
            warn!("ignoring {SIZE_ENV}={s:?}: expected WIDTHxHEIGHT, e.g. 1280x720");
        }
        size
    });
    let world = app.world_mut();
    let mut windows = world.query_filtered::<&mut Window, With<PrimaryWindow>>();
    let mut size = requested.unwrap_or((1280, 720));
    if let Some(mut window) = windows.iter_mut(world).next() {
        match requested {
            Some((w, h)) => window.resolution = WindowResolution::new(w, h),
            None => size = (window.physical_width(), window.physical_height()),
        }
    }

    // Without winit nothing drives the app; run it at a steady 60 updates per second.
    if !app.is_plugin_added::<ScheduleRunnerPlugin>() {
        app.add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    }
    if app.world().contains_resource::<Assets<Image>>() {
        install_offscreen_target(app, size.0, size.1);
    }
    app.insert_resource(Headless);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("640x360"), Some((640, 360)));
        assert_eq!(parse_size(" 800X600 "), Some((800, 600)));
        assert_eq!(parse_size("0x10"), None);
        assert_eq!(parse_size("wide"), None);
    }
}
