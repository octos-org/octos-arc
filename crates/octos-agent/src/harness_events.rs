//! Structured harness event ABI and local sink transport.
//!
//! Child tools/workflows write newline-delimited JSON events to the local
//! transport URI exposed through `OCTOS_EVENT_SINK`. The runtime consumes those
//! events and folds them into durable task snapshots.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::abi_schema::{
    HARNESS_ERROR_SCHEMA_VERSION, HARNESS_PROGRESS_EVENT_SCHEMA_VERSION,
    SUB_AGENT_DISPATCH_SCHEMA_VERSION,
};
use crate::harness_errors::HarnessErrorEvent;
use crate::task_supervisor::TaskSupervisor;

pub const HARNESS_EVENT_SCHEMA_V1: &str = "octos.harness.event.v1";
pub const OCTOS_EVENT_SINK_ENV: &str = "OCTOS_EVENT_SINK";
pub const OCTOS_SESSION_ID_ENV: &str = "OCTOS_SESSION_ID";
pub const OCTOS_TASK_ID_ENV: &str = "OCTOS_TASK_ID";
pub const OCTOS_HARNESS_SESSION_ID_ENV: &str = "OCTOS_HARNESS_SESSION_ID";
pub const OCTOS_HARNESS_TASK_ID_ENV: &str = "OCTOS_HARNESS_TASK_ID";
pub const MAX_HARNESS_EVENT_LINE_BYTES: usize = 16 * 1024;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_TASK_ID_BYTES: usize = 128;
/// Maximum byte length the validator accepts for the `workflow` (pipeline id)
/// field on every event variant. A `workflow` over this bound makes
/// [`HarnessEvent::validate`] (and therefore [`write_event_to_sink`]) reject
/// the event — so producers that copy an UNBOUNDED id into `workflow` (the
/// pipeline executor's DOT graph id) MUST truncate it to this cap at the emit
/// site, or the event silently drops. Exposed so the producer references the
/// canonical limit instead of a drifting magic number (Gap 4.2 / Blocker 3).
pub const MAX_WORKFLOW_BYTES: usize = 128;
const MAX_PHASE_BYTES: usize = 64;
const MAX_MESSAGE_BYTES: usize = 2 * 1024;

fn default_sub_agent_dispatch_schema_version() -> u32 {
    SUB_AGENT_DISPATCH_SCHEMA_VERSION
}

fn default_harness_progress_event_schema_version() -> u32 {
    HARNESS_PROGRESS_EVENT_SCHEMA_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessEventError(String);

impl std::fmt::Display for HarnessEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HarnessEventError {}

type HarnessResult<T> = std::result::Result<T, HarnessEventError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessEventSinkContext {
    pub session_id: String,
    pub task_id: String,
}

static SINK_CONTEXTS: OnceLock<Mutex<HashMap<String, HarnessEventSinkContext>>> = OnceLock::new();

fn sink_contexts() -> &'static Mutex<HashMap<String, HarnessEventSinkContext>> {
    SINK_CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Blocker 3 — per-sink-path write locks. The harness-event sink is an
/// append-only NDJSON file written by MULTIPLE concurrent emitters (the spawned
/// heartbeat task + the now-more-frequent parallel/dynamic_parallel node emits,
/// plus child tools). `writeln!` is NOT a single atomic write syscall — it can
/// issue separate `write`s for the body and the newline — so concurrent writers
/// can interleave a partial line and corrupt the NDJSON stream. We serialize
/// each line into ONE buffer (incl. the trailing newline) and write it under a
/// per-path `Mutex` so a whole line is written without interleaving. The lock is
/// keyed by the resolved sink path, so two different sinks never contend.
static SINK_WRITE_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

fn sink_write_lock(path: &Path) -> Arc<Mutex<()>> {
    let registry = SINK_WRITE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(sink_lock_key(path))
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Append a single, fully-formed NDJSON line (the caller's bytes plus a
/// trailing `\n`) to `path` in ONE `write_all`, serialized against concurrent
/// writers via the per-path [`sink_write_lock`]. Both [`write_event_to_sink`]
/// and [`write_event_line_to_sink`] funnel through here so EVERY sink write is
/// atomic at the whole-line granularity.
fn append_line_atomic(path: &Path, line: &str) -> std::io::Result<()> {
    // Build the entire line (incl. newline) up front so the locked region is a
    // single `write_all` — no formatting work or extra syscalls under the lock.
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');

    let write_lock = sink_write_lock(path);
    let _guard = write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(buf.as_bytes())?;
    file.flush()
}

fn sink_path_from_raw(raw_sink: &str) -> PathBuf {
    if let Some(rest) = raw_sink.strip_prefix("file://") {
        return PathBuf::from(rest.strip_prefix("localhost").unwrap_or(rest));
    }
    PathBuf::from(raw_sink)
}

fn sink_key_from_raw(raw_sink: &str) -> String {
    sink_path_from_raw(raw_sink).display().to_string()
}

/// Key used for the sink-CONTEXT registry (session/task id lookup). This is
/// matched against [`sink_key_from_raw`] (the lookup path), so it MUST stay the
/// plain `display()` form — canonicalizing here would desync registration from
/// lookup. Lock keying uses the separate [`sink_lock_key`] (Blocker 4).
fn sink_key(path: &Path) -> String {
    path.display().to_string()
}

/// Blocker 4 — derive the per-path write-LOCK key from the CANONICAL path so
/// two lexically-different spellings of the same file (`./x` vs `/abs/x`, a
/// symlink vs its target, `a/../b` vs `b`) map to ONE lock and therefore
/// serialize against each other. Without canonicalization the lock is keyed by
/// `display()` and the two spellings get DIFFERENT locks — still racy.
///
/// This is intentionally SEPARATE from [`sink_key`] (the context-registry key,
/// which must stay `display()` to match the lookup path): the lock only needs a
/// stable per-file identity; the context registry needs registration/lookup to
/// agree on the SAME (verbatim) spelling.
///
/// `std::fs::canonicalize` requires the path to EXIST, but a sink file may not
/// exist on the first write. We degrade deterministically so the SAME target
/// always yields the SAME key regardless of which write happens first:
///   1. canonicalize the full path if it already exists;
///   2. else canonicalize the PARENT dir (usually exists) and re-join the file
///      name — this collapses symlinked/relative parents the same way for the
///      first and all subsequent writes;
///   3. else CWD-join to absolutize a relative spelling (no FS access);
///   4. else the raw `display()` string.
fn sink_lock_key(path: &Path) -> String {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon.display().to_string();
    }
    if let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) {
        // An empty parent ("x" with no dir component) canonicalizes to CWD; that
        // is still consistent for every spelling that omits a parent, so use it.
        if let Ok(canon_parent) = std::fs::canonicalize(parent) {
            return canon_parent.join(file_name).display().to_string();
        }
    }
    // No FS access succeeded — CWD-join to absolutize without touching disk so a
    // relative spelling still maps to the same key as its absolute form when the
    // CWD is stable.
    if path.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            return cwd.join(path).display().to_string();
        }
    }
    path.display().to_string()
}

fn register_sink_context(sink: String, context: HarnessEventSinkContext) {
    let mut contexts = sink_contexts().lock().unwrap_or_else(|e| e.into_inner());
    contexts.insert(sink, context);
}

/// Test-only wrapper around [`register_sink_context`] for crates that need
/// to assert against a sink without booting a full [`HarnessEventSink`].
#[doc(hidden)]
pub fn attach_event_sink_context(sink: String, context: HarnessEventSinkContext) {
    register_sink_context(sink, context);
}

/// Test-only wrapper around `unregister_sink_context`.
#[doc(hidden)]
pub fn detach_event_sink_context(sink: &str) {
    unregister_sink_context(sink);
}

fn unregister_sink_context(sink: &str) {
    let mut contexts = sink_contexts().lock().unwrap_or_else(|e| e.into_inner());
    contexts.remove(sink);
}

pub fn lookup_event_sink_context(raw_sink: impl AsRef<str>) -> Option<HarnessEventSinkContext> {
    let raw_sink = raw_sink.as_ref();
    let contexts = sink_contexts().lock().unwrap_or_else(|e| e.into_inner());
    contexts
        .get(raw_sink)
        .cloned()
        .or_else(|| contexts.get(&sink_key_from_raw(raw_sink)).cloned())
}

pub fn write_event_to_sink(raw_sink: impl AsRef<str>, event: &HarnessEvent) -> std::io::Result<()> {
    event
        .validate()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let path = sink_path_from_raw(raw_sink.as_ref());
    let json = serde_json::to_string(event)
        .map_err(|error| std::io::Error::other(format!("serialize harness event: {error}")))?;
    // Blocker 3 — one whole line, one `write_all`, under the per-path lock so
    // concurrent emitters (heartbeat + parallel node emits) cannot interleave.
    append_line_atomic(&path, &json)
}

/// Append a pre-serialized event line to a sink without round-tripping
/// through the [`HarnessEvent`] validator.
///
/// Used by the plugin protocol-v2 shim, which builds events from the wire
/// format on a hot reader path. Callers MUST pass a single well-formed
/// JSON object; the writer adds a trailing newline.
pub fn write_event_line_to_sink(raw_sink: impl AsRef<str>, line: &str) -> std::io::Result<()> {
    let path = sink_path_from_raw(raw_sink.as_ref());
    // Blocker 3 — same atomic whole-line append as `write_event_to_sink`.
    append_line_atomic(&path, line)
}

pub fn emit_registered_progress_event(
    raw_sink: impl AsRef<str>,
    workflow: Option<&str>,
    phase: &str,
    message: &str,
    progress: Option<f64>,
) -> bool {
    let raw_sink = raw_sink.as_ref();
    let Some(context) = lookup_event_sink_context(raw_sink) else {
        return false;
    };
    let event = HarnessEvent::progress(
        context.session_id,
        context.task_id,
        workflow.map(ToOwned::to_owned),
        phase.to_string(),
        Some(message.to_string()),
        progress,
    );
    write_event_to_sink(raw_sink, &event).is_ok()
}

/// Emit a `Progress` event carrying additive structured `extra` fields to a
/// registered sink (Gap 4.2). Same lookup/write path as
/// [`emit_registered_progress_event`] but threads the structured
/// node/eta/preview map through [`HarnessEvent::progress_with_extra`].
/// Returns `true` when the sink accepted the write.
pub fn emit_registered_progress_event_with_extra(
    raw_sink: impl AsRef<str>,
    workflow: Option<&str>,
    phase: &str,
    message: &str,
    progress: Option<f64>,
    extra: HashMap<String, Value>,
) -> bool {
    let raw_sink = raw_sink.as_ref();
    let Some(context) = lookup_event_sink_context(raw_sink) else {
        return false;
    };
    let event = HarnessEvent::progress_with_extra(
        context.session_id,
        context.task_id,
        workflow.map(ToOwned::to_owned),
        phase.to_string(),
        Some(message.to_string()),
        progress,
        extra,
    );
    write_event_to_sink(raw_sink, &event).is_ok()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessEvent {
    pub schema: String,
    #[serde(flatten)]
    pub payload: HarnessEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessEventPayload {
    Progress {
        #[serde(flatten)]
        data: HarnessProgressEvent,
    },
    Phase {
        #[serde(flatten)]
        data: HarnessPhaseEvent,
    },
    Retry {
        #[serde(flatten)]
        data: HarnessRetryEvent,
    },
    Failure {
        #[serde(flatten)]
        data: HarnessFailureEvent,
    },
    SubAgentDispatch {
        #[serde(flatten)]
        data: HarnessSubAgentDispatchEvent,
    },
    /// Periodic progress summary emitted by the `AgentSummaryGenerator`
    /// (M8.7). Produced every `tick` seconds while a spawn_only sub-agent
    /// is running, backed by a cheap-lane LLM call over the last N
    /// activities. The supervisor folds these into
    /// `BackgroundTask.runtime_detail`.
    SubagentProgress {
        #[serde(flatten)]
        data: HarnessSubagentProgressEvent,
    },
    Error {
        #[serde(flatten)]
        data: HarnessErrorEvent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessProgressEvent {
    #[serde(default = "default_harness_progress_event_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(
        default,
        alias = "progress_fraction",
        skip_serializing_if = "Option::is_none"
    )]
    pub progress: Option<f64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessPhaseEvent {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessRetryEvent {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessFailureEvent {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Typed payload emitted when the harness dispatches a task to an
/// MCP-backed sub-agent. The schema is versioned so downstream tooling
/// can reject unknown variants instead of silently dropping fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessSubAgentDispatchEvent {
    #[serde(default = "default_sub_agent_dispatch_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Stable backend label: `"local"` (stdio subprocess) or `"remote"`
    /// (HTTPS).
    pub backend: String,
    /// Human-readable endpoint identifier (command or URL).
    pub endpoint: String,
    /// Outcome label from [`crate::tools::mcp_agent::DispatchOutcome`].
    pub outcome: String,
    /// Optional error text for non-success outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Periodic sub-agent progress summary event (M8.7).
///
/// Emitted every `tick` seconds by `AgentSummaryGenerator` while a
/// spawn_only sub-agent is in `Running` status. Supervisors fold the
/// `summary` string into `BackgroundTask.runtime_detail` so dashboards
/// can display live "what is the sub-agent doing" text without tailing
/// the per-task disk output log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessSubagentProgressEvent {
    pub session_id: String,
    pub task_id: String,
    /// Short LLM-generated description of the current activity (3-5 words,
    /// present continuous tense).
    pub summary: String,
    /// Monotonic tick sequence starting at 1 for the first summary of the
    /// task. Useful for clients that want to deduplicate or show "tick N".
    pub tick_seq: u32,
    /// When the summary was produced (wall-clock, UTC).
    pub at: chrono::DateTime<chrono::Utc>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl HarnessEvent {
    pub fn progress(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<impl Into<String>>,
        phase: impl Into<String>,
        message: Option<impl Into<String>>,
        progress: Option<f64>,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::Progress {
                data: HarnessProgressEvent {
                    schema_version: HARNESS_PROGRESS_EVENT_SCHEMA_VERSION,
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    workflow: workflow.map(Into::into),
                    phase: phase.into(),
                    message: message.map(Into::into),
                    progress,
                    extra: HashMap::new(),
                },
            },
        }
    }

    /// Build a `Progress` event carrying additive structured fields in the
    /// flattened `extra` map (Gap 4.2). The canonical `phase`/`message`/
    /// `progress` keep working for consumers that ignore `extra`; producers
    /// (e.g. the pipeline executor) attach structured per-node fields —
    /// `node`, `node_index`, `node_total`, `eta_secs`, `preview` — so the
    /// SPA/TUI can render real per-node progress instead of an opaque chip.
    ///
    /// `extra` is purely additive on the v1 wire (a HashMap flatten that
    /// round-trips); no schema-version bump is needed and consumers that
    /// don't read the keys are unaffected.
    pub fn progress_with_extra(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<impl Into<String>>,
        phase: impl Into<String>,
        message: Option<impl Into<String>>,
        progress: Option<f64>,
        extra: HashMap<String, Value>,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::Progress {
                data: HarnessProgressEvent {
                    schema_version: HARNESS_PROGRESS_EVENT_SCHEMA_VERSION,
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    workflow: workflow.map(Into::into),
                    phase: phase.into(),
                    message: message.map(Into::into),
                    progress,
                    extra,
                },
            },
        }
    }

    /// Convenience builder for a `SubAgentDispatch` event. Takes a
    /// pre-populated [`HarnessSubAgentDispatchEvent`] so callers pay
    /// the construction cost once and this helper stays below clippy's
    /// argument limit.
    pub fn sub_agent_dispatch(data: HarnessSubAgentDispatchEvent) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::SubAgentDispatch { data },
        }
    }

    pub fn phase_event(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<impl Into<String>>,
        phase: impl Into<String>,
        message: Option<impl Into<String>>,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::Phase {
                data: HarnessPhaseEvent {
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    workflow: workflow.map(Into::into),
                    phase: phase.into(),
                    message: message.map(Into::into),
                    extra: HashMap::new(),
                },
            },
        }
    }

    /// Build a `SubagentProgress` event (M8.7). Emitted every tick while a
    /// spawn_only sub-agent is running so operators can see a live
    /// natural-language summary of current activity.
    pub fn subagent_progress(
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        summary: impl Into<String>,
        tick_seq: u32,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::SubagentProgress {
                data: HarnessSubagentProgressEvent {
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    summary: summary.into(),
                    tick_seq,
                    at,
                    extra: HashMap::new(),
                },
            },
        }
    }

    pub fn from_json_line(line: &str) -> HarnessResult<Self> {
        if line.len() > MAX_HARNESS_EVENT_LINE_BYTES {
            return Err(HarnessEventError(format!(
                "harness event line exceeded {MAX_HARNESS_EVENT_LINE_BYTES} bytes"
            )));
        }

        let event: Self = serde_json::from_str(line)
            .map_err(|error| HarnessEventError(format!("invalid harness event JSON: {error}")))?;
        event.validate()?;
        Ok(event)
    }

    pub fn validate(&self) -> HarnessResult<()> {
        if self.schema != HARNESS_EVENT_SCHEMA_V1 {
            return Err(HarnessEventError(format!(
                "unsupported harness event schema: {}",
                self.schema
            )));
        }

        match &self.payload {
            HarnessEventPayload::Progress { data } => {
                if data.schema_version > HARNESS_PROGRESS_EVENT_SCHEMA_VERSION {
                    return Err(HarnessEventError(format!(
                        "unsupported harness progress schema_version {} (max supported: {})",
                        data.schema_version, HARNESS_PROGRESS_EVENT_SCHEMA_VERSION
                    )));
                }
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_phase(&data.phase)?;
                validate_optional_message(data.message.as_deref())?;
                validate_progress(data.progress)?;
            }
            HarnessEventPayload::Phase { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_phase(&data.phase)?;
                validate_optional_message(data.message.as_deref())?;
            }
            HarnessEventPayload::Retry { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_optional_message(data.message.as_deref())?;
            }
            HarnessEventPayload::Failure { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("failure message", &data.message, MAX_MESSAGE_BYTES)?;
            }
            HarnessEventPayload::SubAgentDispatch { data } => {
                if data.schema_version > SUB_AGENT_DISPATCH_SCHEMA_VERSION {
                    return Err(HarnessEventError(format!(
                        "unsupported sub-agent dispatch schema_version {} (max supported: {})",
                        data.schema_version, SUB_AGENT_DISPATCH_SCHEMA_VERSION
                    )));
                }
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("sub-agent backend", &data.backend, MAX_MESSAGE_BYTES)?;
                validate_bounded("sub-agent endpoint", &data.endpoint, MAX_MESSAGE_BYTES)?;
                validate_bounded("sub-agent outcome", &data.outcome, MAX_MESSAGE_BYTES)?;
                validate_optional_message(data.message.as_deref())?;
            }
            HarnessEventPayload::SubagentProgress { data } => {
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_bounded("summary", &data.summary, MAX_MESSAGE_BYTES)?;
            }
            HarnessEventPayload::Error { data } => {
                if data.schema_version > HARNESS_ERROR_SCHEMA_VERSION {
                    return Err(HarnessEventError(format!(
                        "unsupported harness error schema_version {} (max supported: {})",
                        data.schema_version, HARNESS_ERROR_SCHEMA_VERSION
                    )));
                }
                validate_common_ids(&data.session_id, &data.task_id)?;
                validate_optional_name("workflow", data.workflow.as_deref(), MAX_WORKFLOW_BYTES)?;
                validate_optional_name("phase", data.phase.as_deref(), MAX_PHASE_BYTES)?;
                validate_bounded("variant", &data.variant, MAX_PHASE_BYTES)?;
                validate_bounded("recovery", &data.recovery, MAX_PHASE_BYTES)?;
                validate_bounded("error message", &data.message, MAX_MESSAGE_BYTES)?;
            }
        }

        Ok(())
    }

    pub fn runtime_detail_value(
        &self,
        fallback_workflow_kind: Option<&str>,
        fallback_current_phase: Option<&str>,
    ) -> Value {
        match &self.payload {
            HarnessEventPayload::Progress { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = Some(data.phase.as_str()).or(fallback_current_phase);
                let message = data.message.as_deref();
                let mut detail = serde_json::json!({
                    "schema": self.schema,
                    "schema_version": data.schema_version,
                    "kind": "progress",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "message": message,
                    "progress_message": message,
                    "progress": data.progress,
                });
                // Gap 4.2 — additively surface the structured `extra` fields
                // (node/node_index/node_total/eta_secs/preview) so consumers
                // can render real per-node progress. Canonical typed keys win:
                // a producer can never clobber `progress`/`kind`/etc. by
                // stuffing them into `extra`.
                if !data.extra.is_empty() {
                    if let Some(obj) = detail.as_object_mut() {
                        for (k, v) in &data.extra {
                            obj.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                }
                detail
            }
            HarnessEventPayload::Phase { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = Some(data.phase.as_str()).or(fallback_current_phase);
                let message = data.message.as_deref();
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "phase",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "message": message,
                    "progress_message": message,
                })
            }
            HarnessEventPayload::Retry { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "retry",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "attempt": data.attempt,
                    "message": data.message,
                })
            }
            HarnessEventPayload::Failure { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "failure",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "message": data.message,
                    "retryable": data.retryable,
                })
            }
            HarnessEventPayload::SubAgentDispatch { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "schema_version": data.schema_version,
                    "kind": "sub_agent_dispatch",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "backend": data.backend,
                    "endpoint": data.endpoint,
                    "outcome": data.outcome,
                    "message": data.message,
                })
            }
            HarnessEventPayload::SubagentProgress { data } => {
                serde_json::json!({
                    "schema": self.schema,
                    "kind": "subagent_progress",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "summary": data.summary,
                    "tick": data.tick_seq,
                    "at": data.at.to_rfc3339(),
                    "workflow_kind": fallback_workflow_kind,
                    "current_phase": fallback_current_phase,
                })
            }
            HarnessEventPayload::Error { data } => {
                let workflow = data.workflow.as_deref().or(fallback_workflow_kind);
                let current_phase = data.phase.as_deref().or(fallback_current_phase);
                serde_json::json!({
                    "schema": self.schema,
                    "schema_version": data.schema_version,
                    "kind": "error",
                    "session_id": data.session_id,
                    "task_id": data.task_id,
                    "workflow": workflow,
                    "workflow_kind": workflow,
                    "phase": data.phase,
                    "current_phase": current_phase,
                    "variant": data.variant,
                    "recovery": data.recovery,
                    "message": data.message,
                    "details": data.details,
                })
            }
        }
    }

    pub fn session_id(&self) -> &str {
        match &self.payload {
            HarnessEventPayload::Progress { data } => &data.session_id,
            HarnessEventPayload::Phase { data } => &data.session_id,
            HarnessEventPayload::Retry { data } => &data.session_id,
            HarnessEventPayload::Failure { data } => &data.session_id,
            HarnessEventPayload::SubAgentDispatch { data } => &data.session_id,
            HarnessEventPayload::SubagentProgress { data } => &data.session_id,
            HarnessEventPayload::Error { data } => &data.session_id,
        }
    }

    pub fn task_id(&self) -> &str {
        match &self.payload {
            HarnessEventPayload::Progress { data } => &data.task_id,
            HarnessEventPayload::Phase { data } => &data.task_id,
            HarnessEventPayload::Retry { data } => &data.task_id,
            HarnessEventPayload::Failure { data } => &data.task_id,
            HarnessEventPayload::SubAgentDispatch { data } => &data.task_id,
            HarnessEventPayload::SubagentProgress { data } => &data.task_id,
            HarnessEventPayload::Error { data } => &data.task_id,
        }
    }

    pub fn workflow(&self) -> Option<&str> {
        match &self.payload {
            HarnessEventPayload::Progress { data } => data.workflow.as_deref(),
            HarnessEventPayload::Phase { data } => data.workflow.as_deref(),
            HarnessEventPayload::Retry { data } => data.workflow.as_deref(),
            HarnessEventPayload::Failure { data } => data.workflow.as_deref(),
            HarnessEventPayload::SubAgentDispatch { data } => data.workflow.as_deref(),
            HarnessEventPayload::SubagentProgress { .. } => None,
            HarnessEventPayload::Error { data } => data.workflow.as_deref(),
        }
    }

    pub fn phase(&self) -> Option<&str> {
        match &self.payload {
            HarnessEventPayload::Progress { data } => Some(data.phase.as_str()),
            HarnessEventPayload::Phase { data } => Some(data.phase.as_str()),
            HarnessEventPayload::Retry { data } => data.phase.as_deref(),
            HarnessEventPayload::Failure { data } => data.phase.as_deref(),
            HarnessEventPayload::SubAgentDispatch { data } => data.phase.as_deref(),
            HarnessEventPayload::SubagentProgress { .. } => None,
            HarnessEventPayload::Error { data } => data.phase.as_deref(),
        }
    }
}

fn validate_common_ids(session_id: &str, task_id: &str) -> HarnessResult<()> {
    validate_bounded("session_id", session_id, MAX_SESSION_ID_BYTES)?;
    validate_bounded("task_id", task_id, MAX_TASK_ID_BYTES)?;
    Ok(())
}

fn validate_optional_name(
    field: &'static str,
    value: Option<&str>,
    max: usize,
) -> HarnessResult<()> {
    if let Some(value) = value {
        validate_bounded(field, value, max)?;
    }
    Ok(())
}

fn validate_phase(phase: &str) -> HarnessResult<()> {
    validate_bounded("phase", phase, MAX_PHASE_BYTES)?;
    if !is_valid_phase_name(phase) {
        return Err(HarnessEventError(format!(
            "invalid phase name '{phase}': expected snake_case"
        )));
    }
    Ok(())
}

fn validate_optional_message(message: Option<&str>) -> HarnessResult<()> {
    if let Some(message) = message {
        validate_bounded("message", message, MAX_MESSAGE_BYTES)?;
    }
    Ok(())
}

fn validate_progress(progress: Option<f64>) -> HarnessResult<()> {
    if let Some(progress) = progress {
        if !(0.0..=1.0).contains(&progress) {
            return Err(HarnessEventError(format!(
                "progress must be between 0.0 and 1.0, got {progress}"
            )));
        }
    }
    Ok(())
}

fn validate_bounded(field: &'static str, value: &str, max: usize) -> HarnessResult<()> {
    if value.is_empty() {
        return Err(HarnessEventError(format!("{field} cannot be empty")));
    }
    if value.len() > max {
        return Err(HarnessEventError(format!("{field} exceeded {max} bytes")));
    }
    Ok(())
}

fn is_valid_phase_name(phase: &str) -> bool {
    let mut chars = phase.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

/// Local sink that feeds structured child events into a task supervisor.
pub struct HarnessEventSink {
    sink_file: tempfile::NamedTempFile,
    sink_key: String,
    stop: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}

impl HarnessEventSink {
    pub fn new(
        task_supervisor: Arc<TaskSupervisor>,
        task_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> std::io::Result<Self> {
        let sink_file = tempfile::NamedTempFile::new()?;
        let path = sink_file.path().to_path_buf();
        let sink_key = sink_key(&path);
        let task_id = task_id.into();
        let session_id = session_id.into();
        register_sink_context(
            sink_key.clone(),
            HarnessEventSinkContext {
                session_id: session_id.clone(),
                task_id: task_id.clone(),
            },
        );
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();

        let reader = tokio::spawn(run_reader(
            path,
            task_supervisor,
            task_id,
            session_id,
            reader_stop,
        ));

        Ok(Self {
            sink_file,
            sink_key,
            stop,
            reader,
        })
    }

    pub fn path(&self) -> &Path {
        self.sink_file.path()
    }

    /// Return the transport URI child processes should receive in OCTOS_EVENT_SINK.
    pub fn uri(&self) -> String {
        format!("file://{}", self.path().display())
    }
}

impl Drop for HarnessEventSink {
    fn drop(&mut self) {
        unregister_sink_context(&self.sink_key);
        self.stop.store(true, Ordering::Release);
        self.reader.abort();
    }
}

async fn run_reader(
    path: PathBuf,
    task_supervisor: Arc<TaskSupervisor>,
    task_id: String,
    session_id: String,
    stop: Arc<AtomicBool>,
) {
    let mut file = loop {
        match tokio::fs::OpenOptions::new().read(true).open(&path).await {
            Ok(file) => break file,
            Err(error) => {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                warn!(path = %path.display(), error = %error, "failed to open harness event sink");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    };

    let mut carry = Vec::new();
    let mut chunk = vec![0_u8; 4096];

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        let read = match file.read(&mut chunk).await {
            Ok(read) => read,
            Err(error) => {
                warn!(path = %path.display(), error = %error, "failed to read harness event sink");
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
        };

        if read == 0 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            continue;
        }

        carry.extend_from_slice(&chunk[..read]);
        while let Some(pos) = carry.iter().position(|byte| *byte == b'\n') {
            let mut line = carry.drain(..=pos).collect::<Vec<u8>>();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > MAX_HARNESS_EVENT_LINE_BYTES {
                warn!(
                    path = %path.display(),
                    task_id = %task_id,
                    "dropping oversized harness event line"
                );
                continue;
            }

            let Ok(line) = String::from_utf8(line) else {
                warn!(
                    path = %path.display(),
                    task_id = %task_id,
                    "dropping non-utf8 harness event line"
                );
                continue;
            };

            let Ok(event) = HarnessEvent::from_json_line(&line) else {
                warn!(
                    path = %path.display(),
                    task_id = %task_id,
                    "dropping invalid harness event line"
                );
                continue;
            };

            if event.session_id() != session_id || event.task_id() != task_id {
                warn!(
                    path = %path.display(),
                    task_id = %task_id,
                    session_id = %session_id,
                    "ignoring harness event for unexpected task/session"
                );
                continue;
            }

            if let Err(error) = task_supervisor.apply_harness_event(&task_id, &event) {
                warn!(
                    path = %path.display(),
                    task_id = %task_id,
                    error = %error,
                    "failed to apply harness event"
                );
            }
        }

        if carry.len() > MAX_HARNESS_EVENT_LINE_BYTES {
            warn!(
                path = %path.display(),
                task_id = %task_id,
                "discarding partial oversized harness event"
            );
            carry.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_event_round_trips_and_keeps_schema() {
        let event = HarnessEvent::progress(
            "session-1",
            "task-1",
            Some("bg_research"),
            "fetching_sources",
            Some("Fetching source 3/12"),
            Some(0.42),
        );

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""schema":"octos.harness.event.v1""#));
        assert!(json.contains(r#""kind":"progress""#));

        let parsed = HarnessEvent::from_json_line(&json).unwrap();
        assert_eq!(parsed.schema, HARNESS_EVENT_SCHEMA_V1);
        assert_eq!(parsed.session_id(), "session-1");
        assert_eq!(parsed.task_id(), "task-1");
        assert_eq!(parsed.workflow(), Some("bg_research"));
        assert_eq!(parsed.phase(), Some("fetching_sources"));

        let detail = parsed.runtime_detail_value(None, None);
        assert_eq!(detail["workflow_kind"], "bg_research");
        assert_eq!(detail["current_phase"], "fetching_sources");
        assert_eq!(detail["progress_message"], "Fetching source 3/12");
    }

    #[test]
    fn progress_with_extra_surfaces_structured_fields_in_runtime_detail() {
        // Gap 4.2 — producer-side structured per-node progress. The pipeline
        // executor needs to attach node-index/eta/preview as structured fields
        // (not buried in the message string) so existing consumers can render
        // them. They ride the additive `extra` map; `runtime_detail_value`
        // must surface them so the SPA/TUI see them via `BackgroundTask
        // .runtime_detail`.
        let mut extra = HashMap::new();
        extra.insert("node".to_string(), Value::String("analyze".into()));
        extra.insert("node_index".to_string(), Value::from(2));
        extra.insert("node_total".to_string(), Value::from(3));
        extra.insert("eta_secs".to_string(), Value::from(45));
        extra.insert(
            "preview".to_string(),
            Value::String("partial output…".into()),
        );

        let event = HarnessEvent::progress_with_extra(
            "session-1",
            "task-1",
            Some("research"),
            "node_completed",
            Some("analyze (2 of 3)"),
            Some(0.66),
            extra,
        );

        // Round-trips on the wire (extra is flattened, so it survives).
        let json = serde_json::to_string(&event).unwrap();
        let parsed = HarnessEvent::from_json_line(&json).unwrap();
        match &parsed.payload {
            HarnessEventPayload::Progress { data } => {
                assert_eq!(data.extra["node"], Value::String("analyze".into()));
                assert_eq!(data.extra["node_index"], Value::from(2));
            }
            other => panic!("expected Progress, got {other:?}"),
        }

        // Consumers read runtime_detail — the structured fields must be there.
        let detail = parsed.runtime_detail_value(Some("research"), None);
        assert_eq!(detail["progress_message"], "analyze (2 of 3)");
        assert_eq!(detail["node"], "analyze");
        assert_eq!(detail["node_index"], 2);
        assert_eq!(detail["node_total"], 3);
        assert_eq!(detail["eta_secs"], 45);
        assert_eq!(detail["preview"], "partial output…");
        // Backward-compat: the canonical progress keys must still be present so
        // consumers that ignore `extra` keep working.
        assert_eq!(detail["kind"], "progress");
        assert_eq!(detail["workflow_kind"], "research");
        let progress = detail["progress"].as_f64().unwrap();
        assert!((progress - 0.66).abs() < 0.0001);
    }

    #[test]
    fn progress_with_extra_does_not_let_extra_clobber_canonical_keys() {
        // Defense-in-depth: a producer that accidentally stuffs a reserved key
        // (e.g. "progress") into `extra` must not overwrite the typed
        // canonical value in runtime_detail — the typed fields win.
        let mut extra = HashMap::new();
        extra.insert("progress".to_string(), Value::from(0.99));
        extra.insert("kind".to_string(), Value::String("hijack".into()));
        extra.insert("node".to_string(), Value::String("plan".into()));

        let event = HarnessEvent::progress_with_extra(
            "s",
            "t",
            Some("research"),
            "node_started",
            Some("plan (1 of 3)"),
            Some(0.0),
            extra,
        );
        let detail = event.runtime_detail_value(None, None);
        // Canonical typed values survive; only the genuinely-new key lands.
        assert_eq!(detail["kind"], "progress");
        assert_eq!(detail["progress"], 0.0);
        assert_eq!(detail["node"], "plan");
    }

    #[test]
    fn ignores_unknown_future_fields() {
        let mut json = serde_json::to_value(HarnessEvent::phase_event(
            "s",
            "t",
            Some("demo"),
            "running",
            Some("phase changed"),
        ))
        .unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("future_field".into(), Value::String("ok".into()));
        let parsed = HarnessEvent::from_json_line(&json.to_string()).unwrap();

        assert_eq!(parsed.workflow(), Some("demo"));
        assert_eq!(parsed.phase(), Some("running"));
    }

    #[test]
    fn progress_event_defaults_and_rejects_future_schema_version() {
        let legacy = serde_json::json!({
            "schema": "octos.harness.event.v1",
            "kind": "progress",
            "session_id": "session-1",
            "task_id": "task-1",
            "workflow": "bg_research",
            "phase": "fetch",
            "message": "Fetching",
            "progress": 0.4
        });

        let parsed = HarnessEvent::from_json_line(&legacy.to_string()).unwrap();
        match &parsed.payload {
            HarnessEventPayload::Progress { data } => {
                assert_eq!(data.schema_version, HARNESS_PROGRESS_EVENT_SCHEMA_VERSION);
            }
            other => panic!("expected Progress, got {other:?}"),
        }
        assert_eq!(
            parsed.runtime_detail_value(None, None)["schema_version"],
            serde_json::json!(HARNESS_PROGRESS_EVENT_SCHEMA_VERSION)
        );

        let future = serde_json::json!({
            "schema": "octos.harness.event.v1",
            "schema_version": HARNESS_PROGRESS_EVENT_SCHEMA_VERSION + 1,
            "kind": "progress",
            "session_id": "session-1",
            "task_id": "task-1",
            "phase": "fetch"
        });
        assert!(HarnessEvent::from_json_line(&future.to_string()).is_err());
    }

    #[test]
    fn subagent_progress_event_integrates_with_supervisor() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("search", "call-1", Some("api:session"));
        supervisor.mark_running(&task_id);

        let event = HarnessEvent::subagent_progress(
            "api:session",
            task_id.clone(),
            "parsing response",
            3,
            chrono::Utc::now(),
        );
        supervisor.apply_harness_event(&task_id, &event).unwrap();

        let task = supervisor.get_task(&task_id).expect("task missing");
        let detail: serde_json::Value =
            serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["kind"], "subagent_progress");
        assert_eq!(detail["summary"], "parsing response");
        assert_eq!(detail["tick"], 3);
        // The coarse status must remain Running — progress ticks are purely
        // observational.
        assert_eq!(task.status, crate::task_supervisor::TaskStatus::Running);
    }

    #[test]
    fn rejects_oversized_fields_and_invalid_phases() {
        let oversized = HarnessEvent::progress(
            "session-1",
            "task-1",
            Some("bg_research"),
            "fetching_sources",
            Some("x".repeat(MAX_MESSAGE_BYTES + 1)),
            Some(0.42),
        );
        assert!(oversized.validate().is_err());

        let invalid_phase = HarnessEvent::progress(
            "session-1",
            "task-1",
            Some("bg_research"),
            "FetchSources",
            Some("ok"),
            Some(0.42),
        );
        assert!(invalid_phase.validate().is_err());
    }
}
