# harness.* method reference

All methods are JSON-RPC 2.0 over HTTP POST to the BRP endpoint (default
`http://127.0.0.1:15702`). Methods whose name contains `+watch` stream responses as
server-sent events (`data: {...}\n\n`) until the client disconnects.

Entities are u64 ids (`Entity::to_bits`). Component and resource names are fully qualified
type paths (`bevy_transform::components::transform::Transform`); the CLI resolves short names,
the raw protocol does not.

| method | params | result |
|---|---|---|
| `harness.status` | none | `{ frame, fps, elapsed, real_elapsed, delta, paused, speed, stepping, entities, windows: [{entity, primary, width, height, physical_width, physical_height, scale_factor, cursor, focused}], cameras: [{entity, name, active, order, target, size}], process: {pid, exe, uptime, stale}, headless, log_capture }`. `stale` is true when the executable on disk is newer than the running process |
| `harness.exit` | none | `null`; the app exits cleanly after the current frame |
| `harness.input.key` | `{ key?: KeyCode, keys?: [KeyCode], state?: "tap"\|"press"\|"release", hold_frames?: 1, delay?: 0 }` | `{ queued, horizon_frames }` |
| `harness.input.mouse` | `{ button?: "Left", state?: "tap"\|"press"\|"release", position?: [x, y], hold_frames?, delay? }` | `{ queued, horizon_frames }` |
| `harness.input.cursor` | `{ position?: [x, y], delta?: [dx, dy], delay? }` | `{ queued, horizon_frames }` |
| `harness.input.scroll` | `{ x?: 0, y: 1, unit?: "line"\|"pixel", delay? }` | `{ queued, horizon_frames }` |
| `harness.input` | `{ actions: [Action \| { delay, action: Action }] }` where `Action` is `{kind: "key", key, state: "Pressed"\|"Released"}`, `{kind: "mouse_button", button, state}`, `{kind: "cursor", position}`, `{kind: "motion", delta}`, `{kind: "scroll", x, y, unit}` | `{ queued, horizon_frames }` |
| `harness.types` | `{ kind: "event"\|"message" }` | `[type_path, ...]` usable with `world.trigger_event` / `world.write_message` |
| `harness.events` | `{ event: type_path, entity?: entity }` | `{ recording_started, events: [value, ...], dropped }`; the first call per `(event, entity)` starts recording, later calls return and remove what was recorded since |
| `harness.logs` | `{ since?: seq, level?: "error"\|"warn"\|"info"\|"debug"\|"trace", limit?: 200, contains?: str, target?: prefix }` | `{ entries: [{seq, t, level, target, message}], next, matched, dropped }` |
| `harness.logs.clear` | none | `null` |
| `harness.pause` | none | status |
| `harness.resume` | none | status |
| `harness.step` | `{ frames?: 1, dt?: 0.01667 }` | status plus `until_frame` |
| `harness.time` | `{ speed?: f64, paused?: bool }` | status |
| `harness.wait_frame+watch` | `{ until_frame: u32 }` | status, once `frame >= until_frame` |
| `harness.screenshot` | `{ path?: string, camera?: entity, window?: entity }` | `{ id }` |
| `harness.screenshot.result+watch` | `{ id }` | `{ path, width, height }` or `{ png_base64, width, height }` |

Timing notes:

- Input handlers run after `Last`; queued actions are delivered at the start of the next frame
  (in `First`, before picking's input pass and `InputSystems`), both as the specific messages
  (`KeyboardInput`, `MouseButtonInput`, `CursorMoved`, ...) and as `WindowEvent`, so a `delay` of
  0 means "next frame". `horizon_frames` is how many
  frames until the last queued action fires; wait `horizon_frames + 1` frames to be sure the
  game has reacted.
- `until_frame` from `harness.step` accounts for the frame in flight; pass it straight to
  `harness.wait_frame+watch`.
- Screenshots complete one or two frames after the request; the `+watch` call returns exactly one
  event and then closes.

Built-in BRP methods (from Bevy): `world.query`, `world.get_components`, `world.list_components`,
`world.spawn_entity`, `world.despawn_entity`, `world.insert_components`,
`world.remove_components`, `world.mutate_components`, `world.reparent_entities`,
`world.get_resources`, `world.list_resources`, `world.insert_resources`,
`world.remove_resources`, `world.mutate_resources`, `registry.schema`, `rpc.discover`, plus
`world.get_components+watch` and `world.list_components+watch`.

Events, messages and schedules:

| method | params | result |
|---|---|---|
| `world.trigger_event` | `{ event: type_path, value?: reflected value }` | `null`; observers have run when it returns |
| `world.write_message` | `{ message: type_path, value?: reflected value }` | `null`; written after the current frame's systems, read next frame |
| `world.observe+watch` | `{ event: type_path, entity?: entity }` | stream of `[value, ...]` batches; `{"$error", "$dropped"}` entries mark overflow or unserializable events. Shares its recording with `harness.events` |
| `schedule.list` | none | `{ schedule_labels, empty_schedule_labels, unavailable_schedule_labels }` |
| `schedule.graph` | `{ schedule_label: "Update" }` | `{ schedule_data: { name, systems: [{name, exclusive, ...}], system_sets, hierarchy: [[set, node]], dependency: [[first, second]], components, conflicts } }` where a node is `{"System": i}` or `{"SystemSet": i}` |

`harness.logs` returns an error instead of an empty list when the game did not install
`bevy_agent_harness::log_layer`.

Environment read by the game: `BEVY_AGENT_HEADLESS=1` (with `agent_plugins(...)`) runs without a
window; `BEVY_AGENT_SIZE=1280x720` sets the headless window and render size. `bevy-agent run
--headless --size` sets both.

`bevy-agent methods` lists what the running game actually exposes.
