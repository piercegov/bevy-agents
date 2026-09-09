//! `bevy-agent` – command line access to a running Bevy game via the Bevy
//! Remote Protocol and the `harness.*` methods of `bevy_agent_harness`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bevy_agent_harness::client::{BrpClient, DEFAULT_URL, EntityNode, short_name};
use bevy_agent_harness::{entity_index, format_entity, parse_entity};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

#[derive(Parser)]
#[command(
    name = "bevy-agent",
    version,
    about = "Inspect and drive a running Bevy game over the Bevy Remote Protocol",
    long_about = "Inspect and drive a running Bevy game over the Bevy Remote Protocol.\n\n\
        The game must include `bevy_agent_harness::AgentHarnessPlugin`. Component names may be \
        given fully qualified (bevy_transform::components::transform::Transform) or by their \
        short name (Transform) when unambiguous. Entities are written as the u64 BRP id \
        (4294967298) or as index v generation (2v1)."
)]
struct Cli {
    /// BRP endpoint. Also read from BEVY_AGENT_URL.
    #[arg(long, global = true, env = "BEVY_AGENT_URL", default_value = DEFAULT_URL)]
    url: String,
    /// Emit machine-readable JSON instead of human-friendly text.
    #[arg(long, global = true)]
    json: bool,
    /// Shorten fully-qualified type names in output.
    #[arg(long, global = true)]
    short: bool,
    /// Timeout in seconds for blocking operations (screenshots, step, wait).
    #[arg(long, global = true, default_value_t = 30.0)]
    timeout: f64,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Frame, time, pause state, entity count, windows and cameras.
    Status,
    /// List the remote methods the game exposes.
    Methods,
    /// Entity hierarchy with names and component lists.
    #[command(alias = "list", alias = "tree")]
    Ls {
        /// Only entities that have all of these components.
        #[arg(long = "with", short = 'w')]
        with: Vec<String>,
        /// Hide component lists (faster on big worlds).
        #[arg(long)]
        no_components: bool,
        /// Print a flat list instead of a tree.
        #[arg(long)]
        flat: bool,
        /// Include Bevy-internal entities (observers, registered systems, resources, monitors).
        #[arg(long, short = 'a')]
        all: bool,
    },
    /// Show an entity's components (all reflectable ones, or the given subset).
    /// Entities can be given as `@Name`.
    Get {
        entity: String,
        components: Vec<String>,
    },
    /// Run a world.query and print matching entities.
    Query(QueryArgs),
    /// Spawn an entity from a JSON object of components.
    Spawn {
        /// e.g. '{"Transform": {"translation": [0,0,0], "rotation": [0,0,0,1], "scale": [1,1,1]}}'
        components: String,
    },
    /// Insert (or overwrite) components on an entity.
    Insert { entity: String, components: String },
    /// Remove components from an entity.
    Remove {
        entity: String,
        components: Vec<String>,
    },
    /// Despawn an entity (and its children).
    Despawn { entities: Vec<String> },
    /// Mutate one field of a component, e.g. `mutate 2v1 Transform translation.x 5.0`.
    Mutate {
        entity: String,
        component: String,
        path: String,
        value: String,
    },
    /// Set the parent of entities (omit --parent to detach).
    Reparent {
        entities: Vec<String>,
        #[arg(long)]
        parent: Option<String>,
    },
    /// Inspect and edit resources.
    #[command(subcommand, alias = "res")]
    Resource(ResourceCmd),
    /// Type schemas from the reflection registry.
    Schema {
        /// Type names to look up (short or full). With none, lists all matching crates.
        types: Vec<String>,
        /// Restrict to these crates.
        #[arg(long = "crate", short = 'c')]
        crates: Vec<String>,
    },
    /// Capture a screenshot to a PNG file.
    Screenshot {
        /// Output path (default screenshot.png in the current directory).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Capture what this camera renders.
        #[arg(long)]
        camera: Option<String>,
        /// Capture this window.
        #[arg(long)]
        window: Option<String>,
    },
    /// Tap keys in sequence, e.g. `key Space` or `key KeyW KeyA`.
    Key {
        keys: Vec<String>,
        /// Press without releasing.
        #[arg(long, conflicts_with = "up")]
        down: bool,
        /// Release a held key.
        #[arg(long)]
        up: bool,
        /// Frames to hold each tap (default 1).
        #[arg(long)]
        hold: Option<u32>,
        /// Return immediately instead of waiting until the input has been processed.
        #[arg(long)]
        no_wait: bool,
    },
    /// Click at a position (logical pixels, origin top-left) or at the current cursor.
    Click {
        x: Option<f32>,
        y: Option<f32>,
        #[arg(long, short, default_value = "left")]
        button: MouseButtonArg,
        #[arg(long)]
        hold: Option<u32>,
        /// Return immediately instead of waiting until the input has been processed.
        #[arg(long)]
        no_wait: bool,
    },
    /// Low-level mouse control.
    #[command(subcommand)]
    Mouse(MouseCmd),
    /// Read captured game logs.
    Logs {
        /// Only entries with sequence number >= N.
        #[arg(long)]
        since: Option<u64>,
        /// Minimum level: error, warn, info, debug, trace.
        #[arg(long, short)]
        level: Option<String>,
        /// Show at most N entries (default 200).
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Substring filter on the message.
        #[arg(long, short)]
        grep: Option<String>,
        /// Prefix filter on the target / module path.
        #[arg(long)]
        target: Option<String>,
        /// Keep polling and print new entries as they arrive.
        #[arg(long, short)]
        follow: bool,
        /// Clear the buffer instead of reading.
        #[arg(long)]
        clear: bool,
    },
    /// Pause the virtual clock (systems keep running, Time stands still).
    Pause,
    /// Resume the virtual clock.
    Resume,
    /// Advance N frames with a fixed dt and pause again. Waits for completion.
    Step {
        #[arg(default_value_t = 1)]
        frames: u32,
        /// Virtual seconds per frame (default 1/60).
        #[arg(long)]
        dt: Option<f64>,
        #[arg(long)]
        no_wait: bool,
    },
    /// Set the virtual clock speed multiplier.
    Speed { factor: f64 },
    /// Block until N more frames have rendered.
    Wait {
        #[arg(default_value_t = 1)]
        frames: u32,
    },
    /// Call any BRP method with raw JSON params.
    Call {
        method: String,
        params: Option<String>,
    },
    /// Stream component changes of an entity (Ctrl-C to stop).
    Watch {
        entity: String,
        components: Vec<String>,
    },
    /// Print or install the bundled agent skill that teaches AI agents this workflow.
    #[command(subcommand)]
    Skill(SkillCmd),
}

#[derive(Subcommand)]
enum SkillCmd {
    /// Print SKILL.md to stdout (paste into AGENTS.md / CLAUDE.md for other tools).
    Print {
        /// Print the method reference instead.
        #[arg(long)]
        reference: bool,
    },
    /// Write the skill into a project (or user) skills directory and add an AGENTS.md pointer.
    Install {
        /// Project root (default: current directory). A path ending in `.claude` or `.agents`
        /// (e.g. `~/.agents`) is used as the base directly, for user-wide installs.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Which agent tools to install for.
        #[arg(long = "for", value_enum, default_value_t = SkillTarget::Claude)]
        target: SkillTarget,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
        /// Do not add the pointer to AGENTS.md.
        #[arg(long)]
        no_agents_md: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SkillTarget {
    /// Claude Code: `.claude/skills/bevy-agent/`
    Claude,
    /// Codex and other Agent Skills tools: `.agents/skills/bevy-agent/`
    #[value(alias = "agents")]
    Codex,
    /// Both directories.
    All,
}

const AGENTS_MD_MARKER: &str = "<!-- bevy-agent skill -->";

const SKILL_MD: &str = include_str!("../../skill/SKILL.md");
const SKILL_REFERENCE_MD: &str = include_str!("../../skill/reference.md");

#[derive(Args)]
struct QueryArgs {
    /// Components whose values to fetch (entity must have them).
    #[arg(long, short = 'd')]
    data: Vec<String>,
    /// Components to fetch if present.
    #[arg(long, short = 'o')]
    option: Vec<String>,
    /// Report presence of these components as booleans.
    #[arg(long)]
    has: Vec<String>,
    /// Required components (not fetched).
    #[arg(long, short = 'w')]
    with: Vec<String>,
    /// Excluded components.
    #[arg(long, short = 'x')]
    without: Vec<String>,
    /// Fetch every reflectable component of matching entities.
    #[arg(long, short = 'a')]
    all: bool,
    #[arg(long)]
    strict: bool,
}

#[derive(Subcommand)]
enum ResourceCmd {
    /// List registered resource types.
    Ls,
    /// Print a resource's value.
    Get { name: String },
    /// Insert / replace a resource from JSON.
    Set { name: String, value: String },
    /// Mutate one field of a resource.
    Mutate {
        name: String,
        path: String,
        value: String,
    },
    /// Remove a resource.
    Remove { name: String },
}

#[derive(Subcommand)]
enum MouseCmd {
    /// Move the cursor to a position.
    Move { x: f32, y: f32 },
    /// Relative motion (for look controls).
    Motion { dx: f32, dy: f32 },
    /// Press a button.
    Down {
        #[arg(default_value = "left")]
        button: MouseButtonArg,
    },
    /// Release a button.
    Up {
        #[arg(default_value = "left")]
        button: MouseButtonArg,
    },
    /// Scroll the wheel.
    Scroll {
        y: f32,
        #[arg(long, default_value_t = 0.0)]
        x: f32,
        #[arg(long)]
        pixels: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum MouseButtonArg {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl MouseButtonArg {
    fn as_value(self) -> Value {
        json!(match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Middle => "Middle",
            Self::Back => "Back",
            Self::Forward => "Forward",
        })
    }
}

fn main() {
    // Exit quietly when piped into `head` etc. instead of panicking on EPIPE.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Parse `4294967298`, `2v1`, or `@Name` (exact `Name` component match).
fn entity_bits(client: &BrpClient, s: &str) -> Result<u64> {
    if let Some(name) = s.strip_prefix('@') {
        let rows = client.query(
            json!({ "components": [bevy_agent_harness::type_paths::NAME] }),
            json!({}),
            false,
        )?;
        let matches: Vec<u64> = rows
            .iter()
            .filter(|r| {
                r["components"][bevy_agent_harness::type_paths::NAME].as_str() == Some(name)
            })
            .filter_map(|r| r["entity"].as_u64())
            .collect();
        return match matches.as_slice() {
            [one] => Ok(*one),
            [] => bail!("no entity named `{name}`"),
            many => bail!("{} entities are named `{name}`: {:?}", many.len(), many),
        };
    }
    parse_entity(s).map(|e| e.to_bits()).ok_or_else(|| {
        anyhow!("invalid entity `{s}` (use the u64 id, index v generation like 2v1, or @Name)")
    })
}

/// Components that mark Bevy-internal entities hidden from `ls` by default.
const INTERNAL_MARKERS: &[&str] = &[
    "bevy_ecs::resource::IsResource",
    "bevy_ecs::observer::distributed_storage::Observer",
    "bevy_ecs::system::system_registry::SystemIdMarker",
    "bevy_window::monitor::Monitor",
];

/// Plumbing components hidden from `ls` unless `--all` is given.
const NOISE_COMPONENTS: &[&str] = &[
    "InheritedVisibility",
    "ViewVisibility",
    "VisibilityClass",
    "VisibleEntities",
    "RenderEntity",
    "SyncToRenderWorld",
    "TransformTreeChanged",
    "GlobalTransform",
    "Aabb",
    "CameraMainTextureUsages",
    "CameraRenderGraph",
    "Frustum",
    "DebandDither",
    "Tonemapping",
    "Msaa",
    "RawHandleWrapper",
    "RawHandleWrapperHolder",
    "CachedCursorOptions",
    "CachedWindow",
    "WinitWindowPressedKeys",
    "OnMonitor",
    "Anchor",
    "NoFrustumCulling",
];

fn is_noise(component: &str) -> bool {
    let short = short_name(component);
    NOISE_COMPONENTS.contains(&short.as_str())
}

fn is_internal(n: &EntityNode) -> bool {
    n.components.is_empty()
        || n.components.iter().any(|c| {
            INTERNAL_MARKERS.contains(&c.as_str())
                || c.ends_with("::IsResource")
                || c.ends_with("::Observer")
                || c.ends_with("::SystemIdMarker")
        })
}

fn parse_json(s: &str) -> Result<Value> {
    serde_json::from_str(s).with_context(|| format!("invalid JSON: {s}"))
}

fn parse_json_object(s: &str) -> Result<Map<String, Value>> {
    match parse_json(s)? {
        Value::Object(m) => Ok(m),
        other => bail!("expected a JSON object, got {other}"),
    }
}

/// Resolve the keys of a component map to full type paths.
fn resolve_map(client: &BrpClient, map: Map<String, Value>) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    for (k, v) in map {
        out.insert(client.resolve_component(&k)?, v);
    }
    Ok(out)
}

fn shorten_value(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (short_name(&k), shorten_value(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(shorten_value).collect()),
        other => other,
    }
}

struct Out {
    json: bool,
    short: bool,
}

impl Out {
    fn value(&self, v: Value) {
        let v = if self.short { shorten_value(v) } else { v };
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    }
    fn name(&self, s: &str) -> String {
        if self.short {
            short_name(s)
        } else {
            s.to_string()
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let client = BrpClient::new(cli.url.clone());
    let timeout = Duration::from_secs_f64(cli.timeout);
    let out = Out {
        json: cli.json,
        short: cli.short,
    };

    match cli.cmd {
        Cmd::Status => {
            let s = client.status()?;
            if out.json {
                out.value(s);
            } else {
                print_status(&s);
            }
        }
        Cmd::Methods => {
            let methods = client.methods()?;
            if out.json {
                out.value(json!(methods));
            } else {
                for m in methods {
                    println!("{m}");
                }
            }
        }
        Cmd::Ls {
            with,
            no_components,
            flat,
            all,
        } => {
            let mut nodes = client.entity_tree(true)?;
            if !all {
                nodes.retain(|n| !is_internal(n));
            }
            if !with.is_empty() {
                let with = client.resolve_components(&with)?;
                nodes.retain(|n| with.iter().all(|c| n.components.contains(c)));
            }
            nodes.sort_by_key(|n| entity_index(n.id));
            if out.json {
                out.value(serde_json::to_value(&nodes)?);
            } else if flat || !with.is_empty() {
                for n in &nodes {
                    println!("{}", format_node(n, no_components, out.short, all));
                }
            } else {
                print_tree(&nodes, no_components, out.short, all);
            }
        }
        Cmd::Get { entity, components } => {
            let e = entity_bits(&client, &entity)?;
            let v = if components.is_empty() {
                Value::Object(client.get_all_components(e)?)
            } else {
                let comps = client.resolve_components(&components)?;
                let r = client.get_components(e, &comps, false)?;
                Value::Object(bevy_agent_harness::client::component_results(&r))
            };
            if !out.json {
                println!("entity {} ({})", e, format_entity(e));
            }
            out.value(v);
        }
        Cmd::Query(q) => {
            let data = json!({
                "components": client.resolve_components(&q.data)?,
                "option": if q.all { json!("all") } else { json!(client.resolve_components(&q.option)?) },
                "has": client.resolve_components(&q.has)?,
            });
            let filter = json!({
                "with": client.resolve_components(&q.with)?,
                "without": client.resolve_components(&q.without)?,
            });
            let rows = client.query(data, filter, q.strict)?;
            if out.json {
                out.value(Value::Array(rows));
            } else {
                println!("{} entities", rows.len());
                for row in rows {
                    let e = row["entity"].as_u64().unwrap_or(0);
                    let mut body = row.clone();
                    body.as_object_mut().map(|m| m.remove("entity"));
                    let body = if out.short { shorten_value(body) } else { body };
                    println!("--- {} ({})", e, format_entity(e));
                    println!("{}", serde_json::to_string_pretty(&body)?);
                }
            }
        }
        Cmd::Spawn { components } => {
            let map = resolve_map(&client, parse_json_object(&components)?)?;
            let e = client.spawn(map)?;
            if out.json {
                out.value(json!({ "entity": e }));
            } else {
                println!("spawned {} ({})", e, format_entity(e));
            }
        }
        Cmd::Insert { entity, components } => {
            let map = resolve_map(&client, parse_json_object(&components)?)?;
            client.insert(entity_bits(&client, &entity)?, map)?;
            ok(&out);
        }
        Cmd::Remove { entity, components } => {
            let comps = client.resolve_components(&components)?;
            client.remove(entity_bits(&client, &entity)?, &comps)?;
            ok(&out);
        }
        Cmd::Despawn { entities } => {
            for e in entities {
                client.despawn(entity_bits(&client, &e)?)?;
            }
            ok(&out);
        }
        Cmd::Mutate {
            entity,
            component,
            path,
            value,
        } => {
            let comp = client.resolve_component(&component)?;
            client.mutate(
                entity_bits(&client, &entity)?,
                &comp,
                &path,
                parse_json(&value)?,
            )?;
            ok(&out);
        }
        Cmd::Reparent { entities, parent } => {
            let ids = entities
                .iter()
                .map(|e| entity_bits(&client, e))
                .collect::<Result<Vec<_>>>()?;
            let parent = parent
                .as_deref()
                .map(|p| entity_bits(&client, p))
                .transpose()?;
            client.reparent(&ids, parent)?;
            ok(&out);
        }
        Cmd::Resource(cmd) => match cmd {
            ResourceCmd::Ls => {
                let list = client.registered_resources()?;
                if out.json {
                    out.value(json!(list));
                } else {
                    for r in list {
                        println!("{}", out.name(&r));
                    }
                }
            }
            ResourceCmd::Get { name } => {
                let name = client.resolve_resource(&name)?;
                out.value(client.get_resource(&name)?);
            }
            ResourceCmd::Set { name, value } => {
                let name = client.resolve_resource(&name)?;
                client.insert_resource(&name, parse_json(&value)?)?;
                ok(&out);
            }
            ResourceCmd::Mutate { name, path, value } => {
                let name = client.resolve_resource(&name)?;
                client.mutate_resource(&name, &path, parse_json(&value)?)?;
                ok(&out);
            }
            ResourceCmd::Remove { name } => {
                let name = client.resolve_resource(&name)?;
                client.remove_resource(&name)?;
                ok(&out);
            }
        },
        Cmd::Schema { types, crates } => {
            let mut params = json!({});
            if !crates.is_empty() {
                params["with_crates"] = json!(crates);
            }
            if !types.is_empty() {
                // Resolve against components first, then resources; fall back to raw.
                let resolved: Vec<String> = types
                    .iter()
                    .map(|t| {
                        client
                            .resolve_component(t)
                            .or_else(|_| client.resolve_resource(t))
                            .unwrap_or_else(|_| t.clone())
                    })
                    .collect();
                // Narrow the request to the crates of the requested types, then pick them out.
                let mut with_crates: Vec<String> = resolved
                    .iter()
                    .filter_map(|r| r.split("::").next().map(String::from))
                    .collect();
                with_crates.sort();
                with_crates.dedup();
                params["with_crates"] = json!(with_crates);
                let schema = client.schema(params)?;
                let mut picked = Map::new();
                if let Some(m) = schema.as_object() {
                    for r in &resolved {
                        match m.get(r) {
                            Some(v) => {
                                picked.insert(r.clone(), v.clone());
                            }
                            None => eprintln!(
                                "no schema for `{r}` (is it registered with the type registry?)"
                            ),
                        }
                    }
                }
                out.value(Value::Object(picked));
            } else {
                out.value(client.schema(params)?);
            }
        }
        Cmd::Screenshot {
            out: path,
            camera,
            window,
        } => {
            let path = path.unwrap_or_else(|| PathBuf::from("screenshot.png"));
            let abs = std::env::current_dir()?.join(&path);
            let mut params = json!({ "path": abs });
            if let Some(c) = camera {
                params["camera"] = json!(entity_bits(&client, &c)?);
            }
            if let Some(w) = window {
                params["window"] = json!(entity_bits(&client, &w)?);
            }
            let r = client.screenshot(params, timeout)?;
            if out.json {
                out.value(r);
            } else {
                println!(
                    "saved {} ({}x{})",
                    r["path"].as_str().unwrap_or(""),
                    r["width"],
                    r["height"]
                );
            }
        }
        Cmd::Key {
            keys,
            down,
            up,
            hold,
            no_wait,
        } => {
            if keys.is_empty() {
                bail!("give at least one key, e.g. Space, KeyW, ArrowLeft, Enter, Escape");
            }
            let keys: Vec<String> = keys.into_iter().map(normalize_key).collect();
            let state = if down {
                "press"
            } else if up {
                "release"
            } else {
                "tap"
            };
            let mut params = json!({ "keys": keys, "state": state });
            if let Some(h) = hold {
                params["hold_frames"] = json!(h);
            }
            let r = client.key(params)?;
            finish_input(&client, &out, r, no_wait, timeout)?;
        }
        Cmd::Click {
            x,
            y,
            button,
            hold,
            no_wait,
        } => {
            let mut params = json!({ "button": button.as_value(), "state": "tap" });
            match (x, y) {
                (Some(x), Some(y)) => params["position"] = json!([x, y]),
                (None, None) => {}
                _ => bail!("give both x and y"),
            }
            if let Some(h) = hold {
                params["hold_frames"] = json!(h);
            }
            let r = client.mouse(params)?;
            finish_input(&client, &out, r, no_wait, timeout)?;
        }
        Cmd::Mouse(cmd) => {
            let r = match cmd {
                MouseCmd::Move { x, y } => client.cursor(json!({ "position": [x, y] }))?,
                MouseCmd::Motion { dx, dy } => client.cursor(json!({ "delta": [dx, dy] }))?,
                MouseCmd::Down { button } => {
                    client.mouse(json!({ "button": button.as_value(), "state": "press" }))?
                }
                MouseCmd::Up { button } => {
                    client.mouse(json!({ "button": button.as_value(), "state": "release" }))?
                }
                MouseCmd::Scroll { y, x, pixels } => client.scroll(json!({
                    "x": x, "y": y, "unit": if pixels { "pixel" } else { "line" }
                }))?,
            };
            finish_input(&client, &out, r, false, timeout)?;
        }
        Cmd::Logs {
            since,
            level,
            limit,
            grep,
            target,
            follow,
            clear,
        } => {
            if clear {
                client.call("harness.logs.clear", None)?;
                ok(&out);
                return Ok(());
            }
            let mut params = json!({});
            if let Some(s) = since {
                params["since"] = json!(s);
            }
            if let Some(l) = &level {
                params["level"] = json!(l);
            }
            if let Some(n) = limit {
                params["limit"] = json!(n);
            }
            if let Some(g) = &grep {
                params["contains"] = json!(g);
            }
            if let Some(t) = &target {
                params["target"] = json!(t);
            }
            let mut next = None;
            loop {
                let r = client.logs(params.clone())?;
                let entries = r["entries"].as_array().cloned().unwrap_or_default();
                if out.json {
                    for e in &entries {
                        println!("{}", serde_json::to_string(e)?);
                    }
                } else {
                    for e in &entries {
                        print_log_entry(e);
                    }
                    if next.is_none() && !follow {
                        let matched = r["matched"].as_u64().unwrap_or(0);
                        if matched as usize > entries.len() {
                            println!(
                                "({} more matching entries; use --limit or --since)",
                                matched as usize - entries.len()
                            );
                        }
                    }
                }
                std::io::stdout().flush().ok();
                if !follow {
                    break;
                }
                next = r["next"].as_u64();
                params["since"] = json!(next.unwrap_or(0));
                params["limit"] = json!(10_000);
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        Cmd::Pause => status_or_json(&out, client.pause()?),
        Cmd::Resume => status_or_json(&out, client.resume()?),
        Cmd::Step {
            frames,
            dt,
            no_wait,
        } => status_or_json(&out, client.step(frames, dt, !no_wait, timeout)?),
        Cmd::Speed { factor } => status_or_json(&out, client.set_time(json!({ "speed": factor }))?),
        Cmd::Wait { frames } => status_or_json(&out, client.wait_frames(frames, timeout)?),
        Cmd::Call { method, params } => {
            let params = params.as_deref().map(parse_json).transpose()?;
            if method.contains("+watch") {
                client.watch(&method, params, None, |r| {
                    match r {
                        Ok(v) => println!("{}", serde_json::to_string(&v).unwrap()),
                        Err(e) => eprintln!("error: {e}"),
                    }
                    std::io::stdout().flush().ok();
                    true
                })?;
            } else {
                out.value(client.call(&method, params)?);
            }
        }
        Cmd::Watch { entity, components } => {
            let e = entity_bits(&client, &entity)?;
            let comps = if components.is_empty() {
                client.list_components(Some(e))?
            } else {
                client.resolve_components(&components)?
            };
            let mut last: Option<Value> = None;
            client.watch(
                "world.get_components+watch",
                Some(json!({ "entity": e, "components": comps })),
                None,
                |r| {
                    match r {
                        Ok(v) => {
                            if last.as_ref() == Some(&v) {
                                return true;
                            }
                            last = Some(v.clone());
                            let v = if out.short { shorten_value(v) } else { v };
                            println!("{}", serde_json::to_string(&v).unwrap());
                        }
                        Err(e) => eprintln!("error: {e}"),
                    }
                    std::io::stdout().flush().ok();
                    true
                },
            )?;
        }
        Cmd::Skill(cmd) => match cmd {
            SkillCmd::Print { reference } => {
                print!(
                    "{}",
                    if reference {
                        SKILL_REFERENCE_MD
                    } else {
                        SKILL_MD
                    }
                );
            }
            SkillCmd::Install {
                dir,
                target,
                force,
                no_agents_md,
            } => {
                let root = dir.unwrap_or(std::env::current_dir()?);
                // A `.claude` / `.agents` directory given directly means "user-wide": install
                // straight into it and skip AGENTS.md.
                let base_name = root.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let (bases, project_root): (Vec<PathBuf>, Option<PathBuf>) = match base_name {
                    ".claude" | ".agents" => (vec![root.clone()], None),
                    _ => {
                        let mut bases = Vec::new();
                        if matches!(target, SkillTarget::Claude | SkillTarget::All) {
                            bases.push(root.join(".claude"));
                        }
                        if matches!(target, SkillTarget::Codex | SkillTarget::All) {
                            bases.push(root.join(".agents"));
                        }
                        (bases, Some(root.clone()))
                    }
                };

                let mut installed_rel = Vec::new();
                for base in &bases {
                    let skill_dir = base.join("skills").join("bevy-agent");
                    std::fs::create_dir_all(&skill_dir)
                        .with_context(|| format!("creating {}", skill_dir.display()))?;
                    for (name, body) in
                        [("SKILL.md", SKILL_MD), ("reference.md", SKILL_REFERENCE_MD)]
                    {
                        let path = skill_dir.join(name);
                        if path.exists() && !force {
                            bail!(
                                "{} already exists (use --force to overwrite)",
                                path.display()
                            );
                        }
                        std::fs::write(&path, body)
                            .with_context(|| format!("writing {}", path.display()))?;
                        println!("wrote {}", path.display());
                    }
                    if let Some(root) = &project_root
                        && let Ok(rel) = skill_dir.strip_prefix(root)
                    {
                        installed_rel.push(rel.to_path_buf());
                    }
                }

                if let (Some(root), false) = (&project_root, no_agents_md) {
                    // Point AGENTS.md at the copy every tool can read (`.agents` if present).
                    let pointer_dir = installed_rel
                        .iter()
                        .find(|p| p.starts_with(".agents"))
                        .or(installed_rel.first())
                        .cloned()
                        .unwrap_or_else(|| PathBuf::from(".claude/skills/bevy-agent"));
                    let agents_md = root.join("AGENTS.md");
                    let existing = std::fs::read_to_string(&agents_md).unwrap_or_default();
                    if existing.contains(AGENTS_MD_MARKER) {
                        println!("AGENTS.md already points at the skill");
                    } else {
                        let mut text = existing;
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&format!(
                            "{AGENTS_MD_MARKER}\n## Verifying changes to this Bevy game\n\n\
                             Before assuming a gameplay or rendering change works, read \
                             `{}` and follow its verification loop: run the game, inspect it with \
                             `bevy-agent`, and add `TestApp` tests. `bevy-agent skill print` shows \
                             the same instructions.\n<!-- /bevy-agent skill -->\n",
                            pointer_dir.join("SKILL.md").display()
                        ));
                        std::fs::write(&agents_md, text)
                            .with_context(|| format!("writing {}", agents_md.display()))?;
                        println!("updated {}", agents_md.display());
                    }
                }
                let tools = match (target, project_root.is_some()) {
                    (_, false) => "the agent tools reading that directory",
                    (SkillTarget::Claude, _) => "Claude Code",
                    (SkillTarget::Codex, _) => "Codex and other Agent Skills tools",
                    (SkillTarget::All, _) => "Claude Code, Codex and other Agent Skills tools",
                };
                println!("The `bevy-agent` skill is now available to {tools}.");
            }
        },
    }
    Ok(())
}

fn ok(out: &Out) {
    if out.json {
        println!("{{\"ok\":true}}");
    } else {
        println!("ok");
    }
}

/// Report a queued input and, unless `no_wait`, block until the game has run
/// enough frames to consume it (so a following `get` observes the effect).
fn finish_input(
    client: &BrpClient,
    out: &Out,
    r: Value,
    no_wait: bool,
    timeout: Duration,
) -> Result<()> {
    let horizon = r["horizon_frames"].as_u64().unwrap_or(1) as u32;
    if !no_wait {
        // +1: the queue is drained at the start of the frame after the request.
        client.wait_frames(horizon + 1, timeout)?;
    }
    if out.json {
        out.value(r);
    } else if no_wait {
        println!(
            "queued ({} pending, delivered over the next {} frame(s))",
            r["queued"], horizon
        );
    } else {
        println!("done ({} frame(s))", horizon + 1);
    }
    Ok(())
}

fn status_or_json(out: &Out, s: Value) {
    if out.json {
        out.value(s);
    } else {
        print_status(&s);
    }
}

fn print_status(s: &Value) {
    let fps = s["fps"]
        .as_f64()
        .map(|f| format!("{f:.1}"))
        .unwrap_or("-".into());
    println!(
        "frame {}  fps {}  elapsed {:.3}s  delta {:.4}s  speed {}x  {}{}",
        s["frame"],
        fps,
        s["elapsed"].as_f64().unwrap_or(0.0),
        s["delta"].as_f64().unwrap_or(0.0),
        s["speed"].as_f64().unwrap_or(1.0),
        if s["paused"].as_bool().unwrap_or(false) {
            "PAUSED"
        } else {
            "running"
        },
        s["stepping"]
            .as_u64()
            .filter(|n| *n > 0)
            .map(|n| format!(" (stepping {n})"))
            .unwrap_or_default()
    );
    println!("entities {}", s["entities"]);
    if let Some(ws) = s["windows"].as_array() {
        for w in ws {
            println!(
                "window {}{}  {}x{} (physical {}x{}, scale {})  cursor {}  {}",
                w["entity"],
                if w["primary"].as_bool().unwrap_or(false) {
                    " (primary)"
                } else {
                    ""
                },
                w["width"],
                w["height"],
                w["physical_width"],
                w["physical_height"],
                w["scale_factor"],
                w["cursor"]
                    .as_array()
                    .map(|c| format!("({}, {})", c[0], c[1]))
                    .unwrap_or("-".into()),
                if w["focused"].as_bool().unwrap_or(false) {
                    "focused"
                } else {
                    "unfocused"
                }
            );
        }
    }
    if let Some(cs) = s["cameras"].as_array() {
        for c in cs {
            println!(
                "camera {}{}  order {}  {}  target {}  size {}",
                c["entity"],
                c["name"]
                    .as_str()
                    .map(|n| format!(" \"{n}\""))
                    .unwrap_or_default(),
                c["order"],
                if c["active"].as_bool().unwrap_or(false) {
                    "active"
                } else {
                    "inactive"
                },
                c["target"].as_str().unwrap_or("-"),
                c["size"]
                    .as_array()
                    .map(|s| format!("{}x{}", s[0], s[1]))
                    .unwrap_or("-".into())
            );
        }
    }
}

fn print_log_entry(e: &Value) {
    println!(
        "{:>9.3} {:<5} {} {}",
        e["t"].as_f64().unwrap_or(0.0),
        e["level"].as_str().unwrap_or(""),
        e["target"].as_str().unwrap_or(""),
        e["message"].as_str().unwrap_or("")
    );
}

fn format_node(n: &EntityNode, no_components: bool, short: bool, all: bool) -> String {
    let mut s = format!("{} ({})", n.id, format_entity(n.id));
    if let Some(name) = &n.name {
        s.push_str(&format!("  \"{name}\""));
    }
    if !no_components {
        let comps: Vec<String> = n
            .components
            .iter()
            .filter(|c| {
                !matches!(
                    c.as_str(),
                    bevy_agent_harness::type_paths::NAME
                        | bevy_agent_harness::type_paths::CHILD_OF
                        | bevy_agent_harness::type_paths::CHILDREN
                ) && (all || !is_noise(c))
            })
            .map(|c| if short { short_name(c) } else { c.clone() })
            .collect();
        s.push_str(&format!("  [{}]", comps.join(", ")));
    }
    s
}

fn print_tree(nodes: &[EntityNode], no_components: bool, short: bool, all: bool) {
    let by_id: BTreeMap<u64, &EntityNode> = nodes.iter().map(|n| (n.id, n)).collect();
    fn rec(
        id: u64,
        depth: usize,
        by_id: &BTreeMap<u64, &EntityNode>,
        no_components: bool,
        short: bool,
        all: bool,
    ) {
        let Some(n) = by_id.get(&id) else { return };
        println!(
            "{}{}",
            "  ".repeat(depth),
            format_node(n, no_components, short, all)
        );
        let mut children = n.children.clone();
        children.sort_by_key(|c| entity_index(*c));
        for c in children {
            rec(c, depth + 1, by_id, no_components, short, all);
        }
    }
    for n in nodes {
        let is_root = n.parent.map(|p| !by_id.contains_key(&p)).unwrap_or(true);
        if is_root {
            rec(n.id, 0, &by_id, no_components, short, all);
        }
    }
}

/// Accept `space`, `Space`, `w`, `KeyW`, `left`, `ArrowLeft`, `1`, `Digit1`, `f5`.
fn normalize_key(k: String) -> String {
    let lower = k.to_ascii_lowercase();
    if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphabetic()) {
        return format!("Key{}", k.to_ascii_uppercase());
    }
    if k.len() == 1 && k.chars().all(|c| c.is_ascii_digit()) {
        return format!("Digit{k}");
    }
    match lower.as_str() {
        "space" => "Space".into(),
        "enter" | "return" => "Enter".into(),
        "esc" | "escape" => "Escape".into(),
        "tab" => "Tab".into(),
        "backspace" => "Backspace".into(),
        "delete" | "del" => "Delete".into(),
        "left" | "arrowleft" => "ArrowLeft".into(),
        "right" | "arrowright" => "ArrowRight".into(),
        "up" | "arrowup" => "ArrowUp".into(),
        "down" | "arrowdown" => "ArrowDown".into(),
        "shift" | "shiftleft" => "ShiftLeft".into(),
        "ctrl" | "control" | "controlleft" => "ControlLeft".into(),
        "alt" | "altleft" => "AltLeft".into(),
        "super" | "meta" | "cmd" | "superleft" => "SuperLeft".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "pageup" => "PageUp".into(),
        "pagedown" => "PageDown".into(),
        _ => {
            if let Some(n) = lower.strip_prefix('f')
                && n.parse::<u8>().is_ok()
            {
                return format!("F{n}");
            }
            k
        }
    }
}
