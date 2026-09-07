# harness.* method reference

All methods are JSON-RPC 2.0 over HTTP POST to the BRP endpoint (default
`http://127.0.0.1:15702`). Methods whose name contains `+watch` stream responses as
server-sent events (`data: {...}\n\n`) until the client disconnects.

Entities are u64 ids (`Entity::to_bits`). Component and resource names are fully qualified
type paths (`bevy_transform::components::transform::Transform`); the CLI resolves short names,
the raw protocol does not.

| method | params | result |
|---|---|---|
| `harness.status` | none | `{ frame, fps, elapsed, real_elapsed, delta, paused, speed, stepping, entities, windows: [{entity, primary, width, height, physical_width, physical_height, scale_factor, cursor, focused}], cameras: [{entity, name, active, order, target, size}] }` |
| `harness.input.key` | `{ key?: KeyCode, keys?: [KeyCode], state?: "tap"\|"press"\|"release", hold_frames?: 1, delay?: 0 }` | `{ queued, horizon_frames }` |
| `harness.input.mouse` | `{ button?: "Left", state?: "tap"\|"press"\|"release", position?: [x, y], hold_frames?, delay? }` | `{ queued, horizon_frames }` |
| `harness.input.cursor` | `{ position?: [x, y], delta?: [dx, dy], delay? }` | `{ queued, horizon_frames }` |
| `harness.input.scroll` | `{ x?: 0, y: 1, unit?: "line"\|"pixel", delay? }` | `{ queued, horizon_frames }` |
| `harness.input` | `{ actions: [Action \| { delay, action: Action }] }` where `Action` is `{kind: "key", key, state: "Pressed"\|"Released"}`, `{kind: "mouse_button", button, state}`, `{kind: "cursor", position}`, `{kind: "motion", delta}`, `{kind: "scroll", x, y, unit}` | `{ queued, horizon_frames }` |
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
  (before `InputSystems`), so a `delay` of 0 means "next frame". `horizon_frames` is how many
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
`world.remove_resources`, `world.mutate_resources`, `world.trigger_event`, `registry.schema`,
`rpc.discover`, plus `world.get_components+watch` and `world.list_components+watch`.
`bevy-agent methods` lists what the running game actually exposes.
