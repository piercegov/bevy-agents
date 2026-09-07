//! In-process integration testing for Bevy games.
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_agent_harness::testing::TestApp;
//!
//! #[derive(Component, Debug)]
//! struct Health(f32);
//!
//! fn my_game(app: &mut App) { /* add your plugins/systems */ }
//!
//! let mut app = TestApp::new();          // headless, no GPU
//! app.add_plugins(my_game);
//! app.advance(1);                        // run Startup
//! let player = app.single::<Health>();
//! app.tap(KeyCode::Space);               // press + release, two frames
//! app.advance(10);
//! app.assert_component::<Health>(player, |h| h.0 < 100.0);
//! ```
//!
//! With [`TestApp::with_rendering`] the app also renders to an offscreen
//! texture and can take screenshots / compare against golden images.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::app::PluginsState;
use bevy::diagnostic::FrameCount;
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy::window::{ExitCondition, PrimaryWindow, WindowResolution};

use crate::harness::input::{InputAction, InputQueue};
use crate::harness::logs::{LogBuffer, LogEntry, global_buffer};

/// Builder for [`TestApp`].
pub struct TestAppBuilder {
    width: u32,
    height: u32,
    dt: Duration,
    render: bool,
    golden_dir: Option<PathBuf>,
    log_filter: String,
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        Self {
            width: 640,
            height: 360,
            dt: Duration::from_micros(16_667),
            render: false,
            golden_dir: None,
            log_filter: "warn,bevy_agent_harness=info".into(),
        }
    }
}

impl TestAppBuilder {
    /// Logical window size reported to the game (and the offscreen render size).
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }
    /// Fixed virtual time per frame. Default 1/60 s.
    pub fn dt(mut self, dt: Duration) -> Self {
        self.dt = dt;
        self
    }
    /// Enable a real (headless) GPU renderer so screenshots work.
    pub fn render(mut self, render: bool) -> Self {
        self.render = render;
        self
    }
    /// Where golden images live. Default `$CARGO_MANIFEST_DIR/tests/golden`.
    pub fn golden_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.golden_dir = Some(dir.into());
        self
    }
    /// `RUST_LOG`-style filter for the captured log. Default `warn`.
    pub fn log_filter(mut self, filter: impl Into<String>) -> Self {
        self.log_filter = filter.into();
        self
    }

    pub fn build(self) -> TestApp {
        let mut app = App::new();
        let window = Window {
            resolution: WindowResolution::new(self.width, self.height),
            title: "bevy_agent_harness TestApp".into(),
            ..default()
        };
        let log_plugin = bevy::log::LogPlugin {
            filter: self.log_filter.clone(),
            level: bevy::log::Level::TRACE,
            custom_layer: crate::harness::logs::log_layer,
            ..default()
        };

        let mut render = None;
        if self.render {
            use bevy::render::RenderPlugin;
            use bevy::render::pipelined_rendering::PipelinedRenderingPlugin;
            use bevy::winit::WinitPlugin;

            app.add_plugins(
                DefaultPlugins
                    .set(WindowPlugin {
                        primary_window: Some(window),
                        exit_condition: ExitCondition::DontExit,
                        ..default()
                    })
                    .set(log_plugin)
                    .set(RenderPlugin {
                        synchronous_pipeline_compilation: true,
                        ..default()
                    })
                    .set(ImagePlugin::default_nearest())
                    .disable::<WinitPlugin>()
                    .disable::<PipelinedRenderingPlugin>(),
            );

            use bevy::render::render_resource::{TextureFormat, TextureUsages};
            let mut image = Image::new_target_texture(
                self.width,
                self.height,
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
            render = Some(RenderState { target: handle });
        } else {
            app.add_plugins((
                bevy::app::TaskPoolPlugin::default(),
                bevy::diagnostic::FrameCountPlugin,
                bevy::time::TimePlugin,
                log_plugin,
                bevy::transform::TransformPlugin,
                bevy::input::InputPlugin,
                WindowPlugin {
                    primary_window: Some(window),
                    exit_condition: ExitCondition::DontExit,
                    ..default()
                },
                bevy::asset::AssetPlugin::default(),
                bevy::state::app::StatesPlugin,
            ));
        }

        app.insert_resource(TimeUpdateStrategy::ManualDuration(self.dt));
        app.add_plugins(crate::harness::input::plugin);
        if !app.world().contains_resource::<LogBuffer>() {
            app.insert_resource(global_buffer().clone());
        }

        let golden_dir = self.golden_dir.unwrap_or_else(|| {
            std::env::var_os("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("tests/golden")
        });

        TestApp {
            app,
            dt: self.dt,
            started: false,
            render,
            log_start: global_buffer().next_seq(),
            golden_dir,
        }
    }
}

#[derive(Resource, Clone)]
struct OffscreenTarget(Handle<Image>);

struct RenderState {
    target: Handle<Image>,
}

fn retarget_cameras(
    target: Res<OffscreenTarget>,
    mut cameras: Query<(Entity, Option<&mut bevy::camera::RenderTarget>), With<Camera>>,
    mut commands: Commands,
) {
    use bevy::camera::{ImageRenderTarget, RenderTarget};
    for (entity, rt) in cameras.iter_mut() {
        match rt {
            Some(mut rt) => {
                if matches!(*rt, RenderTarget::Window(_)) {
                    *rt = RenderTarget::Image(ImageRenderTarget::from(target.0.clone()));
                }
            }
            None => {
                commands
                    .entity(entity)
                    .insert(RenderTarget::Image(ImageRenderTarget::from(
                        target.0.clone(),
                    )));
            }
        }
    }
}

/// A Bevy [`App`] driven frame-by-frame from a test, with input injection,
/// component assertions, captured logs and (optionally) screenshots.
///
/// Every input helper (`press`, `tap`, `click`, ...) advances the app so that
/// the input has been processed when it returns; use [`TestApp::queue`] for
/// raw control.
pub struct TestApp {
    app: App,
    dt: Duration,
    started: bool,
    render: Option<RenderState>,
    log_start: u64,
    golden_dir: PathBuf,
}

impl Default for TestApp {
    fn default() -> Self {
        Self::new()
    }
}

impl TestApp {
    /// Headless app without a GPU: time, input, transforms, assets, states, logs.
    pub fn new() -> Self {
        Self::builder().build()
    }

    pub fn builder() -> TestAppBuilder {
        TestAppBuilder::default()
    }

    /// Full `DefaultPlugins` (minus winit) rendering into an offscreen texture.
    pub fn with_rendering(width: u32, height: u32) -> Self {
        Self::builder().render(true).size(width, height).build()
    }

    // ---- app access --------------------------------------------------------

    pub fn app(&self) -> &App {
        &self.app
    }
    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }
    pub fn world(&self) -> &World {
        self.app.world()
    }
    pub fn world_mut(&mut self) -> &mut World {
        self.app.world_mut()
    }

    /// Add game plugins/systems. Must be called before the first `advance`.
    pub fn add_plugins<M>(&mut self, plugins: impl bevy::app::Plugins<M>) -> &mut Self {
        assert!(
            !self.started,
            "add_plugins must be called before the first advance()"
        );
        self.app.add_plugins(plugins);
        self
    }

    pub fn insert_resource<R: Resource>(&mut self, resource: R) -> &mut Self {
        self.app.insert_resource(resource);
        self
    }

    pub fn spawn(&mut self, bundle: impl Bundle) -> Entity {
        self.app.world_mut().spawn(bundle).id()
    }

    // ---- time --------------------------------------------------------------

    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        while self.app.plugins_state() == PluginsState::Adding {
            bevy::tasks::tick_global_task_pools_on_main_thread();
            std::thread::yield_now();
        }
        self.app.finish();
        self.app.cleanup();
        self.started = true;
    }

    /// Run `frames` full frames (`app.update()`), each advancing virtual time by `dt`.
    pub fn advance(&mut self, frames: u32) -> &mut Self {
        self.ensure_started();
        for _ in 0..frames {
            self.app.update();
        }
        self
    }

    /// Advance by roughly `secs` of virtual time (rounded up to whole frames).
    pub fn advance_secs(&mut self, secs: f32) -> &mut Self {
        let frames = (secs as f64 / self.dt.as_secs_f64()).ceil().max(1.0) as u32;
        self.advance(frames)
    }

    /// Keep advancing until `pred` is true or `max_frames` elapse.
    pub fn run_until(
        &mut self,
        max_frames: u32,
        mut pred: impl FnMut(&mut World) -> bool,
    ) -> Result<u32, String> {
        self.ensure_started();
        for i in 0..max_frames {
            if pred(self.app.world_mut()) {
                return Ok(i);
            }
            self.app.update();
        }
        if pred(self.app.world_mut()) {
            Ok(max_frames)
        } else {
            Err(format!("condition not met after {max_frames} frames"))
        }
    }

    pub fn frame(&self) -> u32 {
        self.app
            .world()
            .get_resource::<FrameCount>()
            .map(|f| f.0)
            .unwrap_or(0)
    }

    pub fn dt(&self) -> Duration {
        self.dt
    }

    // ---- input -------------------------------------------------------------

    /// Raw access to the input queue (actions fire at the start of coming frames).
    pub fn queue(&mut self) -> Mut<'_, InputQueue> {
        self.app.world_mut().resource_mut::<InputQueue>()
    }

    /// Press and hold a key. Advances one frame (`just_pressed` fires in it).
    pub fn press(&mut self, key: KeyCode) -> &mut Self {
        self.queue().push(
            0,
            InputAction::Key {
                key,
                state: ButtonState::Pressed,
                logical: None,
            },
        );
        self.advance(1)
    }

    /// Release a held key. Advances one frame.
    pub fn release(&mut self, key: KeyCode) -> &mut Self {
        self.queue().push(
            0,
            InputAction::Key {
                key,
                state: ButtonState::Released,
                logical: None,
            },
        );
        self.advance(1)
    }

    /// Press then release. Advances two frames.
    pub fn tap(&mut self, key: KeyCode) -> &mut Self {
        self.press(key).release(key)
    }

    /// Hold a key for `frames` frames, then release.
    pub fn hold(&mut self, key: KeyCode, frames: u32) -> &mut Self {
        self.press(key);
        if frames > 1 {
            self.advance(frames - 1);
        }
        self.release(key)
    }

    /// Move the cursor (logical pixels, origin top-left). Advances one frame.
    pub fn move_cursor(&mut self, position: Vec2) -> &mut Self {
        self.queue().push(
            0,
            InputAction::Cursor {
                position: position.into(),
            },
        );
        self.advance(1)
    }

    /// Relative mouse motion. Advances one frame.
    pub fn mouse_motion(&mut self, delta: Vec2) -> &mut Self {
        self.queue().push(
            0,
            InputAction::Motion {
                delta: delta.into(),
            },
        );
        self.advance(1)
    }

    pub fn mouse_press(&mut self, button: MouseButton) -> &mut Self {
        self.queue().push(
            0,
            InputAction::MouseButton {
                button,
                state: ButtonState::Pressed,
            },
        );
        self.advance(1)
    }

    pub fn mouse_release(&mut self, button: MouseButton) -> &mut Self {
        self.queue().push(
            0,
            InputAction::MouseButton {
                button,
                state: ButtonState::Released,
            },
        );
        self.advance(1)
    }

    /// Move the cursor and click. Advances three frames.
    pub fn click(&mut self, position: Vec2) -> &mut Self {
        self.click_with(position, MouseButton::Left)
    }

    pub fn click_with(&mut self, position: Vec2, button: MouseButton) -> &mut Self {
        self.move_cursor(position)
            .mouse_press(button)
            .mouse_release(button)
    }

    pub fn scroll(&mut self, y: f32) -> &mut Self {
        self.queue().push(
            0,
            InputAction::Scroll {
                x: 0.0,
                y,
                unit: crate::harness::input::ScrollUnit::Line,
            },
        );
        self.advance(1)
    }

    // ---- inspection --------------------------------------------------------

    pub fn get<C: Component>(&self, entity: Entity) -> Option<&C> {
        self.app.world().get::<C>(entity)
    }

    /// Panics if the entity lacks the component.
    pub fn component<C: Component>(&self, entity: Entity) -> &C {
        self.get::<C>(entity).unwrap_or_else(|| {
            panic!(
                "entity {entity} has no {} (components: {})",
                std::any::type_name::<C>(),
                self.component_names(entity).join(", ")
            )
        })
    }

    /// Names of all components on an entity.
    pub fn component_names(&self, entity: Entity) -> Vec<String> {
        let world = self.app.world();
        let Ok(e) = world.get_entity(entity) else {
            return vec!["<despawned>".into()];
        };
        e.archetype()
            .components()
            .iter()
            .filter_map(|id| world.components().get_info(*id))
            .map(|info| crate::client_free_short_name(info.name().as_ref()))
            .collect()
    }

    pub fn has<C: Component>(&self, entity: Entity) -> bool {
        self.get::<C>(entity).is_some()
    }

    pub fn is_alive(&self, entity: Entity) -> bool {
        self.app.world().get_entity(entity).is_ok()
    }

    /// Assert a predicate on a component, with a helpful panic message.
    #[track_caller]
    pub fn assert_component<C: Component + std::fmt::Debug>(
        &self,
        entity: Entity,
        pred: impl FnOnce(&C) -> bool,
    ) -> &Self {
        let c = self.component::<C>(entity);
        assert!(
            pred(c),
            "assertion on {} of entity {entity} failed; value: {c:?}",
            std::any::type_name::<C>()
        );
        self
    }

    /// Like [`assert_component`](Self::assert_component) but for components without `Debug`.
    #[track_caller]
    pub fn assert_component_with<C: Component>(
        &self,
        entity: Entity,
        pred: impl FnOnce(&C) -> bool,
        message: &str,
    ) -> &Self {
        let c = self.component::<C>(entity);
        assert!(
            pred(c),
            "{message} ({} of entity {entity})",
            std::any::type_name::<C>()
        );
        self
    }

    pub fn resource<R: Resource>(&self) -> &R {
        self.app.world().resource::<R>()
    }

    pub fn resource_mut<R: Resource<Mutability = bevy::ecs::component::Mutable>>(
        &mut self,
    ) -> Mut<'_, R> {
        self.app.world_mut().resource_mut::<R>()
    }

    /// All entities that have `C`.
    pub fn entities_with<C: Component>(&mut self) -> Vec<Entity> {
        let world = self.app.world_mut();
        world
            .query_filtered::<Entity, With<C>>()
            .iter(world)
            .collect()
    }

    /// The single entity with `C`; panics if there are zero or several.
    #[track_caller]
    pub fn single<C: Component>(&mut self) -> Entity {
        let all = self.entities_with::<C>();
        match all.as_slice() {
            [e] => *e,
            [] => panic!("no entity has {}", std::any::type_name::<C>()),
            many => panic!(
                "{} entities have {}",
                many.len(),
                std::any::type_name::<C>()
            ),
        }
    }

    pub fn find_by_name(&mut self, name: &str) -> Option<Entity> {
        let world = self.app.world_mut();
        world
            .query::<(Entity, &Name)>()
            .iter(world)
            .find(|(_, n)| n.as_str() == name)
            .map(|(e, _)| e)
    }

    pub fn primary_window(&mut self) -> Option<Entity> {
        let world = self.app.world_mut();
        world
            .query_filtered::<Entity, With<PrimaryWindow>>()
            .iter(world)
            .next()
    }

    // ---- logs --------------------------------------------------------------

    /// Log entries emitted since this `TestApp` was created (process-global buffer).
    pub fn logs(&self) -> Vec<LogEntry> {
        global_buffer().since(self.log_start)
    }

    /// Entries at `WARN` or `ERROR`.
    pub fn log_problems(&self) -> Vec<LogEntry> {
        self.logs()
            .into_iter()
            .filter(|e| e.level == "ERROR" || e.level == "WARN")
            .collect()
    }

    #[track_caller]
    pub fn assert_no_log_errors(&self) -> &Self {
        let errors: Vec<_> = self
            .logs()
            .into_iter()
            .filter(|e| e.level == "ERROR")
            .collect();
        assert!(
            errors.is_empty(),
            "{} error log(s):\n{}",
            errors.len(),
            errors
                .iter()
                .map(|e| format!("  [{}] {}", e.target, e.message))
                .collect::<Vec<_>>()
                .join("\n")
        );
        self
    }

    // ---- screenshots -------------------------------------------------------

    /// Render target handle when rendering is enabled.
    pub fn render_target(&self) -> Option<Handle<Image>> {
        self.render.as_ref().map(|r| r.target.clone())
    }

    /// Capture the offscreen target. Requires [`TestApp::with_rendering`].
    /// Advances a few frames while the GPU readback completes.
    pub fn screenshot(&mut self) -> image::RgbaImage {
        let target = self
            .render
            .as_ref()
            .map(|r| r.target.clone())
            .expect("screenshot() requires TestApp::with_rendering / builder().render(true)");
        self.ensure_started();
        let slot: Arc<Mutex<Option<Image>>> = Arc::new(Mutex::new(None));
        let sink = slot.clone();
        self.app
            .world_mut()
            .spawn(bevy::render::view::screenshot::Screenshot::image(target))
            .observe(
                move |captured: On<bevy::render::view::screenshot::ScreenshotCaptured>| {
                    *sink.lock().unwrap() = Some(captured.event().image.clone());
                },
            );
        for _ in 0..60 {
            self.app.update();
            if let Some(img) = slot.lock().unwrap().take() {
                return crate::harness::screenshot::to_rgba(img)
                    .expect("screenshot format conversion");
            }
        }
        panic!("screenshot was not captured within 60 frames");
    }

    pub fn save_screenshot(&mut self, path: impl AsRef<Path>) -> image::RgbaImage {
        let img = self.screenshot();
        if let Some(parent) = path.as_ref().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        img.save(path.as_ref()).expect("saving screenshot");
        img
    }

    /// Compare a screenshot to `tests/golden/<name>.png`.
    ///
    /// Set `UPDATE_GOLDEN=1` to (re)write the golden file. On mismatch the
    /// actual image and a diff are written to `tests/golden/__failures__/`.
    #[track_caller]
    pub fn assert_golden(&mut self, name: &str) -> &mut Self {
        self.assert_golden_with(name, GoldenOptions::default())
    }

    #[track_caller]
    pub fn assert_golden_with(&mut self, name: &str, opts: GoldenOptions) -> &mut Self {
        let actual = self.screenshot();
        let golden_path = self.golden_dir.join(format!("{name}.png"));
        let update = std::env::var("UPDATE_GOLDEN")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);

        if update || !golden_path.exists() {
            std::fs::create_dir_all(&self.golden_dir).ok();
            actual.save(&golden_path).expect("writing golden image");
            if update {
                eprintln!("updated golden {}", golden_path.display());
                return self;
            }
            panic!(
                "golden image {} did not exist; it has been created from the current render. \
                 Inspect it and re-run.",
                golden_path.display()
            );
        }

        let expected = image::open(&golden_path)
            .unwrap_or_else(|e| panic!("reading golden {}: {e}", golden_path.display()))
            .to_rgba8();
        let report = compare_images(&expected, &actual, &opts);
        if report.passes(&opts) {
            return self;
        }

        let fail_dir = self.golden_dir.join("__failures__");
        std::fs::create_dir_all(&fail_dir).ok();
        let actual_path = fail_dir.join(format!("{name}.actual.png"));
        let diff_path = fail_dir.join(format!("{name}.diff.png"));
        actual.save(&actual_path).ok();
        report.diff_image.save(&diff_path).ok();
        panic!(
            "screenshot `{name}` differs from golden: {} of {} pixels differ ({:.3}%), max channel delta {} \
             (allowed {:.3}% / {}). Wrote {} and {}. Run with UPDATE_GOLDEN=1 to accept.",
            report.differing,
            report.total,
            report.fraction() * 100.0,
            report.max_delta,
            opts.max_diff_fraction * 100.0,
            opts.channel_tolerance,
            actual_path.display(),
            diff_path.display()
        );
    }
}

/// Tolerances for golden comparisons.
#[derive(Debug, Clone, Copy)]
pub struct GoldenOptions {
    /// Fraction of pixels allowed to differ (0.0..=1.0). Default 0.001.
    pub max_diff_fraction: f64,
    /// Per-channel difference below which a pixel counts as equal. Default 8.
    pub channel_tolerance: u8,
}

impl Default for GoldenOptions {
    fn default() -> Self {
        Self {
            max_diff_fraction: 0.001,
            channel_tolerance: 8,
        }
    }
}

/// Result of [`compare_images`].
pub struct DiffReport {
    pub differing: u64,
    pub total: u64,
    pub max_delta: u8,
    pub size_mismatch: bool,
    /// Differing pixels in red over a dimmed copy of the expected image.
    pub diff_image: image::RgbaImage,
}

impl DiffReport {
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.differing as f64 / self.total as f64
        }
    }
    pub fn passes(&self, opts: &GoldenOptions) -> bool {
        !self.size_mismatch && self.fraction() <= opts.max_diff_fraction
    }
}

/// Pixel-wise comparison of two RGBA images.
pub fn compare_images(
    expected: &image::RgbaImage,
    actual: &image::RgbaImage,
    opts: &GoldenOptions,
) -> DiffReport {
    let (w, h) = expected.dimensions();
    if actual.dimensions() != (w, h) {
        return DiffReport {
            differing: (w as u64) * (h as u64),
            total: (w as u64) * (h as u64),
            max_delta: 255,
            size_mismatch: true,
            diff_image: actual.clone(),
        };
    }
    let mut diff = image::RgbaImage::new(w, h);
    let mut differing = 0u64;
    let mut max_delta = 0u8;
    for (x, y, e) in expected.enumerate_pixels() {
        let a = actual.get_pixel(x, y);
        let delta = (0..3).map(|i| e.0[i].abs_diff(a.0[i])).max().unwrap_or(0);
        max_delta = max_delta.max(delta);
        if delta > opts.channel_tolerance {
            differing += 1;
            diff.put_pixel(x, y, image::Rgba([255, 0, 0, 255]));
        } else {
            diff.put_pixel(x, y, image::Rgba([e.0[0] / 3, e.0[1] / 3, e.0[2] / 3, 255]));
        }
    }
    DiffReport {
        differing,
        total: (w as u64) * (h as u64),
        max_delta,
        size_mismatch: false,
        diff_image: diff,
    }
}
