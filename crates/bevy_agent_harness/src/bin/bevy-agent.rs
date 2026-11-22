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
    /// Start the game in the background and wait until it answers, e.g.
    /// `bevy-agent run -- cargo run`. Fails (with the end of the game's output)
    /// if the command exits or does not come up in time, and refuses to start
    /// while another game is answering on the same URL.
    Run {
        /// Run without a window: no real input reaches the game and the render size is
        /// fixed. The game must wrap its plugins in `bevy_agent_harness::agent_plugins`.
        #[arg(long)]
        headless: bool,
        /// Window / render size for --headless, e.g. 1280x720.
        #[arg(long, requires = "headless")]
        size: Option<String>,
        /// Stop a game that is already running on this URL first.
        #[arg(long)]
        restart: bool,
        /// File for the game's stdout and stderr (default: a per-port file in the temp dir).
        #[arg(long)]
        log: Option<PathBuf>,
        /// Seconds to wait for the game to answer, including any compile time.
        #[arg(long, default_value_t = 600.0)]
        ready_timeout: f64,
        /// The command that starts the game.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Stop the game answering on this URL (clean exit, then kill). Succeeds if none is running.
    Stop,
    /// Frame, time, pause state, entity count, windows, cameras and the game process.
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
        /// Include Bevy-internal entities (observers, registered systems, resources, monitors)
        /// and plumbing components.
        #[arg(long, short = 'a', alias = "verbose")]
        all: bool,
    },
    /// Show an entity's components (all reflectable ones, or the given subset).
    /// Entities can be given as `@Name`. Exits 1 if a component you asked for by
    /// name is missing or unreadable, so `get @Player Stunned && ...` is a presence check.
    ///
    /// With no component names, plumbing components (GlobalTransform, Aabb,
    /// visibility and render bookkeeping) are left out; name one or pass --all to see it.
    Get {
        entity: String,
        components: Vec<String>,
        /// Include plumbing components when listing everything.
        #[arg(long, short = 'a', alias = "verbose")]
        all: bool,
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
    /// Record a video of the game (needs ffmpeg on the game's machine).
    ///
    /// The video follows game time: while the game is paused nothing is added, and
    /// `step N` adds exactly N frames of game time, so `pause`, `record start`,
    /// inputs and `step`s, `record stop` gives a smooth clip with no dead time
    /// however long you take between commands.
    #[command(subcommand)]
    Record(RecordCmd),
    /// Serve a video or image over HTTP in the background and print a link that
    /// shows it in a browser, e.g. `bevy-agent share demo.mp4 --bind tailscale`.
    /// The server stops by itself once nobody has used it for a while.
    Share {
        /// The file: mp4, mov, mkv, webm, gif, png or jpg.
        file: PathBuf,
        #[command(flatten)]
        share_args: ShareArgs,
        /// Serve in this process instead of in the background.
        #[arg(long)]
        foreground: bool,
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
    /// List, trigger and observe events (types with `#[reflect(Event)]`).
    #[command(subcommand, alias = "events")]
    Event(EventCmd),
    /// List and write buffered messages (types with `#[reflect(Message)]`).
    #[command(subcommand, alias = "messages")]
    Message(MessageCmd),
    /// List schedules and the systems in them.
    #[command(subcommand, alias = "schedules")]
    Schedule(ScheduleCmd),
    /// Print or install the bundled agent skill that teaches AI agents this workflow.
    #[command(subcommand)]
    Skill(SkillCmd),
}

#[derive(Subcommand)]
enum RecordCmd {
    /// Start recording. Fails if a recording is already running.
    Start {
        /// Output file (default demo.mp4). The extension picks the format:
        /// .mp4, .mov, .mkv, .webm or .gif.
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Video frames per second of game time (default 60).
        #[arg(long)]
        fps: Option<f64>,
        /// Stop automatically after this many seconds of game time (default 120).
        #[arg(long)]
        max_seconds: Option<f64>,
        /// Record what this camera renders.
        #[arg(long)]
        camera: Option<String>,
        /// Record this window.
        #[arg(long)]
        window: Option<String>,
    },
    /// Stop recording, wait for the file to be finished and print where it is.
    Stop {
        /// Also serve the video (as `bevy-agent share` does) and print a link
        /// that plays it in a browser.
        #[arg(long)]
        share: bool,
        #[command(flatten)]
        share_args: ShareArgs,
    },
}

#[derive(Args)]
struct ShareArgs {
    /// Address to serve on: an IP address, or `tailscale` for this machine's
    /// Tailscale address. The default, 127.0.0.1, is reachable from this machine
    /// only; anyone who can reach the address can see the file.
    #[arg(long, env = bevy_agent_harness::share::BIND_ENV, default_value = "127.0.0.1")]
    bind: String,
    /// Port to serve on (default: any free port).
    #[arg(long)]
    port: Option<u16>,
    /// Stop serving after this many minutes without a request.
    #[arg(long, default_value_t = 60.0)]
    idle_minutes: f64,
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
enum EventCmd {
    /// List event types that can be triggered and observed.
    Ls,
    /// Trigger an event so its observers run, e.g. `event trigger Heal '{"amount": 5}'`.
    /// Entity events carry their target in the value: `'{"entity": 4294966870, ...}'`.
    Trigger {
        event: String,
        /// The event's value as JSON (omit for events without fields).
        value: Option<String>,
        /// Return immediately instead of waiting for the next frame.
        #[arg(long)]
        no_wait: bool,
    },
    /// Print events recorded since the last `read`. The first `read` of an event
    /// starts recording, so call it once before the action you want to check.
    Read {
        event: String,
        /// Only events targeting this entity (a separate recording from the global one).
        #[arg(long, short)]
        entity: Option<String>,
        /// Exit 1 unless at least one event was recorded.
        #[arg(long)]
        expect: bool,
    },
    /// Stream events as they happen (Ctrl-C to stop, or --count).
    Watch {
        event: String,
        #[arg(long, short)]
        entity: Option<String>,
        /// Stop after this many events.
        #[arg(long, short = 'n')]
        count: Option<usize>,
    },
}

#[derive(Subcommand)]
enum MessageCmd {
    /// List message types that can be written.
    Ls,
    /// Write a message for systems to read next frame, e.g. `message write SpawnCoin '{"x": 0, "y": 50}'`.
    #[command(alias = "send")]
    Write {
        message: String,
        /// The message's value as JSON (omit for messages without fields).
        value: Option<String>,
        /// Return immediately instead of waiting until systems have read it.
        #[arg(long)]
        no_wait: bool,
    },
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// List schedule labels.
    Ls,
    /// Systems of one schedule with their sets and ordering constraints.
    #[command(alias = "systems")]
    Graph {
        /// Schedule label as printed by `schedule ls`, e.g. Update.
        label: String,
        /// Only systems whose name contains this text.
        #[arg(long, short)]
        grep: Option<String>,
    },
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
    let url = cli.url.clone();
    if let Err(e) = run(cli) {
        let text = format!("{e:#}");
        eprintln!("error: {text}");
        let log = default_log_path(&url);
        if text.contains("could not reach") && log.exists() {
            eprintln!(
                "hint: if the game crashed, the output of the last `bevy-agent run` is in {}",
                log.display()
            );
        }
        std::process::exit(1);
    }
}

/// Where `run` sends the game's output unless `--log` is given.
fn default_log_path(url: &str) -> PathBuf {
    let port = url
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or("default");
    std::env::temp_dir().join(format!("bevy-agent-{port}.log"))
}

fn log_tail(path: &std::path::Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// `pid 123  target/debug/game  up 4.2s` from a status value.
fn describe_process(status: &Value) -> String {
    let p = &status["process"];
    match p["pid"].as_u64() {
        Some(pid) => format!(
            "pid {pid}  {}  up {:.1}s",
            p["exe"].as_str().unwrap_or("?"),
            p["uptime"].as_f64().unwrap_or(0.0)
        ),
        None => "unknown process (older bevy_agent_harness in the game)".into(),
    }
}

const STALE_WARNING: &str = "the game binary was rebuilt after this process started, so you are \
    looking at the OLD build. Restart it (`bevy-agent run --restart -- <command>`) before \
    verifying a change";

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn force_kill(pid: u32, group: bool) {
    let target = pid as libc::pid_t;
    // SAFETY: plain kill(2) on a pid (or its process group) we started or were told about.
    unsafe {
        libc::kill(if group { -target } else { target }, libc::SIGKILL);
    }
}
#[cfg(not(unix))]
fn force_kill(_pid: u32, _group: bool) {}

/// Whether `pid` belongs to the process group led by the command `run` spawned.
#[cfg(unix)]
fn started_by(pid: u64, leader: u32) -> bool {
    // SAFETY: getpgid only reads process metadata.
    unsafe { libc::getpgid(pid as libc::pid_t) == leader as libc::pid_t }
}
#[cfg(not(unix))]
fn started_by(_pid: u64, _leader: u32) -> bool {
    true
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Ask the game to exit, escalate to a kill, and wait until it is really gone.
/// Returns a description of what was stopped, or `None` if nothing was running.
fn stop_game(client: &BrpClient) -> Result<Option<String>> {
    let Ok(status) = client.status() else {
        return Ok(None);
    };
    let what = describe_process(&status);
    let pid = status["process"]["pid"].as_u64().map(|p| p as u32);
    // Older plugins have no `harness.exit`; the kill below covers them.
    let _ = client.call("harness.exit", None);
    let gone = |client: &BrpClient| client.status().is_err() && !pid.is_some_and(process_alive);
    if !wait_until(Duration::from_secs(5), || gone(client)) {
        if let Some(pid) = pid {
            force_kill(pid, false);
        }
        if !wait_until(Duration::from_secs(5), || gone(client)) {
            bail!(
                "the game ({what}) is still answering at {}; kill it by hand",
                client.url()
            );
        }
    }
    Ok(Some(what))
}

struct RunArgs {
    headless: bool,
    size: Option<String>,
    restart: bool,
    log: Option<PathBuf>,
    ready_timeout: f64,
    command: Vec<String>,
}

fn run_game(client: &BrpClient, out: &Out, args: RunArgs) -> Result<()> {
    if let Ok(status) = client.status() {
        if !args.restart {
            bail!(
                "a game is already answering at {} ({}). Stop it with `bevy-agent stop`, or pass \
                 `run --restart`",
                client.url(),
                describe_process(&status)
            );
        }
        stop_game(client)?;
    }

    let log = args.log.unwrap_or_else(|| default_log_path(client.url()));
    let log = std::env::current_dir()?.join(log);
    let file =
        std::fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;
    let mut cmd = std::process::Command::new(&args.command[0]);
    cmd.args(&args.command[1..])
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file);
    if args.headless {
        cmd.env(bevy_agent_harness::harness::headless::HEADLESS_ENV, "1");
        if let Some(size) = &args.size {
            cmd.env(bevy_agent_harness::harness::headless::SIZE_ENV, size);
        }
    } else {
        cmd.env_remove(bevy_agent_harness::harness::headless::HEADLESS_ENV);
    }
    #[cfg(unix)]
    {
        // Own process group: the game outlives this command and can be killed as a unit.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting `{}`", args.command.join(" ")))?;
    let leader = child.id();

    let mut poll = BrpClient::new(client.url());
    poll.timeout = Duration::from_secs(2);
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(args.ready_timeout);
    let fail = |why: String| -> anyhow::Error {
        anyhow!(
            "{why}\n--- last output ({}) ---\n{}",
            log.display(),
            log_tail(&log, 30)
        )
    };
    let status = loop {
        if let Ok(status) = poll.status() {
            let pid = status["process"]["pid"].as_u64();
            if pid.is_some_and(|pid| !started_by(pid, leader)) {
                force_kill(leader, true);
                bail!(
                    "another game ({}) answered at {} instead of the one just started",
                    describe_process(&status),
                    client.url()
                );
            }
            break status;
        }
        if let Some(exit) = child.try_wait()? {
            return Err(fail(format!(
                "`{}` exited ({exit}) before the game answered at {}",
                args.command.join(" "),
                client.url()
            )));
        }
        if std::time::Instant::now() >= deadline {
            force_kill(leader, true);
            let _ = child.kill();
            return Err(fail(format!(
                "the game did not answer at {} within {}s; it was killed. Does it add \
                 AgentHarnessPlugin on this port?",
                client.url(),
                args.ready_timeout
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    if args.headless && status["headless"] != true {
        stop_game(client)?;
        force_kill(leader, true);
        bail!(
            "the game started in a normal window although --headless was requested, so it was \
             stopped. Wrap its plugin group: `bevy_agent_harness::agent_plugins(DefaultPlugins)`"
        );
    }
    if out.json {
        out.value(json!({ "status": status, "log": log }));
    } else {
        println!("ready: {}", describe_process(&status));
        println!("output: {}", log.display());
        print_status(&status);
    }
    Ok(())
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

/// Valid JSON, indented like `to_string_pretty` except that arrays without
/// nested arrays/objects stay on one line, so vectors, quaternions and
/// matrices do not take a line per number.
fn compact_json(v: &Value) -> String {
    fn write(v: &Value, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth + 1);
        let close = "  ".repeat(depth);
        match v {
            Value::Array(a) if a.iter().any(|x| x.is_array() || x.is_object()) => {
                out.push_str("[\n");
                for (i, x) in a.iter().enumerate() {
                    out.push_str(&pad);
                    write(x, depth + 1, out);
                    out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
                }
                out.push_str(&close);
                out.push(']');
            }
            Value::Array(a) => {
                let items: Vec<String> = a.iter().map(Value::to_string).collect();
                out.push_str(&format!("[{}]", items.join(", ")));
            }
            Value::Object(m) if !m.is_empty() => {
                out.push_str("{\n");
                for (i, (k, x)) in m.iter().enumerate() {
                    out.push_str(&format!("{pad}{}: ", Value::from(k.as_str())));
                    write(x, depth + 1, out);
                    out.push_str(if i + 1 < m.len() { ",\n" } else { "\n" });
                }
                out.push_str(&close);
                out.push('}');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(v, 0, &mut out);
    out
}

struct Out {
    json: bool,
    short: bool,
}

impl Out {
    /// Strict pretty JSON with `--json`; otherwise the same data with arrays of
    /// plain values kept on one line (`"translation": [1.0, 2.0, 0.0]`).
    fn value(&self, v: Value) {
        let v = if self.short { shorten_value(v) } else { v };
        if self.json {
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
        } else {
            println!("{}", compact_json(&v));
        }
    }
    fn name(&self, s: &str) -> String {
        if self.short {
            short_name(s)
        } else {
            s.to_string()
        }
    }
}

impl ShareArgs {
    fn idle(&self) -> Duration {
        Duration::from_secs_f64(self.idle_minutes.clamp(0.1, 7.0 * 24.0 * 60.0) * 60.0)
    }
}

struct ShareLink {
    url: String,
    pid: u32,
    local_only: bool,
    idle_minutes: f64,
}

impl ShareLink {
    fn print(&self) {
        println!(
            "watch {}  (server pid {}, stops after {} min unused)",
            self.url, self.pid, self.idle_minutes
        );
        if self.local_only {
            println!(
                "note: that address only works on this machine; for another device use \
                 `--bind tailscale` (or an IP of this machine), or forward the port"
            );
        }
    }
}

/// Start a background `bevy-agent share --foreground` for `file` and wait until it answers.
fn share_file(file: &std::path::Path, args: &ShareArgs) -> Result<ShareLink> {
    use bevy_agent_harness::share;
    share::check(file)?;
    let ip = share::resolve_bind(&args.bind)?;
    let port = match args.port {
        Some(port) => port,
        None => share::free_port(ip)?,
    };
    if share::is_up(ip, port) {
        bail!("port {port} is already in use; pick another with --port");
    }
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("share")
        .arg(file)
        .arg("--foreground")
        .args(["--bind", &ip.to_string()])
        .args(["--port", &port.to_string()])
        .args(["--idle-minutes", &args.idle_minutes.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().context("starting the share server")?;
    let up = wait_until(Duration::from_secs(5), || {
        share::is_up(ip, port) || matches!(child.try_wait(), Ok(Some(_)))
    });
    if !up || !share::is_up(ip, port) {
        let _ = child.kill();
        bail!("the share server did not start on {ip}:{port}");
    }
    Ok(ShareLink {
        url: share::url(ip, port),
        pid: child.id(),
        local_only: ip.is_loopback(),
        idle_minutes: args.idle_minutes,
    })
}

fn run(cli: Cli) -> Result<()> {
    let client = BrpClient::new(cli.url.clone());
    let timeout = Duration::from_secs_f64(cli.timeout);
    let out = Out {
        json: cli.json,
        short: cli.short,
    };

    // Verifying against a process that predates the latest build is the easiest way
    // to "confirm" a change that is not running, so say so on every command.
    if !matches!(
        cli.cmd,
        Cmd::Skill(_) | Cmd::Run { .. } | Cmd::Stop | Cmd::Share { .. }
    ) && let Ok(status) = client.status()
        && status["process"]["stale"] == true
    {
        eprintln!("warning: {STALE_WARNING}");
    }

    match cli.cmd {
        Cmd::Run {
            headless,
            size,
            restart,
            log,
            ready_timeout,
            command,
        } => run_game(
            &client,
            &out,
            RunArgs {
                headless,
                size,
                restart,
                log,
                ready_timeout,
                command,
            },
        )?,
        Cmd::Stop => match stop_game(&client)? {
            Some(what) if out.json => out.value(json!({ "stopped": what })),
            Some(what) => println!("stopped {what}"),
            None if out.json => out.value(json!({ "stopped": null })),
            None => println!("no game is running at {}", client.url()),
        },
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
        Cmd::Get {
            entity,
            components,
            all,
        } => {
            let e = entity_bits(&client, &entity)?;
            let mut hidden = 0;
            let v = if components.is_empty() {
                let mut map = client.get_all_components(e)?;
                // `--json` is for parsing and always carries everything.
                if !all && !out.json {
                    let before = map.len();
                    map.retain(|name, _| !is_noise(name));
                    hidden = before - map.len();
                }
                Value::Object(map)
            } else {
                let comps = client.resolve_components(&components)?;
                let r = client.get_components(e, &comps, false)?;
                Value::Object(bevy_agent_harness::client::component_results(&r))
            };
            if !out.json {
                println!("entity {} ({})", e, format_entity(e));
            }
            // Only components asked for by name count as failures: a bare `get` lists
            // everything on the entity, where unreadable components are routine.
            let failed = if components.is_empty() {
                Vec::new()
            } else {
                failed_components(&v)
            };
            out.value(v);
            if hidden > 0 {
                println!(
                    "({hidden} plumbing component(s) hidden; name one or pass --all to see them)"
                );
            }
            if !failed.is_empty() {
                bail!("entity {} ({}): {}", e, format_entity(e), failed.join("; "));
            }
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
                    println!("{}", compact_json(&body));
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
        Cmd::Record(RecordCmd::Start {
            out: path,
            fps,
            max_seconds,
            camera,
            window,
        }) => {
            let path = path.unwrap_or_else(|| PathBuf::from("demo.mp4"));
            let abs = std::env::current_dir()?.join(&path);
            let mut params = json!({ "path": abs });
            if let Some(fps) = fps {
                params["fps"] = json!(fps);
            }
            if let Some(max) = max_seconds {
                params["max_seconds"] = json!(max);
            }
            if let Some(c) = camera {
                params["camera"] = json!(entity_bits(&client, &c)?);
            }
            if let Some(w) = window {
                params["window"] = json!(entity_bits(&client, &w)?);
            }
            let r = client.record_start(params)?;
            if out.json {
                out.value(r);
            } else {
                let paused = client.status()?["paused"].as_bool().unwrap_or(false);
                println!(
                    "recording to {} at {} fps{}",
                    r["path"].as_str().unwrap_or(""),
                    r["fps"],
                    if paused {
                        " (game is paused: `step` adds frames)"
                    } else {
                        ""
                    }
                );
            }
        }
        Cmd::Record(RecordCmd::Stop { share, share_args }) => {
            let mut r = client.record_stop(timeout.max(Duration::from_secs(120)))?;
            let shared = match (share, r["path"].as_str()) {
                (true, Some(path)) => Some(share_file(std::path::Path::new(path), &share_args)),
                _ => None,
            };
            if let Some(Ok(link)) = &shared {
                r["url"] = json!(link.url);
            }
            if out.json {
                out.value(r);
            } else {
                let bytes = r["bytes"].as_f64().unwrap_or(0.0);
                let size = if bytes >= 1e6 {
                    format!("{:.1} MB", bytes / 1e6)
                } else {
                    format!("{:.0} kB", bytes / 1e3)
                };
                println!(
                    "saved {} ({}x{}, {} frames, {:.1}s, {size})",
                    r["path"].as_str().unwrap_or(""),
                    r["width"],
                    r["height"],
                    r["frames"],
                    r["seconds"].as_f64().unwrap_or(0.0),
                );
                if let Some(Ok(link)) = &shared {
                    link.print();
                }
            }
            // The video is saved either way; a failed share is still an error.
            if let Some(Err(e)) = shared {
                return Err(e.context("the video was saved but could not be shared"));
            }
        }
        Cmd::Share {
            file,
            share_args,
            foreground,
        } => {
            use bevy_agent_harness::share;
            let file = std::env::current_dir()?.join(file);
            if foreground {
                let ip = share::resolve_bind(&share_args.bind)?;
                let port = match share_args.port {
                    Some(port) => port,
                    None => share::free_port(ip)?,
                };
                share::check(&file)?;
                eprintln!("serving {} at {}", file.display(), share::url(ip, port));
                share::serve(&file, ip, port, share_args.idle())?;
            } else {
                let link = share_file(&file, &share_args)?;
                if out.json {
                    out.value(json!({ "url": link.url, "pid": link.pid, "path": file }));
                } else {
                    link.print();
                }
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
            warn_headless_interaction(&client);
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
            warn_headless_interaction(&client);
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
        Cmd::Event(cmd) => match cmd {
            EventCmd::Ls => {
                let list = client.registered_events()?;
                if out.json {
                    out.value(json!(list));
                } else {
                    for e in list {
                        println!("{}", out.name(&e));
                    }
                }
            }
            EventCmd::Trigger {
                event,
                value,
                no_wait,
            } => {
                let event = client.resolve_event(&event)?;
                let value = value.as_deref().map(parse_json).transpose()?;
                client.trigger_event(&event, value)?;
                if !no_wait {
                    client.wait_frames(1, timeout)?;
                }
                ok(&out);
            }
            EventCmd::Read {
                event,
                entity,
                expect,
            } => {
                let event = client.resolve_event(&event)?;
                let entity = entity
                    .as_deref()
                    .map(|e| entity_bits(&client, e))
                    .transpose()?;
                let r = client.recorded_events(&event, entity)?;
                let started = r["recording_started"].as_bool().unwrap_or(false);
                let events = r["events"].as_array().cloned().unwrap_or_default();
                let dropped = r["dropped"].as_u64().unwrap_or(0);
                if out.json {
                    out.value(r);
                } else {
                    if dropped > 0 {
                        eprintln!(
                            "warning: {dropped} older event(s) were dropped (buffer overflow)"
                        );
                    }
                    print_events(&events, out.short);
                    if started {
                        println!(
                            "recording {} from now on; run `event read` again after your action",
                            short_name(&event)
                        );
                    } else {
                        println!("{} event(s) since the last read", events.len());
                    }
                }
                if expect && events.is_empty() {
                    if started {
                        bail!(
                            "no {} events: recording only just started, so earlier ones were not seen",
                            short_name(&event)
                        );
                    }
                    bail!("no {} events since the last read", short_name(&event));
                }
            }
            EventCmd::Watch {
                event,
                entity,
                count,
            } => {
                let event = client.resolve_event(&event)?;
                let entity = entity
                    .as_deref()
                    .map(|e| entity_bits(&client, e))
                    .transpose()?;
                let mut seen = 0;
                client.watch(
                    "world.observe+watch",
                    Some(bevy_agent_harness::client::observe_params(&event, entity)),
                    None,
                    |r| {
                        match r {
                            Ok(v) => {
                                let batch = v.as_array().cloned().unwrap_or_else(|| vec![v]);
                                seen += print_events(&batch, out.short);
                            }
                            Err(e) => eprintln!("error: {e}"),
                        }
                        std::io::stdout().flush().ok();
                        count.is_none_or(|c| seen < c)
                    },
                )?;
            }
        },
        Cmd::Message(cmd) => match cmd {
            MessageCmd::Ls => {
                let list = client.registered_messages()?;
                if out.json {
                    out.value(json!(list));
                } else {
                    for m in list {
                        println!("{}", out.name(&m));
                    }
                }
            }
            MessageCmd::Write {
                message,
                value,
                no_wait,
            } => {
                let message = client.resolve_message(&message)?;
                let value = value.as_deref().map(parse_json).transpose()?;
                client.write_message(&message, value)?;
                if !no_wait {
                    // Written after this frame's systems; readers see it next frame.
                    client.wait_frames(1, timeout)?;
                }
                ok(&out);
            }
        },
        Cmd::Schedule(cmd) => match cmd {
            ScheduleCmd::Ls => {
                let r = client.schedule_list()?;
                if out.json {
                    out.value(r);
                } else {
                    for (key, note) in [
                        ("schedule_labels", ""),
                        ("empty_schedule_labels", "  (no systems)"),
                        (
                            "unavailable_schedule_labels",
                            "  (running; cannot be inspected)",
                        ),
                    ] {
                        let mut labels: Vec<&str> = r[key]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect();
                        labels.sort_unstable();
                        for l in labels {
                            println!("{l}{note}");
                        }
                    }
                }
            }
            ScheduleCmd::Graph { label, grep } => {
                let data = client.schedule_graph(&label)?;
                if out.json {
                    out.value(data);
                } else {
                    print_schedule(&data, grep.as_deref(), out.short);
                }
            }
        },
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

/// `Name: reason` for every entry of a `get` result that carries a `$error`.
fn failed_components(result: &Value) -> Vec<String> {
    let Some(map) = result.as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter(|(_, v)| v.get("$error").is_some())
        .map(|(name, v)| {
            let name = short_name(name);
            if v["$status"] == "missing" {
                format!("{name} is not present")
            } else {
                format!(
                    "{name} could not be read ({})",
                    v["$error"].as_str().unwrap_or("unknown error")
                )
            }
        })
        .collect()
}

/// Print a batch of recorded/streamed events, one JSON value per line.
/// Returns how many real events (not overflow markers) were printed.
fn print_events(events: &[Value], short: bool) -> usize {
    let mut n = 0;
    for ev in events {
        if let Some(dropped) = ev.get("$dropped") {
            eprintln!("warning: {dropped} older event(s) were dropped (buffer overflow)");
            continue;
        }
        let ev = if short {
            shorten_value(ev.clone())
        } else {
            ev.clone()
        };
        println!("{}", serde_json::to_string(&ev).unwrap());
        n += 1;
    }
    n
}

fn print_schedule(data: &Value, grep: Option<&str>, short: bool) {
    let systems = data["systems"].as_array().cloned().unwrap_or_default();
    let sets = data["system_sets"].as_array().cloned().unwrap_or_default();
    let show = |s: &str| if short { short_name(s) } else { s.to_string() };
    // A node is `{"System": i}` or `{"SystemSet": i}`.
    let node_name = |node: &Value| -> Option<String> {
        if let Some(i) = node["System"].as_u64() {
            systems.get(i as usize)?["name"].as_str().map(show)
        } else {
            let name = sets.get(node["SystemSet"].as_u64()? as usize)?["name"].as_str()?;
            // Every system has an implicit set named after it; show it as the system.
            Some(show(name.strip_prefix("SystemTypeSet:").unwrap_or(name)))
        }
    };
    let n = systems.len();
    let (mut member_of, mut after, mut before) = (
        vec![Vec::new(); n],
        vec![Vec::new(); n],
        vec![Vec::new(); n],
    );
    for edge in data["hierarchy"].as_array().into_iter().flatten() {
        let (Some(set), Some(sys)) = (edge[0].as_u64(), edge[1]["System"].as_u64()) else {
            continue;
        };
        if let Some(name) = sets.get(set as usize).and_then(|s| s["name"].as_str())
            && !name.starts_with("SystemTypeSet:")
            && let Some(slot) = member_of.get_mut(sys as usize)
        {
            slot.push(show(name));
        }
    }
    for edge in data["dependency"].as_array().into_iter().flatten() {
        if let (Some(i), Some(name)) = (edge[1]["System"].as_u64(), node_name(&edge[0]))
            && let Some(slot) = after.get_mut(i as usize)
        {
            slot.push(name);
        }
        if let (Some(i), Some(name)) = (edge[0]["System"].as_u64(), node_name(&edge[1]))
            && let Some(slot) = before.get_mut(i as usize)
        {
            slot.push(name);
        }
    }
    let mut shown = 0;
    for (i, sys) in systems.iter().enumerate() {
        let name = sys["name"].as_str().unwrap_or("?");
        if grep.is_some_and(|g| !name.contains(g)) {
            continue;
        }
        shown += 1;
        let mut line = show(name);
        if sys["exclusive"].as_bool().unwrap_or(false) {
            line.push_str("  [exclusive]");
        }
        for (label, list) in [
            ("in", &member_of[i]),
            ("after", &after[i]),
            ("before", &before[i]),
        ] {
            if !list.is_empty() {
                line.push_str(&format!("  {label}: {}", list.join(", ")));
            }
        }
        println!("{line}");
    }
    let conflicts = data["conflicts"].as_array().map(Vec::len).unwrap_or(0);
    println!(
        "{shown} of {n} systems in {} (listing order is not run order; {conflicts} unordered \
         access conflict(s), see --json)",
        data["name"].as_str().unwrap_or("?")
    );
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

/// bevy_ui's `Interaction` only follows cameras that target a window, so in a
/// headless run it never reacts to the cursor. Say so when the game uses it,
/// instead of letting a dead button look like a bug in the game.
fn warn_headless_interaction(client: &BrpClient) {
    let headless = client.status().is_ok_and(|s| s["headless"] == true);
    if !headless {
        return;
    }
    let uses_interaction = client
        .query(
            json!({}),
            json!({ "with": ["bevy_ui::focus::Interaction"] }),
            false,
        )
        .is_ok_and(|rows| !rows.is_empty());
    if uses_interaction {
        eprintln!(
            "note: headless run: bevy_ui `Interaction` components do not react to the cursor \
             here (picking events such as `Pointer<Click>` do). Use a windowed `bevy-agent run` \
             to check `Interaction`-driven buttons."
        );
    }
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
    if s["process"].is_object() {
        println!(
            "process {}{}{}",
            describe_process(s),
            if s["headless"] == true {
                "  headless"
            } else {
                ""
            },
            if s["process"]["stale"] == true {
                "  STALE (binary rebuilt since start)"
            } else {
                ""
            }
        );
    }
    if s["recording"].is_object() {
        println!(
            "recording {}  {} frames ({:.1}s)",
            s["recording"]["path"].as_str().unwrap_or(""),
            s["recording"]["frames"],
            s["recording"]["seconds"].as_f64().unwrap_or(0.0)
        );
    }
    if s["log_capture"] == false {
        println!("logs NOT captured (`bevy-agent logs` will fail; see its error for the fix)");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_json_keeps_flat_arrays_on_one_line_and_stays_valid() {
        let v = json!({
            "Transform": { "translation": [1.5, 2.0, 0.0], "tags": [] },
            "Children": [[1, 2], {"a": "x\"y"}],
            "Player": {},
            "Health": 90.0,
        });
        let text = compact_json(&v);
        assert!(text.contains("\"translation\": [1.5, 2.0, 0.0]"), "{text}");
        assert!(text.contains("\"Player\": {}"), "{text}");
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), v);
    }

    #[test]
    fn failed_components_reports_only_errors() {
        let v = json!({
            "a::Health": 1,
            "a::Coin": {"$error": "not present", "$status": "missing"},
            "a::Blob": {"$error": "cannot serialize", "$status": "unreadable_or_unknown"},
        });
        assert_eq!(
            failed_components(&v),
            [
                "Blob could not be read (cannot serialize)",
                "Coin is not present"
            ]
        );
    }
}
