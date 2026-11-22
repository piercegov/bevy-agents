# bevy_agent

A Cargo workspace for tooling that makes Bevy games easier to develop with (and by) AI agents.

| crate | what it is |
|---|---|
| [`crates/bevy_agent_harness`](crates/bevy_agent_harness) | Plugin + CLI + test harness that lets an agent inspect, drive and verify a running Bevy game over the Bevy Remote Protocol. |

Requires Bevy 0.19.

## bevy_agent_harness

The goal is to bring Bevy development closer to web development: an agent changes some code,
launches the game, pokes at it, takes a screenshot, reads the logs, and checks that the change
actually did what it was supposed to, without a human in the loop.

It has three layers that work independently:

1. **`AgentHarnessPlugin`** (game side). Enables BRP over HTTP and adds `harness.*` methods for
   screenshots, input injection, log retrieval, pause/step, and status.
2. **`bevy-agent` CLI** and **`BrpClient`** (outside the game). Talk to a running game.
3. **`TestApp`** (in-process). Frame-stepped integration tests with input, assertions, captured
   logs, and golden screenshots rendered on a headless GPU.

### 1. Add the plugin to your game

```toml
[dependencies]
bevy_agent_harness = { path = "../bevy_agent/crates/bevy_agent_harness", default-features = false }
```

```rust
use bevy::prelude::*;
use bevy::log::LogPlugin;
use bevy_agent_harness::AgentHarnessPlugin;

App::new()
    // agent_plugins: lets `bevy-agent run --headless` start the game without a window
    .add_plugins(bevy_agent_harness::agent_plugins(DefaultPlugins.set(LogPlugin {
        custom_layer: bevy_agent_harness::log_layer,   // optional: enables `bevy-agent logs`
        ..default()
    })))
    .add_plugins(AgentHarnessPlugin::default())        // BRP on http://127.0.0.1:15702
    .run();
```

Components and resources you want to inspect must be reflectable and registered
(`#[derive(Reflect)] #[reflect(Component)]` + `app.register_type::<T>()`), which is the normal
BRP requirement. Gate the plugin behind a cargo feature or `cfg(debug_assertions)` for shipping builds.

### 2. Drive it from the shell

```sh
cargo install --path crates/bevy_agent_harness     # installs `bevy-agent`
cargo run -p bevy_agent_harness --example demo     # a small game to play with
```

```text
bevy-agent run -- cargo run             start the game in the background, wait until it answers
bevy-agent run --headless --size 1280x720 -- cargo run     no window, fixed size, no real input
bevy-agent run --restart -- cargo run   stop the running game first (do this after a rebuild)
bevy-agent stop                         clean exit, then kill
bevy-agent status                       frame, fps, time, pause state, windows, cameras, pid/binary
bevy-agent ls --short                   entity tree with names and components
bevy-agent get @Player                  all components of the entity named "Player"
bevy-agent get 424v0 Transform Health   specific components (short names resolve automatically)
bevy-agent query --with Coin -d Transform
bevy-agent key space                    tap a key (waits until the game processed it)
bevy-agent key w --down / --up          hold / release
bevy-agent click 300 200                move cursor and click (logical pixels, top-left origin)
bevy-agent mouse move|motion|down|up|scroll
bevy-agent screenshot -o shot.png       captured by the game's renderer, saved as PNG
bevy-agent logs --level warn --follow   captured log output (needs the LogPlugin line above)
bevy-agent pause / resume / step 10     freeze Time<Virtual>; step advances 10 frames at fixed dt
bevy-agent wait 30                      block until 30 more frames have run
bevy-agent mutate @Player Transform translation.x 100
bevy-agent spawn '{"Coin": {}, "Transform": {...}, "Name": "Extra"}'
bevy-agent insert|remove|despawn|reparent
bevy-agent resource get Score / set / mutate / ls
bevy-agent event ls / message ls        events and messages the game exposes (#[reflect(Event)] / #[reflect(Message)])
bevy-agent event trigger Heal '{"amount": 5}'
bevy-agent event read PlayerDamaged     events since the last read (first read starts recording); --expect exits 1 if none
bevy-agent event watch PlayerDamaged    stream events; -n 2 stops after two
bevy-agent message write SpawnCoin '{"x": 0, "y": 50}'
bevy-agent schedule ls / graph Update   schedules, and a schedule's systems with sets and ordering
bevy-agent schema Health                reflection schema of a type
bevy-agent watch @Player Transform      stream changes
bevy-agent call <method> '<json>'       any BRP method, raw
```

Global flags: `--url` (or `BEVY_AGENT_URL`), `--json` for machine-readable output, `--short` to
strip module paths from type names, `--timeout`. Entities can be written as the u64 BRP id, as
`index v generation` (what Bevy prints), or as `@Name`.

`run` fails with the tail of the game's output if the command exits or the game never answers,
and refuses to start while another game is answering on the same URL. Every command warns when
the game binary on disk is newer than the running process, so a forgotten restart cannot pass
for a verified change. `logs` fails with instructions (instead of printing nothing) when the
game did not install `log_layer`.

Headless runs need `agent_plugins(...)` around the game's plugin group. Cameras render to an
offscreen image that `screenshot` captures; UI is drawn and picking (`Pointer<Click>`) works,
but bevy_ui's `Interaction` does not react to the cursor without a real window.

Exit codes are usable as checks: `get @Player Stunned` prints what it could read and exits 1
(with the reason on stderr) when a component you named is missing or unreadable, so
`bevy-agent get @Player Stunned && ...` only continues if it is there. A bare `get @Player`
exits 0 and reports unreadable components inline as `"$error"`.

`ls` hides Bevy-internal entities (observers, registered systems, resources-as-entities) and
plumbing components (`ViewVisibility`, `RenderEntity`, ...) unless you pass `--all`. A bare
`get @Player` hides the same plumbing components (and says how many); name one or pass `--all`
(alias `--verbose`) to see it. Output keeps vectors on one line; `--json` is strict, unfiltered JSON.

### 3. Write tests

```rust
use bevy::prelude::*;
use bevy_agent_harness::testing::TestApp;

#[test]
fn space_damages_the_player() {
    let mut app = TestApp::new();               // headless, no GPU, fixed 1/60 s per frame
    app.add_plugins(my_game::GamePlugin);
    app.advance(1);                             // runs Startup
    let player = app.single::<Player>();
    app.tap(KeyCode::Space);                    // press + release, advances two frames
    app.advance(10);
    app.assert_component::<Health>(player, |h| h.0 < 100.0);
    app.assert_no_log_errors();
}

#[test]
fn title_screen_looks_right() {
    let mut app = TestApp::with_rendering(640, 360);   // real wgpu renderer, offscreen
    app.add_plugins(my_game::GamePlugin);
    app.advance(3);
    app.assert_golden("title_screen");          // tests/golden/title_screen.png
}
```

`TestApp` API highlights:

- `advance(frames)`, `advance_secs(s)`, `run_until(max_frames, |world| ...)`
- `press` / `release` / `tap` / `hold(key, frames)` / `move_cursor` / `click` / `scroll`; each
  advances the app so the input has been processed when it returns. `queue()` gives raw control.
- `get::<C>(e)`, `component::<C>(e)`, `assert_component::<C>(e, pred)`, `single::<C>()`,
  `entities_with::<C>()`, `find_by_name("Player")`, `resource::<R>()`, `component_names(e)`
- `logs()`, `log_problems()`, `assert_no_log_errors()`
- `screenshot()`, `save_screenshot(path)`, `assert_golden(name)`, `assert_golden_with(name, GoldenOptions)`.
  Missing golden files are created on first run; `UPDATE_GOLDEN=1` rewrites them; failures write
  `tests/golden/__failures__/<name>.actual.png` and `.diff.png`.

Use ordinary `App`/`MinimalPlugins` tests for small rules. Use TestApp for scheduling,
input, physics, and integration; launch the game only when live behavior or rendering
needs verification.

TestApp captures logs during construction, `add_plugins`, `with_app`, frame advancement,
and screenshots. Each app has its own buffer and target/level filter. Capture scopes
are serialized process-wide so synchronous Bevy worker logs reach the active app.
Detached/background work that outlives those scopes requires a dedicated process;
never run another capture-enabled TestApp from inside a capture scope or its systems.
Raw `app_mut()`/`world_mut()` operations are outside capture: use `with_app` when their
logs matter. An existing tracing subscriber or overflow makes log reads/assertions
fail explicitly. Use `TestApp::builder().capture_logs(false)` when capture is unwanted.
Filters accept `warn,my_game=debug`, not span/field directives.

For headless physics that reads mesh asset messages, initialize the asset type before
adding dependent plugins (no renderer required):

```rust,ignore
app.with_app(|app| {
    app.init_asset::<Mesh>();
    // Initialize StandardMaterial too only if your game systems require it.
    app.add_plugins(MyPhysicsPlugin);
});
```

Do this before populating assets; do not reinitialize an already configured asset type.
`entity_exists(entity)` checks ECS existence, including entities with a game's `Dead`
marker. `is_alive` remains a compatibility alias with the same meaning.

The harness replaces Bevy 0.19.1's `world.observe+watch` handler to return serialization
errors as data instead of panicking. It retains at most 1024 events per event/entity
key and 64 keys per world; overflow is reported as a `$dropped` error record. Successful
payloads retain Bevy's format. Keys persist for the world's lifetime, and readers of
the same key share a draining queue. This is live observation, not a durable event
recorder. Events need registered `ReflectEvent` metadata and serializable fields;
nonreflectable events need an application adapter.

Rendering tests need a wgpu adapter. On a headless CI box use lavapipe (`mesa-vulkan-drivers`)
or SwiftShader, or keep rendering tests behind a feature.

### 4. Teach your agent (bundled skill)

The crate ships a [skill](crates/bevy_agent_harness/skill/SKILL.md) describing the verification
workflow, the command cheatsheet, `TestApp` usage and the gotchas. Install it into a game repo:

```sh
cd my_game
bevy-agent skill install                 # Claude Code: .claude/skills/bevy-agent/
bevy-agent skill install --for codex     # Codex and other Agent Skills tools: .agents/skills/bevy-agent/
bevy-agent skill install --for all       # both
bevy-agent skill install --dir ~/.agents # user-wide (also ~/.claude)
bevy-agent skill print                   # dump SKILL.md, e.g. to paste into CLAUDE.md
```

Project installs also append a short pointer to `AGENTS.md` (created if needed, skipped with
`--no-agents-md`), so tools without skill support still find the instructions. The files are
plain copies embedded in the binary; rerun `skill install --force` after upgrading the crate to
refresh them. The skill triggers when the agent is working on a Bevy game and needs to verify a
change, inspect state, inject input, or write gameplay tests.

### How it works

- Input is injected by writing the same `KeyboardInput` / `MouseButtonInput` / `CursorMoved` /
  `MouseMotion` / `MouseWheel` messages winit would, at the start of the next frame, so
  `ButtonInput::just_pressed` and friends behave exactly as with a real keyboard.
- Pause freezes `Time<Virtual>`; systems still run (rendering, BRP, UI), but anything driven by
  `Time` or `FixedUpdate` stops. `step` temporarily switches to `TimeUpdateStrategy::ManualDuration`
  so each stepped frame advances by exactly `dt`, then restores the previous strategy.
- Screenshots use Bevy's own `Screenshot` component. Because capture is asynchronous, the BRP flow
  is `harness.screenshot` (returns an id) followed by `harness.screenshot.result+watch`, which
  streams one response when the image is ready. The CLI hides this.
- Logs are captured by a `tracing` layer into a process-global ring buffer; `harness.logs`
  supports `since`, `level`, `contains`, `target`, `limit`.
- `TestApp::with_rendering` builds `DefaultPlugins` without winit and pipelined rendering, keeps a
  `Window` entity (so cursor/window queries work), and retargets any camera that renders to a
  window onto an offscreen texture.

### Workspace layout

```
bevy_agent/
  Cargo.toml                  workspace
  crates/bevy_agent_harness/  the crate
    src/harness/              AgentHarnessPlugin and harness.* BRP methods
    src/client.rs             BrpClient (feature `client`)
    src/testing.rs            TestApp, golden diffs
    src/bin/bevy-agent.rs     CLI (feature `cli`, default)
    examples/demo.rs          sample game
    tests/                    TestApp tests, headless render test
```

### Roadmap / ideas

- MCP server mode (`bevy-agent mcp`) wrapping the same client, for hosts that prefer tools over a CLI.
- `bevy-agent diff-screenshot` for golden comparisons against a live game.
- Gamepad injection, text input, and window resize.
- A `record` / `replay` of injected inputs to turn a manual session into a test.
