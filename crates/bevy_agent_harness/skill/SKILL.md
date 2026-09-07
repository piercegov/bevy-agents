---
name: bevy-agent
description: Inspect, drive and verify a running Bevy game with the `bevy-agent` CLI (Bevy Remote Protocol), and write frame-stepped integration tests with `bevy_agent_harness::testing::TestApp`. Use when working on a Bevy game and you need to check that a change actually works, look at entities/components/resources, inject input, take screenshots, read game logs, or add tests for gameplay.
---

# bevy-agent: verify Bevy changes like a web developer would

You have two tools for checking your work on a Bevy game, and you should use them instead of
assuming a change works because it compiles:

1. **`bevy-agent`** talks to a *running* game over HTTP. Inspect entities, inject input, take
   screenshots, read logs, pause and step frames.
2. **`TestApp`** (in `bevy_agent_harness::testing`) runs the game *inside a test*, frame by
   frame, with input injection, component assertions and golden screenshots.

## Prerequisites (check once per session)

```sh
bevy-agent --version || cargo install --path <path-to>/bevy_agent/crates/bevy_agent_harness
grep -rn "AgentHarnessPlugin" src/ || echo "plugin missing"
```

If the plugin is missing, add it to the game's `App` (gate it however the project gates dev tools):

```rust
use bevy::log::LogPlugin;
use bevy_agent_harness::AgentHarnessPlugin;

App::new()
    .add_plugins(DefaultPlugins.set(LogPlugin {
        custom_layer: bevy_agent_harness::log_layer, // makes `bevy-agent logs` work
        ..default()
    }))
    .add_plugins(AgentHarnessPlugin::default())      // BRP on http://127.0.0.1:15702
```

Components and resources you want to see must be reflectable: `#[derive(Reflect)]`,
`#[reflect(Component)]` (or `Resource`), and `app.register_type::<T>()`. If `bevy-agent get`
shows `"$error"` for a component or `ls --with Foo` cannot resolve `Foo`, this is why.

## The verification loop

```sh
# 1. build and launch in the background, logging to a file
cargo build && (cargo run > /tmp/game.log 2>&1 &)
# 2. wait for BRP to come up (retry a few times)
until bevy-agent status >/dev/null 2>&1; do sleep 1; done
# 3. orient yourself
bevy-agent status
bevy-agent ls --short
# 4. drive the game and observe
bevy-agent key space
bevy-agent get @Player --short
bevy-agent screenshot -o /tmp/shot.png     # then view the PNG
bevy-agent logs --level warn
# 5. always stop the game when done
pkill -f target/debug/<binary-name>
```

Every input command (`key`, `click`, `mouse ...`) blocks until the game has processed the
input, so the very next `get` already reflects it. For deterministic checks of time-based logic,
pause and step instead of sleeping:

```sh
bevy-agent pause
bevy-agent key --down d          # hold
bevy-agent step 60               # exactly 60 frames of 1/60 s
bevy-agent key --up d
bevy-agent get @Player Transform --short
bevy-agent resume
```

## Command cheatsheet

| Need | Command |
|---|---|
| What is running | `bevy-agent status` |
| Entity tree (names, components) | `bevy-agent ls --short`, `ls --with Enemy`, `ls --all` for internals |
| One entity | `bevy-agent get @Player`, `get 424v0 Transform Health` |
| Query | `bevy-agent query --with Enemy -d Transform -d Health --short` |
| Change a field | `bevy-agent mutate @Player Transform translation.x 100` |
| Spawn / edit | `spawn '{"Coin":{},"Transform":{...}}'`, `insert`, `remove`, `despawn`, `reparent` |
| Resources | `bevy-agent resource ls`, `resource get Score`, `resource set`, `resource mutate` |
| Type layout | `bevy-agent schema Health` |
| Keyboard | `bevy-agent key space`, `key w a s d`, `key --down w`, `key --up w`, `key --hold 10 space` |
| Mouse | `bevy-agent click 300 200`, `mouse move x y`, `mouse motion dx dy`, `mouse down|up`, `mouse scroll 1` |
| Screenshot | `bevy-agent screenshot -o /tmp/shot.png` (then look at it) |
| Logs | `bevy-agent logs`, `logs --level error`, `logs --grep collision`, `logs --follow`, `logs --clear` |
| Time | `bevy-agent pause`, `resume`, `step 10`, `step 1 --dt 0.1`, `speed 0.25`, `wait 30` |
| Stream changes | `bevy-agent watch @Player Transform` |
| Anything else | `bevy-agent methods`, `bevy-agent call world.query '{...}'` |

Useful flags: `--json` when you will parse the output, `--short` for readable type names,
`--url http://127.0.0.1:15702` (or `BEVY_AGENT_URL`) if the port is not the default.

Entities can be written as `@Name`, as Bevy's `index v generation` form (`424v0`), or as the raw
u64 id. Key names accept `space`, `w`, `left`, `enter`, `esc`, `f5`, or Bevy's `KeyCode`
variants (`KeyW`, `ArrowLeft`). Click and cursor positions are logical window pixels with the
origin at the top-left, matching `Window::cursor_position()`.

## Writing tests with TestApp

Put gameplay tests in `tests/` and make the game logic addable as a plugin so tests can include it.

```rust
use bevy::prelude::*;
use bevy_agent_harness::testing::TestApp;

#[test]
fn jump_leaves_the_ground() {
    let mut app = TestApp::new();              // headless, fixed 1/60 s per frame
    app.add_plugins(my_game::GamePlugin);      // must be before the first advance
    app.advance(1);                            // runs Startup
    let player = app.single::<Player>();
    let y0 = app.component::<Transform>(player).translation.y;
    app.tap(KeyCode::Space);                   // press + release, 2 frames
    app.advance(5);
    app.assert_component::<Transform>(player, |t| t.translation.y > y0);
    app.assert_no_log_errors();
}
```

- Input helpers (`press`, `release`, `tap`, `hold(key, n)`, `move_cursor`, `click`, `scroll`)
  advance the app so the input is already processed when they return.
- Find things with `single::<C>()`, `entities_with::<C>()`, `find_by_name("Player")`.
- Read state with `get`, `component`, `resource`, `world()`; drive it with `run_until(max, |world| ...)`.
- Logs from the run: `logs()`, `log_problems()`, `assert_no_log_errors()`.
- Visuals: `TestApp::with_rendering(640, 360)` then `screenshot()` or `assert_golden("name")`.
  Golden PNGs live in `tests/golden/`; the first run creates them, `UPDATE_GOLDEN=1` rewrites
  them, and a failure writes `tests/golden/__failures__/<name>.actual.png` and `.diff.png` for
  you to look at. Rendering tests need a GPU (or lavapipe/SwiftShader).
- Components used in `TestApp` do **not** need `Reflect`; only the BRP path does.

Run with `cargo test`. If a test needs plugins that assume a window or audio device, disable them
in the game plugin under `cfg(test)` or accept the warnings in the log.

## Gotchas

- `bevy-agent` needs the game running; "could not reach" means it is not, or the port differs.
  Only one game can bind a port; use `AgentHarnessPlugin::default().with_port(...)` and `--url`
  to run several.
- `screenshot -o` paths are resolved by the CLI to absolute paths, but the *game process* writes
  the file, so it must be able to reach that path.
- `pause` freezes `Time<Virtual>`; systems still run every frame, so anything not driven by
  `Time` (or `FixedUpdate`) keeps going. Use `step` for controlled progress.
- `ls` hides Bevy-internal entities and plumbing components. Pass `--all` when hunting for
  something unusual.
- `mutate` paths follow `bevy_reflect` syntax: `translation.x`, `.0` for tuple structs, `[2]` for
  lists. The error message tells you what the reflect type actually is.
- BRP serializes with reflection, so `spawn`/`insert` values must match the reflected shape
  (`Transform` needs `translation`, `rotation`, `scale`). Use `schema <Type>` to see it.
- Kill the game process when you are done. Leaving it running holds the port and the GPU.

See `reference.md` next to this file for the raw `harness.*` JSON-RPC shapes when using `call`
or writing your own client.
