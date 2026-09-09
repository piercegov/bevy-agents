//! In-memory log capture so agents can read `info!`/`warn!`/`error!` output
//! from a running game via `harness.logs`.
//!
//! Wire it into `LogPlugin`:
//!
//! ```no_run
//! # use bevy::prelude::*;
//! # use bevy::log::LogPlugin;
//! App::new().add_plugins(DefaultPlugins.set(LogPlugin {
//!     custom_layer: bevy_agent_harness::log_layer,
//!     ..default()
//! }));
//! ```

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use bevy::log::BoxedLayer;
use bevy::prelude::*;
use bevy::remote::{BrpError, BrpResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use super::parse_params;

/// A captured log record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    /// Monotonic sequence number, starts at 0 for the process.
    pub seq: u64,
    /// Seconds since the log buffer was created.
    pub t: f64,
    /// `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE`.
    pub level: String,
    /// Module path / tracing target.
    pub target: String,
    /// The formatted message (plus any extra `key=value` fields).
    pub message: String,
}

#[derive(Debug)]
struct Inner {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
    capacity: usize,
    dropped: u64,
    start: Instant,
}

/// Shared ring buffer of captured log entries. Cloning shares the buffer.
#[derive(Resource, Clone, Debug)]
pub struct LogBuffer(Arc<Mutex<Inner>>);

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            entries: VecDeque::new(),
            next_seq: 0,
            capacity: capacity.max(1),
            dropped: 0,
            start: Instant::now(),
        })))
    }

    pub fn set_capacity(&self, capacity: usize) {
        let mut inner = self.0.lock().unwrap();
        inner.capacity = capacity.max(1);
        while inner.entries.len() > inner.capacity {
            inner.entries.pop_front();
            inner.dropped += 1;
        }
    }

    pub fn push(&self, level: &str, target: &str, message: String) {
        let mut inner = self.0.lock().unwrap();
        let entry = LogEntry {
            seq: inner.next_seq,
            t: inner.start.elapsed().as_secs_f64(),
            level: level.to_string(),
            target: target.to_string(),
            message,
        };
        inner.next_seq += 1;
        inner.entries.push_back(entry);
        if inner.entries.len() > inner.capacity {
            inner.entries.pop_front();
            inner.dropped += 1;
        }
    }

    /// Next sequence number that will be assigned.
    pub fn next_seq(&self) -> u64 {
        self.0.lock().unwrap().next_seq
    }

    /// All entries with `seq >= since`.
    pub fn since(&self, since: u64) -> Vec<LogEntry> {
        self.0
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.seq >= since)
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        let mut inner = self.0.lock().unwrap();
        inner.entries.clear();
    }

    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.0.lock().unwrap().entries.iter().cloned().collect()
    }

    fn dropped(&self) -> u64 {
        self.0.lock().unwrap().dropped
    }
}

/// The process-wide buffer used by [`log_layer`]. `tracing` only allows one
/// global subscriber per process, so capture is global too.
pub fn global_buffer() -> &'static LogBuffer {
    static GLOBAL: OnceLock<LogBuffer> = OnceLock::new();
    GLOBAL.get_or_init(|| LogBuffer::new(10_000))
}

/// Use as `LogPlugin { custom_layer: bevy_agent_harness::log_layer, .. }`.
pub fn log_layer(app: &mut App) -> Option<BoxedLayer> {
    let buffer = global_buffer().clone();
    app.insert_resource(buffer.clone());
    Some(Box::new(CaptureLayer { buffer }))
}

pub(crate) fn ensure_buffer(app: &mut App, capacity: usize) {
    if !app.world().contains_resource::<LogBuffer>() {
        app.insert_resource(global_buffer().clone());
    }
    app.world().resource::<LogBuffer>().set_capacity(capacity);
}

struct CaptureLayer {
    buffer: LogBuffer,
}

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        self.buffer
            .push(meta.level().as_str(), meta.target(), visitor.finish());
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<String>,
}

impl MessageVisitor {
    fn finish(mut self) -> String {
        if !self.fields.is_empty() {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message.push_str(&self.fields.join(" "));
        }
        self.message
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.push(format!("{}={:?}", field.name(), value));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }
}

// A process-wide routing slot intentionally includes Bevy worker threads. Only
// synchronous TestApp operations are attributed; detached work needs a process.
static TEST_SCOPE: Mutex<()> = Mutex::new(());
static TEST_SINK: Mutex<Option<TestCapture>> = Mutex::new(None);
thread_local! { static IN_TEST_SCOPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

#[derive(Clone)]
pub(crate) struct TestCapture {
    buffer: LogBuffer,
    filter: tracing_subscriber::filter::Targets,
    enabled: bool,
    available: bool,
}

impl TestCapture {
    pub(crate) fn buffer(&self) -> LogBuffer {
        self.buffer.clone()
    }
    pub(crate) fn new(enabled: bool, filter: &str) -> Self {
        use tracing_subscriber::prelude::*;
        static INSTALLED: OnceLock<bool> = OnceLock::new();
        let available = enabled
            && *INSTALLED.get_or_init(|| {
                let installed = tracing::subscriber::set_global_default(
                    tracing_subscriber::registry().with(TestLayer),
                )
                .is_ok();
                if installed {
                    // A pre-existing log facade logger is harmless: tracing capture
                    // still works. Never attempt to replace the caller's logger.
                    let _ = tracing_log::LogTracer::init();
                }
                installed
            });
        Self {
            buffer: LogBuffer::new(10_000),
            filter: filter
                .parse()
                .expect("invalid log_filter: use target=level directives"),
            enabled,
            available,
        }
    }

    pub(crate) fn enter(&self) -> TestCaptureGuard {
        if !self.enabled || !self.available {
            return TestCaptureGuard(None);
        }
        assert!(
            !IN_TEST_SCOPE.get(),
            "nested TestApp capture scopes are unsupported; disable capture on the nested app"
        );
        let guard = TEST_SCOPE.lock().unwrap_or_else(|e| e.into_inner());
        IN_TEST_SCOPE.set(true);
        *TEST_SINK.lock().unwrap() = Some(self.clone());
        TestCaptureGuard(Some(guard))
    }

    pub(crate) fn logs(&self) -> Vec<LogEntry> {
        assert!(self.enabled, "TestApp log capture is disabled");
        assert!(
            self.available,
            "TestApp log capture unavailable: another tracing subscriber is already installed; use a dedicated test process or disable capture"
        );
        assert_eq!(
            self.buffer.dropped(),
            0,
            "TestApp log capture overflowed; error assertions would be incomplete"
        );
        self.buffer.snapshot()
    }
}

pub(crate) struct TestCaptureGuard(Option<std::sync::MutexGuard<'static, ()>>);
impl Drop for TestCaptureGuard {
    fn drop(&mut self) {
        if self.0.is_some() {
            *TEST_SINK.lock().unwrap() = None;
            IN_TEST_SCOPE.set(false);
        }
    }
}

struct TestLayer;
impl<S: Subscriber> Layer<S> for TestLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let sink = TEST_SINK.lock().unwrap().clone();
        if let Some(sink) = sink {
            let meta = event.metadata();
            if sink.filter.would_enable(meta.target(), meta.level()) {
                let mut visitor = MessageVisitor::default();
                event.record(&mut visitor);
                sink.buffer
                    .push(meta.level().as_str(), meta.target(), visitor.finish());
            }
        }
    }
}

// ---- BRP handlers ---------------------------------------------------------

#[derive(Deserialize, Default)]
struct LogsParams {
    /// Only entries with `seq >= since`.
    #[serde(default)]
    since: Option<u64>,
    /// Minimum level: error, warn, info, debug, trace.
    #[serde(default)]
    level: Option<String>,
    /// Keep only the last `limit` matching entries (default 200).
    #[serde(default)]
    limit: Option<usize>,
    /// Substring filter on the message.
    #[serde(default)]
    contains: Option<String>,
    /// Prefix filter on the target (module path).
    #[serde(default)]
    target: Option<String>,
}

fn level_rank(level: &str) -> u8 {
    match level.to_ascii_uppercase().as_str() {
        "ERROR" => 0,
        "WARN" => 1,
        "INFO" => 2,
        "DEBUG" => 3,
        _ => 4,
    }
}

/// `harness.logs`
pub fn get_logs(In(params): In<Option<Value>>, buffer: Option<Res<LogBuffer>>) -> BrpResult {
    let p: LogsParams = parse_params(params)?;
    let buffer = buffer.ok_or_else(|| BrpError {
        code: bevy::remote::error_codes::INTERNAL_ERROR,
        message: "log capture is not enabled; set `LogPlugin { custom_layer: bevy_agent_harness::log_layer, .. }`".into(),
        data: None,
    })?;
    let max_rank = p.level.as_deref().map(level_rank).unwrap_or(4);
    let next = buffer.next_seq();
    let mut entries: Vec<LogEntry> = buffer
        .since(p.since.unwrap_or(0))
        .into_iter()
        .filter(|e| level_rank(&e.level) <= max_rank)
        .filter(|e| p.contains.as_deref().is_none_or(|s| e.message.contains(s)))
        .filter(|e| p.target.as_deref().is_none_or(|t| e.target.starts_with(t)))
        .collect();
    let matched = entries.len();
    let limit = p.limit.unwrap_or(200);
    if entries.len() > limit {
        entries.drain(..entries.len() - limit);
    }
    Ok(json!({
        "entries": entries,
        "next": next,
        "matched": matched,
        "dropped": buffer.dropped(),
    }))
}

/// `harness.logs.clear`
pub fn clear_logs(In(_): In<Option<Value>>, buffer: Option<Res<LogBuffer>>) -> BrpResult {
    if let Some(b) = buffer {
        b.clear();
    }
    Ok(Value::Null)
}
