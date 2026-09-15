//! UI Protocol v1 WebSocket transport.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex as StdMutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use octos_agent::{
    Agent, BackgroundResultKind, BackgroundResultPayload, PromptContextManager, PromptContextPhase,
    PromptContextReport, PromptContextRequest, ToolApprovalDecision, ToolApprovalRequest,
    UserQuestionOutcome, UserQuestionRequest,
};
use octos_core::app_ui_codec::{self, AppUiFrame, MAX_TEXT_FRAME_BYTES};
use octos_core::ui_protocol::{
    ApprovalAutoResolvedEvent, ApprovalCancelledEvent, ApprovalCommandDetails,
    ApprovalDecidedEvent, ApprovalDecision, ApprovalId, ApprovalRenderHints,
    ApprovalRequestedEvent, ApprovalTypedDetails, AttachmentOwnerV2,
    ContextCompactionCompletedEvent, ContextCompactionStartedEvent,
    ContextNormalizationReportedEvent, EnvelopeTokenUsage, EnvelopeV2, EnvelopeV2Notification,
    FileRef, HydratedMessage, HydratedTurn, InputItem, MessageDeltaEvent, MessageMeta,
    OutputCursor, Payload, PayloadV2, ReplayLossyEvent, RpcError, RpcErrorResponse, RpcRequest,
    RpcResponse, SESSION_HYDRATE_INCLUDE_MAX, SessionBtwParams, SessionHydrateParams,
    SessionHydrateResult, SessionOpenParams, SessionOpenResult, SessionOpened,
    SessionRollbackParams, SessionRollbackResult, TaskOutputDeltaEvent, ThreadGraphEntry,
    ThreadGraphGetParams, ThreadGraphGetResult, TurnCompletedEvent, TurnErrorEvent,
    TurnErrorPartialResult, TurnId, TurnInterruptParams, TurnInterruptResult, TurnLifecycleState,
    TurnSessionResult, TurnStartParams, TurnStateGetParams, TurnStateGetResult, TurnTerminalError,
    TurnTerminalOutcome, UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1,
    UI_PROTOCOL_FEATURE_BACKGROUND_ACTIVITY_V1, UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1,
    UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1, UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1,
    UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1, UI_PROTOCOL_FEATURE_FILE_ATTACHED_V1,
    UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1, UI_PROTOCOL_FEATURE_PLAN_TODOS_V1,
    UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1, UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2,
    UI_PROTOCOL_FEATURE_REVIEW_START_V1, UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1,
    UI_PROTOCOL_FEATURE_SESSION_SANDBOX_V1, UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
    UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1, UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1,
    UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1, UI_PROTOCOL_FEATURE_TURN_STEER_DROPPED_V1,
    UI_PROTOCOL_FEATURE_USER_QUESTION_V1, UiArtifactPaneItem, UiArtifactPaneSnapshot, UiCommand,
    UiContextCompactionRecord, UiContextNormalizationReport, UiContextState, UiCursor,
    UiFileMutationNotice, UiGitHistoryItem, UiGitPaneSnapshot, UiGitStatusItem, UiNotification,
    UiPaneSnapshot, UiPaneSnapshotLimitation, UiProtocolCapabilities, UiRpcResult,
    UiWorkspacePaneEntry, UiWorkspacePaneSnapshot, UnsupportedCapabilityReport,
    UserQuestionRequestedEvent, UserQuestionRespondParams, approval_cancelled_reasons,
    approval_kinds, hydrate_sections, thread_status,
};

use octos_core::{
    AgentId, InboundMessage, MAIN_PROFILE_ID, Message, MessageRole, SessionKey, TaskId,
};
use octos_llm::pricing::model_pricing;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::{Mutex as TokioMutex, mpsc, oneshot};
use tokio::task::AbortHandle;
use tracing::{Instrument, debug, info, warn};

/// Wire-frame envelope (was `axum::extract::ws::Message`). The stdio writer
/// treats `Text` as the only payload-bearing variant; `Close` preserves the
/// close-code semantics for lifecycle/auth failures, and the control variants
/// exist so legacy frame matches stay exhaustive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WsMessage {
    Text(String),
    Close(Option<CloseFrame>),
}

/// Payload of [`WsMessage::Close`] (was `axum::extract::ws::CloseFrame`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloseFrame {
    pub code: u16,
    pub reason: std::sync::Arc<str>,
}

impl WsMessage {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        WsMessage::Text(text.into())
    }
}

use super::AppState;
use super::ui_protocol_audit::log_decision_tracing;
use super::ui_protocol_ledger::{
    ConnectionId, LedgerConfig, LedgeredUiProtocolEvent, UiProtocolLedger, UiProtocolLedgerEvent,
    spawn_eviction_task,
};
use super::ui_protocol_progress::{
    ProgressMappingContext, UiProgressMapping, background_task_to_progress_json, map_progress_json,
    replay_task_updated_notification,
};
use super::ui_protocol_sanitize::sanitize_display_path;
use super::ui_protocol_scope::{ApprovalScopeKind, match_key_for};
use super::ws_slash;
// Phase 3 (goal-in-chat): the contract stores now live outside the `api` gate
// so `octos chat --peers` shares the identical process-global registry.
use crate::contracts::approvals::PendingApprovalStore;
use crate::contracts::diff::PendingDiffPreviewStore;
use crate::contracts::questions::PendingQuestionStore;
use crate::contracts::scope::ScopePolicy;
use crate::contracts::{UiProtocolContractStores, contract_stores};
// Phase 3 (goal-in-chat): the peer staging / addressing / parked-prompt layer
// moved VERBATIM to the non-`api` `crate::peers` so `octos chat --peers` can
// host peers in-process against the same process-global wire registry. Glob so
// every existing call site in this module (and in `ui_protocol_tests.rs`, which
// does `use super::*`) keeps resolving unchanged.
use crate::context_manager::{
    CompactContextPolicy, ContextCompactionBudgetOutcome, ContextCompactionRecord,
    ContextCompactionStatus, ContextEventKind, ContextManager, ForkPolicy, PromptBuildPolicy,
    PromptFrame, load_context_manager_snapshot, load_or_rebuild_context_manager,
    persist_context_manager_snapshot,
};
use crate::usage_ledger::{
    PersistentUsageLedger, USAGE_LEDGER_FILE, UsageCostSource, UsageEvent, UsageTotals,
};

const MAX_DIFF_PREVIEW_BYTES: usize = 256 * 1024;
const PROGRESS_CHANNEL_CAPACITY: usize = 1024;
const APPUI_CONTEXT_COMPACT_RATIO_NUMERATOR: usize = 7;
const APPUI_CONTEXT_COMPACT_RATIO_DENOMINATOR: usize = 10;
const APPUI_CONTEXT_COMPACT_TARGET_NUMERATOR: usize = 2;
const APPUI_CONTEXT_COMPACT_TARGET_DENOMINATOR: usize = 3;
/// Wall-clock budget for delivering a *terminal* task lifecycle update
/// (`completed` / `failed` / `cancelled`) when the bounded progress
/// channel is full. Long enough that real WebSocket backpressure can
/// drain (UI repaint, network blip), short enough that we don't pile up
/// zombie sends if the consumer is permanently gone. See
/// `forward_task_progress_to_channel` for the durability contract.
const TERMINAL_TASK_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Per-session ring buffer cap. Bumped from 1024 (M9.6 default) to
/// 4096 in M9-FIX-05 — a tool-heavy turn was clipping the start of the
/// current turn from replay. Disk log is now the source of truth, so
/// this is the LRU hot-cache size, not the durable retention.
const EVENT_LEDGER_RETAINED_PER_SESSION: usize = 4096;
/// Spec §10 `unknown_turn` (M9-FIX-02 wires this into `RpcError::unknown_turn`).
/// Until that lands in the trunk this worktree is rebased on, we keep a local
/// constant so the wire code stays correct. TODO: link to M9-FIX-02 once merged.
const UNKNOWN_TURN_CODE: i64 = -32101;
/// Maximum time we wait for the turn task to acknowledge an interrupt before
/// returning `ack_timeout` to the caller.
const INTERRUPT_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Per-connection bounded channel for outgoing WS frames. Decouples send
/// callers from the actual socket so a slow client cannot wedge unrelated
/// traffic. Tunable per session size.
const WS_WRITER_CHANNEL_CAPACITY: usize = 1024;
const APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED: &str = "request_send_failed";
/// Reason recorded when a pending APPROVAL entry is cancelled because the
/// approval-requesting tool's waiting future was dropped (turn interrupt/abort,
/// per-tool timeout, panic, connection close) before a client decision arrived
/// — the approval-store analogue of [`USER_QUESTION_CANCELLED_REASON_WAITER_DROPPED`].
/// This is what gives a kept-pending approval (a `Closed`/`FatalClosed` direct
/// send that waits for reconnect/replay, #1449) a guaranteed release path
/// instead of hanging the turn forever when no client ever answers.
const APPROVAL_CANCELLED_REASON_WAITER_DROPPED: &str = "waiter_dropped";
/// Reason recorded when a pending structured-USER-QUESTION entry is cancelled
/// because the `ask_user_question` tool's waiting future was dropped (turn
/// interrupt/abort, panic, connection close) — UPCR-2026-023 drop-guard. This
/// is the QUESTION-store reason and is intentionally distinct from the approval
/// reasons (`APPROVAL_CANCELLED_REASON_*`): approvals keep their own audit
/// reasons. The wire value stays `"waiter_dropped"` (used by both stores'
/// drop-guards conceptually, but each store records it under its own const).
const USER_QUESTION_CANCELLED_REASON_WAITER_DROPPED: &str = "waiter_dropped";
const APPUI_METHOD_CONFIG_CAPABILITIES_LIST: &str =
    octos_core::ui_protocol::methods::CONFIG_CAPABILITIES_LIST;
const APPUI_METHOD_CLIENT_HELLO: &str = "client_hello";
const APPUI_METHOD_SESSION_STATUS_READ: &str =
    octos_core::ui_protocol::methods::SESSION_STATUS_READ;
const APPUI_METHOD_PROFILE_LOCAL_CREATE: &str =
    octos_core::ui_protocol::methods::PROFILE_LOCAL_CREATE;
const APPUI_METHOD_PROFILE_LLM_LIST: &str = "profile/llm/list";
const APPUI_METHOD_PROFILE_LLM_SELECT: &str = "profile/llm/select";
const APPUI_METHOD_MCP_STATUS_LIST: &str = "mcp/status/list";
const APPUI_METHOD_TOOL_STATUS_LIST: &str = "tool/status/list";
/// Manual, on-demand context compaction for an open session (the `/compact`
/// TUI command). Forces the compaction pass regardless of the auto-compaction
/// threshold; AppUI-only, so a local literal like the other AppUI methods.
const APPUI_METHOD_SESSION_COMPACT: &str = "session/compact";
/// Set the per-session compaction mode (LLM vs heuristic) from the `/context`
/// menu; overrides the `--llm-compaction` default for auto + manual compaction.
const APPUI_METHOD_SESSION_COMPACT_MODE_SET: &str = "session/compact/mode/set";
const APPUI_METHOD_AUTH_STATUS: &str = "auth/status";
const APPUI_METHOD_AUTH_SEND_CODE: &str = "auth/send_code";
const APPUI_METHOD_AUTH_VERIFY: &str = "auth/verify";
const APPUI_METHOD_AUTH_ME: &str = "auth/me";
const APPUI_METHOD_AUTH_LOGOUT: &str = "auth/logout";
const APPUI_METHOD_PROFILE_LLM_CATALOG: &str = "profile/llm/catalog";
const APPUI_METHOD_PROFILE_LLM_UPSERT: &str = "profile/llm/upsert";
const APPUI_METHOD_PROFILE_LLM_DELETE: &str = "profile/llm/delete";
fn should_emit_memory_snapshot(context: &str) -> bool {
    !context.trim().is_empty()
}
const APPUI_METHOD_PROFILE_LLM_TEST: &str = "profile/llm/test";
/// Named provider lanes (`sub_providers`) for per-node pipeline routing (e.g.
/// `bg_research`'s `cheap`/`strong` lanes). `/research` in the TUI reads +
/// edits these; they persist to `profile.config.sub_providers` and rebuild the
/// isolated research router at the next `ProfileRuntime` bootstrap (restart to
/// apply for a pinned solo profile).
const APPUI_METHOD_PROFILE_SUB_PROVIDERS_LIST: &str = "profile/sub_providers/list";
const APPUI_METHOD_PROFILE_SUB_PROVIDERS_UPSERT: &str = "profile/sub_providers/upsert";
const APPUI_METHOD_PROFILE_SUB_PROVIDERS_REMOVE: &str = "profile/sub_providers/remove";
/// `snapshot/list`: list the session workspace's snapshot undo points (#1768).
const APPUI_METHOD_SNAPSHOT_LIST: &str = "snapshot/list";
/// `snapshot/restore`: restore the session workspace to a snapshot (#1768).
const APPUI_METHOD_SNAPSHOT_RESTORE: &str = "snapshot/restore";
/// `turn/steer` — mid-turn prompt injection into the ACTIVE turn (codex
/// parity: app-server `turn/steer` → `Session::steer_input`). Params
/// `{session_id, expected_turn_id?, input}`; result `{turn_id, steered}`.
/// With a live turn, the input is pushed into that turn's pending-input
/// buffer under the active-turns registry lock and drained by the agent
/// loop at the next iteration boundary as a plain `role: user` message
/// (`steered: true` + the ACTIVE turn id). `expected_turn_id` mismatch →
/// `invalid_params` (codex `ExpectedTurnMismatch`). With NO live turn, the
/// call falls back to the ordinary `turn/start` admission path and returns
/// `steered: false` + the NEW turn id (codex `user_input_or_turn_inner`'s
/// `NoActiveTurn` fallback). Steering is NOT an interrupt — the in-flight
/// round always completes; `turn/interrupt` stays a separate op.
const APPUI_METHOD_TURN_STEER: &str = "turn/steer";
const APPUI_METHOD_PROFILE_SKILLS_LIST: &str = "profile/skills/list";
const APPUI_METHOD_PROFILE_SKILLS_REGISTRY_SEARCH: &str = "profile/skills/registry/search";
const APPUI_METHOD_PROFILE_SKILLS_INSTALL: &str = "profile/skills/install";
const APPUI_METHOD_PROFILE_SKILLS_REMOVE: &str = "profile/skills/remove";
/// #1057: M22 TUI Solo Onboarding backend support contract.
///
/// Backend-owned workspace validation/status for onboarding. Resolves the
/// canonical path, reports existence and writability, parses
/// `workspace_policy.toml` (presence + parse-error surfacing), and rejects
/// candidates that root under a banned system path
/// (`validate_session_workspace_path_safety`). Local-solo only — tenant /
/// cloud deployments reject with `profile_local_unsupported` so TUI clients
/// see the same typed shape they get from `profile/local/create`.
const APPUI_METHOD_ONBOARDING_WORKSPACE_PROBE: &str = "onboarding/workspace_probe";
const APPUI_METHOD_REVIEW_START: &str = octos_core::ui_protocol::methods::REVIEW_START;

/// The canonical model catalog — the single source of truth for provisionable
/// models (`model_catalog.json`). Compiled in so onboarding always has the full
/// catalog. This aliases the single embedded copy owned by `qos_catalog` (which
/// also seeds the router / context / pricing tables), so there is exactly one
/// compiled-in catalog across the crate.
use crate::qos_catalog::EMBEDDED_MODEL_CATALOG as CANONICAL_MODEL_CATALOG;
const APPUI_FEATURE_PROFILE_LOCAL_CREATE_V1: &str = "profile.local_create.v1";
/// Additive capability: the server understands the optional `requested_id`
/// field on `profile/local/create` (and treats `username`/`email` as
/// optional). Advertised only alongside `profile.local_create.v1` so a client
/// can negotiate the user-nameable-profile onboarding flow before sending the
/// new shape. Purely additive — no protocol/capabilities schema-version bump,
/// matching how `profile.local_create.v1` itself was introduced.
const APPUI_FEATURE_PROFILE_LOCAL_CREATE_REQUESTED_ID_V1: &str =
    "profile.local_create.requested_id.v1";
/// Nameable-profiles extension: this server honors the optional `make_default`
/// field on `profile/local/create`, recording the created profile as the
/// machine's global default (the brain a bare launch resolves to in a folder
/// with no sticky profile). Advertised alongside `profile.local_create.v1`;
/// purely additive. Gates the onboarding "Make this your default brain?" prompt.
const APPUI_FEATURE_PROFILE_LOCAL_CREATE_DEFAULT_V1: &str = "profile.local_create.default.v1";
const APPUI_FEATURE_PERMISSION_PROFILE_V1: &str = "permission.profile.v1";
const APPUI_FEATURE_RUNTIME_POLICY_STAMP_V1: &str = "runtime.policy_stamp.v1";
const APPUI_FEATURE_CONTEXT_LIFECYCLE_V1: &str = "context.lifecycle.v1";
/// #1057: backend onboarding support contract — TUI Solo Onboarding (M22).
/// Advertises `onboarding/workspace_probe`, which canonicalizes a workspace
/// candidate path, reports existence/writability, surfaces
/// `workspace_policy.toml` presence + parse errors, and rejects roots that
/// escape under banned system paths. Backend-owned runtime truth so the TUI
/// (a separate `octoscode` repo) only stages user intent — the canonical
/// answer is the server's.
const APPUI_FEATURE_ONBOARDING_WORKSPACE_PROBE_V1: &str = "onboarding.workspace_probe.v1";
const APPUI_EXTRA_METHODS: &[&str] = &[
    APPUI_METHOD_CLIENT_HELLO,
    APPUI_METHOD_CONFIG_CAPABILITIES_LIST,
    APPUI_METHOD_SESSION_STATUS_READ,
    APPUI_METHOD_PROFILE_LOCAL_CREATE,
    APPUI_METHOD_PROFILE_LLM_LIST,
    APPUI_METHOD_PROFILE_LLM_SELECT,
    APPUI_METHOD_MCP_STATUS_LIST,
    APPUI_METHOD_TOOL_STATUS_LIST,
    APPUI_METHOD_AUTH_STATUS,
    APPUI_METHOD_AUTH_SEND_CODE,
    APPUI_METHOD_AUTH_VERIFY,
    APPUI_METHOD_AUTH_ME,
    APPUI_METHOD_AUTH_LOGOUT,
    APPUI_METHOD_PROFILE_LLM_CATALOG,
    APPUI_METHOD_PROFILE_LLM_UPSERT,
    APPUI_METHOD_PROFILE_LLM_DELETE,
    APPUI_METHOD_PROFILE_LLM_TEST,
    APPUI_METHOD_PROFILE_SUB_PROVIDERS_LIST,
    APPUI_METHOD_PROFILE_SUB_PROVIDERS_UPSERT,
    APPUI_METHOD_PROFILE_SUB_PROVIDERS_REMOVE,
    APPUI_METHOD_SNAPSHOT_LIST,
    APPUI_METHOD_SNAPSHOT_RESTORE,
    APPUI_METHOD_TURN_STEER,
    APPUI_METHOD_PROFILE_SKILLS_LIST,
    APPUI_METHOD_PROFILE_SKILLS_REGISTRY_SEARCH,
    APPUI_METHOD_PROFILE_SKILLS_INSTALL,
    APPUI_METHOD_PROFILE_SKILLS_REMOVE,
    APPUI_METHOD_ONBOARDING_WORKSPACE_PROBE,
    octos_core::ui_protocol::methods::SESSION_BTW,
    APPUI_METHOD_SESSION_COMPACT,
    APPUI_METHOD_SESSION_COMPACT_MODE_SET,
];
const APPUI_STDIO_AUTH_BOUND_UNAVAILABLE_METHODS: &[&str] =
    &[APPUI_METHOD_AUTH_ME, APPUI_METHOD_AUTH_LOGOUT];
type SharedActiveTurns = Arc<tokio::sync::Mutex<HashMap<SessionKey, ActiveTurn>>>;
#[derive(Clone)]
struct ConnectionTurn {
    turn_id: TurnId,
    // Pin the original dispatch even after ActiveTurns advances to a reused ID.
    state: Arc<TokioMutex<TurnState>>,
}

impl ConnectionTurn {
    fn matches(&self, active: &ActiveTurn) -> bool {
        self.turn_id == active.turn_id && Arc::ptr_eq(&self.state, &active.state)
    }
}

type SharedConnectionTurns = Arc<tokio::sync::Mutex<HashMap<SessionKey, ConnectionTurn>>>;
type DynamicProfileRuntimeMap =
    std::sync::RwLock<HashMap<String, Arc<crate::runtime::ProfileRuntime>>>;

/// Per-connection registry of live ledger-forwarder tasks keyed by session.
/// Each entry pumps `LedgeredUiProtocolEvent`s from the ledger broadcast
/// into the WS write channel for the lifetime of the connection. Dropping
/// or aborting a handle terminates the pump.
///
/// #924 NIT 8: keep the full `JoinHandle` (not just the `AbortHandle`) so
/// the connection-cleanup path can `await` the aborted task before pruning
/// idle subscribers. With only an `AbortHandle`, a single `yield_now()`
/// after `abort()` was best-effort — under load the receiver might not
/// have dropped before `prune_subscriber_if_idle` ran, leaving the ledger
/// broadcaster believing it still had a live subscriber.
type SharedLiveForwarders =
    Arc<tokio::sync::Mutex<HashMap<SessionKey, tokio::task::JoinHandle<()>>>>;

/// Outcome of pushing a frame onto the per-connection writer channel.
///
/// All cases are non-fatal at the channel layer; callers decide how to react.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendError {
    /// Channel is full. The frame was not enqueued. For durable notifications
    /// this triggers a `protocol/replay_lossy` summary; for ephemeral frames
    /// it is logged at DEBUG and dropped.
    BackpressureDrop,
    /// Writer task has exited (peer disconnected or socket error). No further
    /// sends will succeed on this connection.
    Closed,
    /// A lifecycle send (turn lifecycle, RPC reply) failed. The string carries
    /// a short reason for the calling turn to abort cleanly and mark the
    /// ledger entry `delivery_failed`.
    LifecycleFailure(String),
    /// #924 BLOCK 2: a prior lifecycle/RPC send already latched the
    /// connection as failed. Background tasks (turn forwarders, live
    /// forwarders, ledger fan-out) MUST stop enqueueing onto a dead
    /// channel — callers treat this exactly like `Closed`.
    FatalClosed,
}

// Send-site categorization per M9-FIX-04 § Acceptance criteria:
//   • lifecycle  — RPC results/errors, turn/started, turn/completed,
//                  turn/error. Use `send_notification_lifecycle` /
//                  `send_rpc_*`; errors propagate; ledger entry stays as
//                  `delivery_failed`.
//   • durable    — tool/task/approval/warning. Use
//                  `send_notification_durable`; drops bump dropped_count
//                  and emit `protocol/replay_lossy`.
//   • ephemeral  — message/delta. Use `send_notification_ephemeral`;
//                  drops are silent (spec § 9).

#[derive(Debug, Default)]
pub(crate) struct ConnectionMetrics {
    pub(crate) dropped_count: AtomicU64,
    pub(crate) last_durable_seq: AtomicU64,
    pub(crate) last_durable_stream: tokio::sync::Mutex<Option<String>>,
}

impl ConnectionMetrics {
    fn record_durable_cursor(&self, cursor: &UiCursor) {
        self.last_durable_seq.store(cursor.seq, Ordering::Relaxed);
        if let Ok(mut stream) = self.last_durable_stream.try_lock() {
            *stream = Some(cursor.stream.clone());
        }
    }

    fn snapshot_last_cursor(&self) -> Option<UiCursor> {
        let seq = self.last_durable_seq.load(Ordering::Relaxed);
        if seq == 0 {
            return None;
        }
        let stream = self
            .last_durable_stream
            .try_lock()
            .ok()
            .and_then(|guard| guard.clone())?;
        Some(UiCursor { stream, seq })
    }
}

/// Per-connection writer handle: hands frames to a dedicated drainer task.
///
/// Replaces the old `Arc<Mutex<WsSink>>` pattern so no caller ever holds a
/// lock across the network `await`. Cloning is cheap; the underlying writer
/// task lives until the channel is closed (last sender dropped) or the sink
/// errors.
#[derive(Clone)]
struct ConnectionFailureSignal {
    failed: Arc<std::sync::atomic::AtomicBool>,
    failed_notify: Arc<tokio::sync::Notify>,
}

impl ConnectionFailureSignal {
    fn mark_failed(&self) {
        self.failed
            .store(true, std::sync::atomic::Ordering::Release);
        // Wake every current and future `notified()` waiter so the read
        // loop wakes immediately on an idle connection. `notify_waiters`
        // alone has no permit-stash behaviour; combined with the Acquire
        // load in the select! arm a late `notified().await` will still
        // see `failed == true` and bail out before parking.
        self.failed_notify.notify_waiters();
    }
}

#[derive(Clone)]
pub(crate) struct WsConnection {
    writer: mpsc::Sender<WsMessage>,
    stdio_writer: Option<std::sync::mpsc::SyncSender<WsMessage>>,
    metrics: Arc<ConnectionMetrics>,
    /// Unique within the process. Stamped onto every ledger append we
    /// also direct-send so the live forwarder running on this same
    /// connection can drop the broadcast copy and avoid duplicate
    /// delivery to the WS.
    connection_id: ConnectionId,
    /// #922.2: latched when a lifecycle/RPC send hits backpressure or a
    /// closed writer. The read loop polls this and breaks cleanly so a
    /// silently-dropped RPC reply does not strand the client.
    failed: Arc<std::sync::atomic::AtomicBool>,
    /// #924 BLOCK 1: a Notify woken in tandem with `failed.store(true)`.
    /// The connection main loop `select!`s on this alongside the inbound
    /// frame stream so a lifecycle/RPC send failure on an idle socket
    /// triggers cleanup immediately rather than waiting indefinitely
    /// for the next client frame.
    failed_notify: Arc<tokio::sync::Notify>,
    /// Codex #1336 round-2 BLOCKER 1: snapshot of the connection's
    /// negotiated `ConnectionUiFeatures`. The send-helper layer
    /// consults this to apply `live_event_passes_capability_filter`
    /// to DIRECT sends, mirroring what the replay/live-forwarder
    /// paths already do. Without it, a connection that negotiated
    /// `projection.envelope.v1` still received the legacy
    /// `MessageDelta` / `TurnCompleted` / tool / canonical projection
    /// frames directly from handler code — violating the mutual
    /// exclusion the γ cutover gate is supposed to enforce.
    ///
    /// Mutated only by `handle_client_hello_rpc` (via
    /// [`update_live_features`]). Reads are far more frequent than
    /// writes, so `RwLock` is the right fit.
    live_features: Arc<std::sync::RwLock<ConnectionUiFeatures>>,
}

impl WsConnection {
    pub(crate) fn new(writer: mpsc::Sender<WsMessage>) -> Self {
        Self {
            writer,
            stdio_writer: None,
            metrics: Arc::new(ConnectionMetrics::default()),
            connection_id: ConnectionId::next(),
            failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed_notify: Arc::new(tokio::sync::Notify::new()),
            live_features: Arc::new(std::sync::RwLock::new(ConnectionUiFeatures::default())),
        }
    }

    fn new_stdio(writer: std::sync::mpsc::SyncSender<WsMessage>) -> Self {
        let (unused_writer, _unused_rx) = mpsc::channel(1);
        Self {
            writer: unused_writer,
            stdio_writer: Some(writer),
            metrics: Arc::new(ConnectionMetrics::default()),
            connection_id: ConnectionId::next(),
            failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed_notify: Arc::new(tokio::sync::Notify::new()),
            live_features: Arc::new(std::sync::RwLock::new(
                ConnectionUiFeatures::stdio_defaults(),
            )),
        }
    }

    /// True for the `octos serve --stdio` transport (see [`Self::new_stdio`]).
    /// Stdio carries no auth identity, and `write_stdio_message` answers a
    /// `Close` frame by ENDING the writer loop — so WebSocket-only signals
    /// (e.g. the 1008 auth-expiry close) must not be enqueued on it (#2040).
    fn is_stdio(&self) -> bool {
        self.stdio_writer.is_some()
    }

    /// Codex #1336 round-2 BLOCKER 1: snapshot the per-connection
    /// feature set so the send helpers can apply the same capability
    /// filter the broadcast forwarder uses. Cheap because
    /// `ConnectionUiFeatures` is `Copy`.
    fn snapshot_live_features(&self) -> ConnectionUiFeatures {
        match self.live_features.read() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    /// Codex #1336 round-2 BLOCKER 1: update the per-connection
    /// feature set after `client/hello` negotiation. Called by
    /// `handle_client_hello_rpc` so subsequent direct-sends apply the
    /// new filter immediately.
    fn update_live_features(&self, features: ConnectionUiFeatures) {
        let mut guard = match self.live_features.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = features;
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// #924 BLOCK 1: handle to the latch-wakeup notify for the read
    /// loop's `select!` arm. Cloned cheaply (just an `Arc` bump).
    fn failed_notify(&self) -> Arc<tokio::sync::Notify> {
        self.failed_notify.clone()
    }

    fn failure_signal(&self) -> ConnectionFailureSignal {
        ConnectionFailureSignal {
            failed: self.failed.clone(),
            failed_notify: self.failed_notify.clone(),
        }
    }

    fn mark_failed(&self) {
        self.failure_signal().mark_failed();
    }

    #[cfg(test)]
    pub(crate) fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    fn try_enqueue(&self, frame: WsMessage) -> Result<(), SendError> {
        // #924 BLOCK 2: once the connection is latched as failed every
        // further enqueue must fail loudly. Background tasks (turn
        // forwarders, live forwarders, ledger fan-out) keep pumping
        // until the connection cleanup aborts their handles; without
        // this gate they would push frames onto a writer the read loop
        // is about to drop, masking the original lifecycle failure and
        // wasting work. `FatalClosed` is callers' signal to treat the
        // connection like `Closed`.
        if self.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(SendError::FatalClosed);
        }
        if let Some(writer) = self.stdio_writer.as_ref() {
            return match writer.try_send(frame) {
                Ok(()) => Ok(()),
                Err(std::sync::mpsc::TrySendError::Full(_)) => Err(SendError::BackpressureDrop),
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(SendError::Closed),
            };
        }
        // Update the queue-depth gauge whenever we touch the channel — cheap
        // and gives an accurate signal even when sends succeed.
        let depth = WS_WRITER_CHANNEL_CAPACITY.saturating_sub(self.writer.capacity());
        metrics::gauge!("ws.connection.queue_depth").set(depth as f64);
        match self.writer.try_send(frame) {
            Ok(_) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SendError::BackpressureDrop),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SendError::Closed),
        }
    }

    fn enqueue_durable_or_lifecycle(&self, frame: WsMessage) -> Result<(), SendError> {
        if self.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(SendError::FatalClosed);
        }
        let Some(writer) = self.stdio_writer.as_ref() else {
            return self.try_enqueue(frame);
        };
        writer.send(frame).map_err(|_| SendError::Closed)
    }

    /// Lifecycle: turn lifecycle / RPC reply. Caller acts on the failure.
    ///
    /// #922.2: a lifecycle-frame backpressure drop is treated as a
    /// connection failure. The latched `failed` flag tells the read
    /// loop to stop dispatch and tear down — better than silently
    /// dropping RPC replies (which left clients timing out while the
    /// server thought the call succeeded).
    fn send_lifecycle(&self, frame: WsMessage) -> Result<(), SendError> {
        match self.enqueue_durable_or_lifecycle(frame) {
            Ok(_) => Ok(()),
            Err(SendError::BackpressureDrop) => {
                metrics::counter!("ws.send.error.lifecycle").increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    reason = "backpressure",
                    "lifecycle ws send failed; aborting connection"
                );
                self.mark_failed();
                Err(SendError::LifecycleFailure(
                    "writer channel full for lifecycle frame".into(),
                ))
            }
            Err(SendError::Closed) => {
                metrics::counter!("ws.send.error.lifecycle").increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    reason = "closed",
                    "lifecycle ws send failed; aborting connection"
                );
                self.mark_failed();
                Err(SendError::LifecycleFailure(
                    "writer channel closed for lifecycle frame".into(),
                ))
            }
            Err(SendError::FatalClosed) => {
                // #924 BLOCK 2: prior caller already latched the
                // connection failed; surface the lifecycle-shape error
                // for any callsite still doing work on this connection.
                Err(SendError::LifecycleFailure(
                    "connection already latched as failed".into(),
                ))
            }
            Err(other) => Err(other),
        }
    }

    /// Durable notification: tool/task/approval. Errors are logged WARN; the
    /// ledger still records the event so a future replay catches up.
    fn send_durable(&self, frame: WsMessage, method: &str) -> Result<(), SendError> {
        match self.enqueue_durable_or_lifecycle(frame) {
            Ok(_) => Ok(()),
            Err(SendError::BackpressureDrop) => {
                self.metrics.dropped_count.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("ws.send.drop.backpressure", "method" => method.to_string())
                    .increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    method,
                    reason = "backpressure",
                    "durable ws send dropped; emitting replay_lossy"
                );
                Err(SendError::BackpressureDrop)
            }
            Err(SendError::Closed) => {
                metrics::counter!("ws.send.drop.closed", "method" => method.to_string())
                    .increment(1);
                metrics::counter!("ws.send.error.durable", "method" => method.to_string())
                    .increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    method,
                    reason = "closed",
                    "durable ws send failed; client gone"
                );
                Err(SendError::Closed)
            }
            Err(SendError::FatalClosed) => {
                // #924 BLOCK 2: connection already latched failed by a
                // prior lifecycle send — no point counting another
                // dropped row.
                metrics::counter!("ws.send.drop.closed", "method" => method.to_string())
                    .increment(1);
                Err(SendError::FatalClosed)
            }
            Err(other) => Err(other),
        }
    }

    /// Ephemeral frame: `message/delta`. Drops are silent per spec § 9.
    fn send_ephemeral(&self, frame: WsMessage, method: &str) -> Result<(), SendError> {
        match self.try_enqueue(frame) {
            Ok(_) => Ok(()),
            Err(SendError::BackpressureDrop) => {
                tracing::debug!(
                    target: "octos::ui_protocol::ws",
                    method,
                    "ephemeral ws send dropped under backpressure"
                );
                Err(SendError::BackpressureDrop)
            }
            Err(SendError::Closed) => {
                metrics::counter!("ws.send.drop.closed", "method" => method.to_string())
                    .increment(1);
                tracing::debug!(
                    target: "octos::ui_protocol::ws",
                    method,
                    "ephemeral ws send dropped; channel closed"
                );
                Err(SendError::Closed)
            }
            Err(SendError::FatalClosed) => {
                // #924 BLOCK 2: silently drop, consistent with the spec
                // § 9 "ephemeral frames may be dropped" rule. The
                // connection is already torn down by the read loop.
                Err(SendError::FatalClosed)
            }
            Err(other) => Err(other),
        }
    }

    /// #2065 — await-safe durable enqueue for the live-forwarder task. The
    /// stdio durable lane (`enqueue_durable_or_lifecycle`) is a BLOCKING
    /// `SyncSender::send`: correct backpressure on a blocking-capable
    /// caller, but an executor-worker stall when a full stdio queue parks
    /// an async task — and hopping it to `spawn_blocking` trades that for
    /// a worse hazard, because an in-flight blocking closure survives the
    /// task's abort and can enqueue a stale frame AFTER the lane was
    /// retired. Here the stdio lane instead parks COOPERATIVELY: a
    /// non-blocking `try_send` probe with a bounded async sleep between
    /// probes. That keeps the stdio never-drop durable semantics (the frame
    /// waits for capacity, exactly like the blocking send did), never
    /// occupies an executor worker, and is cancellable at every await with
    /// an ATOMIC enqueue: after abort+join there is no
    /// detached in-flight work that could still enqueue. The WS lane keeps
    /// its non-blocking `try_send`+`replay_lossy` semantics via
    /// [`Self::send_durable`], byte-identical to the sync callers.
    async fn send_durable_offloaded(
        &self,
        frame: WsMessage,
        method: &str,
    ) -> Result<(), SendError> {
        if self.stdio_writer.is_none() {
            return self.send_durable(frame, method);
        }
        const CAPACITY_PROBE_WINDOW: std::time::Duration = std::time::Duration::from_millis(15);
        let mut frame = frame;
        loop {
            if self.failed.load(std::sync::atomic::Ordering::Acquire) {
                return Err(SendError::FatalClosed);
            }
            let writer = self
                .stdio_writer
                .as_ref()
                .expect("stdio_writer checked above");
            match writer.try_send(frame) {
                Ok(()) => return Ok(()),
                Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                    frame = returned;
                    tokio::time::sleep(CAPACITY_PROBE_WINDOW).await;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    // Mirror `send_durable`'s Closed accounting.
                    metrics::counter!("ws.send.drop.closed", "method" => method.to_string())
                        .increment(1);
                    metrics::counter!("ws.send.error.durable", "method" => method.to_string())
                        .increment(1);
                    tracing::warn!(
                        target: "octos::ui_protocol::ws",
                        method,
                        reason = "closed",
                        "durable ws send failed; client gone"
                    );
                    return Err(SendError::Closed);
                }
            }
        }
    }
}

// `UiProtocolContractStores` + `contract_stores()` moved VERBATIM to the
// non-`api` `crate::contracts` (Phase 3 of goal-in-chat), so `octos chat
// --peers` shares the identical process-global pending-prompt registry. Used
// here through the `use` at the top of this file; no logic changed.

#[derive(Default)]
struct SessionWorkspaceStore {
    entries: std::sync::Mutex<HashMap<(String, SessionKey), SessionWorkspaceBinding>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionWorkspaceBinding {
    /// Effective workspace used by tools, panes, goals, and the UI.
    root: PathBuf,
    /// Only a genuine Tier-1/Tier-2 cwd belongs here. `None` means `root` was
    /// the derived Tier-3 workspace and must not relocate transcript storage.
    runtime_hint: Option<PathBuf>,
}

impl SessionWorkspaceStore {
    /// Establish an explicit workspace binding. Fleet-keeper recovery and the
    /// legacy call sites using this method carry an authoritative workspace,
    /// so it is also a runtime cwd hint.
    #[cfg(test)]
    fn set(&self, profile_id: &str, session_id: SessionKey, root: PathBuf) {
        self.set_resolved(profile_id, session_id, root.clone(), Some(root));
    }

    /// Publish the resolved tool/UI workspace while retaining whether it came
    /// from an explicit cwd. Keeping the provenance separate prevents a
    /// derived Tier-3 workspace from being fed back into SessionRuntimeCache as
    /// a Tier-1/Tier-2 hint on the next request.
    fn set_resolved(
        &self,
        profile_id: &str,
        session_id: SessionKey,
        root: PathBuf,
        runtime_hint: Option<PathBuf>,
    ) {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                (profile_id.to_owned(), session_id),
                SessionWorkspaceBinding { root, runtime_hint },
            );
    }

    fn get(&self, profile_id: &str, session_id: &SessionKey) -> Option<PathBuf> {
        self.snapshot(profile_id, session_id)
            .map(|binding| binding.root)
    }

    /// Clone the complete binding under one mutex acquisition. Consumers that
    /// need both root and provenance must use this snapshot so a concurrent
    /// `session/open` cannot pair one binding's root with another's hint.
    fn snapshot(
        &self,
        profile_id: &str,
        session_id: &SessionKey,
    ) -> Option<SessionWorkspaceBinding> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(profile_id.to_owned(), session_id.clone()))
            .cloned()
    }

    fn runtime_hint(&self, profile_id: &str, session_id: &SessionKey) -> Option<PathBuf> {
        self.snapshot(profile_id, session_id)
            .and_then(|binding| binding.runtime_hint)
    }
}

/// Resolve the profile component of an in-memory session-workspace key.
///
/// Authenticated SPA sessions deliberately use raw `web-*` ids, so the
/// `SessionKey` alone is not an isolation boundary. Every workspace lookup
/// therefore uses the profile already resolved by the protocol entrypoint.

#[derive(Debug, Clone, Copy, Default)]
struct StoredSessionPermissionProfile {
    selection: octos_core::ui_protocol::PermissionProfileSelection,
    approval_policy: Option<octos_agent::ApprovalPolicy>,
}

#[derive(Default)]
struct SessionPermissionProfileStore {
    selections: std::sync::Mutex<HashMap<SessionKey, StoredSessionPermissionProfile>>,
}

impl SessionPermissionProfileStore {
    // Read-side convenience pair kept for feature combinations that resolve
    // the stored profile directly; this build only reads via
    // `get_state_explicit`.
    #[allow(dead_code)]
    fn get(&self, session_id: &SessionKey) -> octos_core::ui_protocol::PermissionProfileSelection {
        self.get_state(session_id).selection
    }

    #[allow(dead_code)] // see `get` above
    fn get_state(&self, session_id: &SessionKey) -> StoredSessionPermissionProfile {
        self.get_state_explicit(session_id).unwrap_or_default()
    }

    /// The stored selection ONLY if the session made an explicit
    /// `/permissions` choice — `None` lets the caller apply a configurable
    /// default (see `effective_session_permission_state`).
    fn get_state_explicit(
        &self,
        session_id: &SessionKey,
    ) -> Option<StoredSessionPermissionProfile> {
        self.selections
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(session_id)
            .copied()
    }

    fn set(
        &self,
        session_id: SessionKey,
        selection: octos_core::ui_protocol::PermissionProfileSelection,
        approval_policy: Option<octos_agent::ApprovalPolicy>,
    ) {
        self.selections
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                session_id,
                StoredSessionPermissionProfile {
                    selection,
                    approval_policy,
                },
            );
    }
}

/// Session permission state honoring the serve-level dangerous default
/// (`--danger-full-access`, octos' analogue of Claude Code's
/// `--dangerously-skip-permissions`): a session with NO explicit
/// `/permissions` selection falls back to the full-access profile —
/// sandbox off, network allowed, approvals never — instead of the gated
/// workspace-write default. The flag is solo-gated at serve startup (the
/// same keystone that gates selecting the profile from the menu), and an
/// explicit per-session choice always wins.
fn effective_session_permission_state(
    state: &AppState,
    session_id: &SessionKey,
) -> StoredSessionPermissionProfile {
    if let Some(stored) = session_permission_profiles().get_state_explicit(session_id) {
        return stored;
    }
    // The dangerous default is granted ONLY where an EXPLICIT Full Access
    // selection would also be allowed (codex P1/P2 on #1639): Local
    // deployment mode (`effective_permissions_for_session` rejects
    // DangerFullAccess for Tenant/Cloud — an unscoped default there would
    // fail every unselected session/open) AND a session key that does not
    // encode a non-solo tenant/cloud scope (the same defence-in-depth gate
    // `permission_selection_allowed` applies, so a tenant-scoped session
    // can't be handed host access via the fallback). A session that fails
    // either check falls through to the gated workspace-write default.
    if state.dangerous_default_permissions && !session_id_encodes_non_solo_scope(session_id) {
        return StoredSessionPermissionProfile {
            selection: octos_core::ui_protocol::PermissionProfileSelection {
                mode: octos_core::ui_protocol::PermissionProfileMode::DangerFullAccess,
                network: octos_core::ui_protocol::PermissionNetworkPolicy::Allow,
            },
            approval_policy: Some(octos_agent::ApprovalPolicy::Never),
        };
    }
    // Network-on default: a fresh Local session with no explicit selection gets
    // Workspace-Write with network ALLOWED (filesystem still sandboxed, approvals
    // unchanged) so `npm install` / git / fetch work out of the box — the macOS
    // SBPL otherwise emits `(deny network*)` and silently breaks the most common
    // dev workflow. Scoped exactly like the dangerous default (Local deployment +
    // a session key that doesn't encode a tenant/cloud scope) so cloud/tenant
    // stays network-denied. Opt OUT with `--no-network` (`OCTOS_NO_NETWORK=1`).
    // An explicit `/permissions` choice still overrides this.
    if !state.default_network_denied && !session_id_encodes_non_solo_scope(session_id) {
        return StoredSessionPermissionProfile {
            selection: octos_core::ui_protocol::PermissionProfileSelection {
                mode: octos_core::ui_protocol::PermissionProfileMode::WorkspaceWrite,
                network: octos_core::ui_protocol::PermissionNetworkPolicy::Allow,
            },
            approval_policy: None,
        };
    }
    StoredSessionPermissionProfile::default()
}

#[derive(Default)]
struct SessionContextStatusStore {
    statuses: std::sync::Mutex<HashMap<SessionKey, Value>>,
}

impl SessionContextStatusStore {
    fn set(&self, session_id: SessionKey, status: Value) {
        self.statuses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(session_id, status);
    }

    fn get(&self, session_id: &SessionKey) -> Option<Value> {
        self.statuses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(session_id)
            .cloned()
    }
}

/// Per-turn lifecycle state tracked by the registry under a single `Mutex`
/// guard. Together with the `interrupt_tx` signalling channel, this is the
/// boundary that makes interrupt-vs-natural-completion atomic and ensures
/// exactly one terminal event reaches the wire.
///
/// State transitions:
/// ```text
///        (turn/start)
///             |
///             v
///   +------- Active -------+
///   |          |           |
///   |   (handler           |   (task observes
///   |    interrupts)       |    natural finish)
///   |          v           |          v
///   |    Interrupting      |   Terminal(Completed)
///   |          |           |          /
///   |    (task acks)       |   Terminal(Errored)
///   |          v           v
///   +--> Terminal(Interrupted) <------+
/// ```
/// All terminal-event emission sites must lock the state, observe `Active` or
/// `Interrupting`, and atomically transition to `Terminal(_)` before sending.
/// Any path that sees a `Terminal(_)` state is a no-op (lost the race).
/// What caused a turn to be interrupted.
///
/// The turn runtime reports this to the client on `turn/error`, so it has to
/// name the ACTUAL cause. `peer_close` deliberately aborts the closed peer's
/// in-flight turn over the same `interrupt_tx` path `turn/interrupt` uses
/// (see `interrupt_closed_peer_turn`), and the message was hardcoded to
/// "interrupted by client" — so a peer whose review was aborted mid-flight
/// reported an interrupt from a client that had sent nothing, pointing an
/// investigator at the wrong subsystem. Observed live: a peer three tool calls
/// into a 550-line review, closed by its master, blamed the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptOrigin {
    /// `turn/interrupt` from the connected client.
    Client,
}

impl InterruptOrigin {
    /// The `turn/error` message for this origin.
    fn message(self) -> &'static str {
        match self {
            Self::Client => "turn interrupted by client",
        }
    }
}

#[derive(Debug)]
enum TurnState {
    /// Turn is running normally; eligible for interrupt.
    Active,
    /// Handler captured an interrupt request and is waiting for the task to
    /// emit the terminal event and signal `ack`. `origin` records WHO asked, so
    /// the terminal event can name the real cause.
    Interrupting {
        ack: oneshot::Sender<()>,
        origin: InterruptOrigin,
    },
    /// Terminal state — exactly one terminal event has been emitted.
    Terminal(TerminalReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalReason {
    Completed,
    Errored,
    Interrupted,
}

impl TerminalReason {
    fn as_str(self) -> &'static str {
        match self {
            TerminalReason::Completed => "completed",
            TerminalReason::Errored => "errored",
            TerminalReason::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum M9ProtocolFixture {
    Basic,
    M19StdioHappyPath,
    Slow,
    ToolEvents,
    Approval,
    ReplayLossy,
    ReplayLossyForcedTerminalDrop,
    TaskOutput,
    M14CodexP0ToolParity,
    M15LiveSubagents,
}

fn m9_protocol_fixture_for_prompt(prompt: &str) -> Option<M9ProtocolFixture> {
    let prompt_lower = prompt.to_ascii_lowercase();
    if std::env::var("OCTOS_M15_LIVE_SUBAGENT_FIXTURE").as_deref() == Ok("1")
        && (prompt_lower.contains("m15 code review")
            || prompt_lower.contains("live subagent")
            || prompt_lower.contains("supervised subagents"))
    {
        return Some(M9ProtocolFixture::M15LiveSubagents);
    }

    if std::env::var("OCTOS_M9_PROTOCOL_FIXTURES").as_deref() != Ok("1") {
        return None;
    }

    if prompt_lower.contains("m19_stdio_happy_path_final_line") {
        Some(M9ProtocolFixture::M19StdioHappyPath)
    } else if prompt_lower.contains("m9 approval fixture")
        || prompt_lower.contains("m9-approval-e2e")
    {
        Some(M9ProtocolFixture::Approval)
    } else if prompt_lower.contains("m14 codex p0 tool parity fixture")
        || prompt_lower.contains("codex p0 tool parity")
        || prompt_lower.contains("#969")
    {
        Some(M9ProtocolFixture::M14CodexP0ToolParity)
    } else if (prompt_lower.contains("m9 replay-lossy fixture")
        || prompt_lower.contains("replay-lossy"))
        && (prompt_lower.contains("forced dropped turn/completed")
            || prompt_lower.contains("force true dropped turn/completed")
            || prompt_lower.contains("forced terminal drop"))
    {
        Some(M9ProtocolFixture::ReplayLossyForcedTerminalDrop)
    } else if prompt_lower.contains("m9 replay-lossy fixture")
        || prompt_lower.contains("replay-lossy")
    {
        Some(M9ProtocolFixture::ReplayLossy)
    } else if prompt_lower.contains("m9 task output fixture") {
        Some(M9ProtocolFixture::TaskOutput)
    } else if prompt_lower.contains("list_dir tool") {
        Some(M9ProtocolFixture::ToolEvents)
    } else if prompt_lower.contains("200 separate lines")
        || prompt_lower.contains("one line at a time")
    {
        Some(M9ProtocolFixture::Slow)
    } else {
        Some(M9ProtocolFixture::Basic)
    }
}

struct ActiveTurn {
    turn_id: TurnId,
    /// Resolved profile the turn was admitted under (canonical: absent maps
    /// to `MAIN_PROFILE_ID`). `session/btw` verifies it before injecting the
    /// turn's live draft into an aside — the registry is process-global and
    /// keyed by bare `SessionKey`, so two profiles' same-named sessions would
    /// otherwise cross-read each other's stream.
    profile_id: String,
    /// Per-turn state guard; held by both the registry entry and by the turn
    /// task so interrupt + natural-completion races serialize on a single lock.
    state: Arc<TokioMutex<TurnState>>,
    /// Single-shot wake-up so the turn loop can return from `progress_rx.recv`
    /// promptly when an interrupt arrives. `None` once consumed.
    interrupt_tx: Arc<TokioMutex<Option<mpsc::Sender<()>>>>,
    /// Per-turn pending-input buffer for `turn/steer` (codex
    /// `TurnState.pending_input` parity). `Some` for regular standalone
    /// turns — `turn/steer` pushes into it under the registry lock and the
    /// agent loop drains at the next iteration boundary. `None` for
    /// non-steerable turns (code review, M9 protocol fixtures), mirroring
    /// codex's `ActiveTurnNotSteerable` for review/compact turn kinds.
    steer: Option<octos_agent::SharedSteerBuffer>,
    abort: AbortHandle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ConnectionUiFeatures {
    typed_approvals: bool,
    pane_snapshots: bool,
    session_workspace_cwd: bool,
    session_sandbox: bool,
    /// UPCR-2026-009 `state.session_hydrate.v1` negotiated.
    session_hydrate: bool,
    /// UPCR-2026-010 `state.thread_graph.v1` negotiated.
    thread_graph: bool,
    /// UPCR-2026-011 `state.turn_state_get.v1` negotiated.
    turn_state_get: bool,
    /// M10 Phase 1 `event.spawn_complete.v1` negotiated. Retained solely for
    /// historic durable-record compatibility; new background completions use
    /// unconditional canonical v2 child envelopes.
    spawn_complete: bool,
    /// UPCR-2026-014 M9-α-9 `event.file_attached.v1` negotiated. When
    /// set, the connection receives one `file/attached` envelope per
    /// artefact delivered by a `spawn_only` background tool — a
    /// dedicated wire signal that runs alongside the canonical v2 child
    /// payload's media.
    /// Defensive against the slides soak (2026-05-24) failure mode
    /// where the richer envelopes' placement logic dropped PPTX
    /// deliveries on the SPA's chat thread.
    file_attached: bool,
    /// `plan.todos.v1` negotiated. When set, the server streams the
    /// `update_plan` tool's checklist as `plan/updated` notifications and
    /// replays the latest snapshot on `session/open`. Otherwise the plan rides
    /// out only on the legacy `tool/completed` `structured_metadata` path.
    plan_todos: bool,
    /// #2019 `event.background_activity.v1`: the HUMAN sink over background
    /// events that today only wake the model (monitor event lines, claimed
    /// fleet outbox events). Not negotiated → the connection never receives
    /// `background/activity`, so a client that cannot render it never sees an
    /// "unknown notification" (the ui-protocol v2 migration trap).
    background_activity: bool,
    /// UPCR-2026-014 M9-γ `projection.envelope.v1` negotiated. When set,
    /// the client opts in to the historical v1 envelope shape (spec
    /// § 14) for projected events. γ-1 wires capability negotiation
    /// only — no emit site references this flag yet, and legacy
    /// `message/delta`, `tool/*`, and
    /// `turn/completed` notifications continue to flow on the wire.
    /// γ-2 (follow-up) gates emission on this flag; γ-3 deletes the
    /// legacy notifications.
    projection_envelope: bool,
    /// Stage 1 `projection.envelope.v2` negotiated. This is deliberately
    /// independent from the v1 flag and defaults to false on every transport.
    /// When set, the connection receives the cursor-stamped v2 projection
    /// wire shape and neither legacy nor v1 projection frames.
    projection_envelope_v2: bool,
    /// UPCR-2026-021 M15 autonomy capability root. Optional agent and loop
    /// groups are honoured only when this base capability is negotiated too.
    coding_autonomy_v1: bool,
    /// UPCR-2026-021 M15 agent lifecycle inspection/control group.
    coding_agent_control_v1: bool,
    /// UPCR-2026-019 typed backend-owned product review workflow.
    review_start_v1: bool,
    /// M16 backend-owned context generation/checkpoint/compaction lifecycle.
    context_lifecycle_v1: bool,
    /// UPCR-2026-029 additive semantic-context/provider-cache diagnostics.
    /// Meaningful only alongside the parent context lifecycle capability.
    context_semantic_cache_v1: bool,
    /// UPCR-2026-023 `user_question.v1` negotiated. When set, the connection's
    /// turn task installs a [`SessionUserQuestionRequester`] so the agent's
    /// `ask_user_question` tool blocks on `user_question/respond`. When unset,
    /// the requester is not installed and the tool degrades to its
    /// structured-metadata fallback.
    user_question_v1: bool,
    /// task-return-unconsumed-steer-inputs: client asked for the
    /// dropped-before-terminal steer settlement guarantee.
    turn_steer_dropped_v1: bool,
    /// `true` when the client sent at least one feature token via the
    /// `X-Octos-Ui-Features` header or the `ui_feature` / `ui_features`
    /// query parameter (UPCR-2026-007). Distinguishes "no header at all"
    /// (where the server falls back to advertising the full first-slice in
    /// `SessionOpened.capabilities`) from "header sent with all-unknown
    /// tokens" (where the negotiated `supported_features` is empty).
    header_present: bool,
    /// True for `octos serve --stdio`. Stdio shares AppUI where possible,
    /// but WebSocket-routed methods must be removed from advertised
    /// capabilities and rejected with typed transport reasons if called.
    stdio_transport: bool,
}

impl ConnectionUiFeatures {
    fn stdio_defaults() -> Self {
        Self {
            typed_approvals: true,
            pane_snapshots: true,
            session_workspace_cwd: true,
            session_sandbox: true,
            session_hydrate: true,
            thread_graph: true,
            turn_state_get: true,
            spawn_complete: true,
            file_attached: true,
            plan_todos: true,
            background_activity: true,
            // Do NOT auto-enable `projection.envelope.v1` for stdio
            // connections. Legacy `turn/completed` is the turn-lifecycle
            // source for clients that do not consume `projection/envelope`
            // (e.g. the octoscode over stdio, which clears its turn-active
            // state — `live_reply`, backing the send-gate — ONLY on legacy
            // `turn/completed`). The γ-cutover mutual-exclusion gate in
            // `live_event_passes_capability_filter` DROPS legacy
            // `turn/completed` whenever `projection_envelope` is true, so
            // auto-enabling envelopes here suppresses the only lifecycle
            // signal such clients understand and wedges them (every message
            // after turn 1 queues "after active turn" forever). A stdio
            // client that genuinely consumes envelopes can still opt in via
            // `client_hello` (`from_requested_feature_tokens`), so this is a
            // default-only change, not a capability removal.
            projection_envelope: false,
            projection_envelope_v2: false,
            coding_autonomy_v1: true,
            coding_agent_control_v1: true,
            review_start_v1: true,
            context_lifecycle_v1: true,
            // Cache diagnostics are strictly opt-in. Stdio sends a server
            // capability slice before `client_hello`, so enabling this by
            // default would advertise fields the client never negotiated.
            context_semantic_cache_v1: false,
            user_question_v1: true,
            turn_steer_dropped_v1: true,
            header_present: true,
            stdio_transport: true,
        }
    }

    fn from_requested_feature_tokens<I, S>(features: I, stdio_transport: bool) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let requested: HashSet<String> = features
            .into_iter()
            .map(|feature| feature.as_ref().trim().to_owned())
            .filter(|feature| !feature.is_empty())
            .collect();
        let has = |feature: &str| requested.contains(feature);
        Self {
            typed_approvals: has(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1),
            pane_snapshots: has(UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1),
            session_workspace_cwd: has(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1),
            session_sandbox: has(UI_PROTOCOL_FEATURE_SESSION_SANDBOX_V1),
            session_hydrate: has(UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1),
            thread_graph: has(UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1),
            turn_state_get: has(UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1),
            spawn_complete: has(UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1),
            file_attached: has(UI_PROTOCOL_FEATURE_FILE_ATTACHED_V1),
            plan_todos: has(UI_PROTOCOL_FEATURE_PLAN_TODOS_V1),
            background_activity: has(UI_PROTOCOL_FEATURE_BACKGROUND_ACTIVITY_V1),
            projection_envelope: has(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1),
            projection_envelope_v2: has(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2),
            coding_autonomy_v1: has(UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1),
            coding_agent_control_v1: has(UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1),
            review_start_v1: has(UI_PROTOCOL_FEATURE_REVIEW_START_V1),
            context_lifecycle_v1: has(UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1),
            context_semantic_cache_v1: has(UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1),
            user_question_v1: has(UI_PROTOCOL_FEATURE_USER_QUESTION_V1),
            turn_steer_dropped_v1: has(UI_PROTOCOL_FEATURE_TURN_STEER_DROPPED_V1),
            header_present: true,
            stdio_transport,
        }
    }

    /// Build the `UiProtocolCapabilities` payload to advertise on
    /// `SessionOpened` per UPCR-2026-007 § 4 capability negotiation. When
    /// the client sent no feature header at all, the server returns the
    /// `first_server_slice` default so clients can still discover the
    /// surface in-band. When the client sent at least one feature token,
    /// the server returns the intersection of requested features with the
    /// known feature registry — clients see exactly which of their
    /// requests were honoured and never receive a flag they did not ask
    /// for.
    fn negotiated_capabilities(self) -> UiProtocolCapabilities {
        if !self.header_present {
            let mut capabilities = UiProtocolCapabilities::first_server_slice();
            capabilities
                .supported_features
                .retain(|feature| feature != UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1);
            return capabilities;
        }
        let mut requested: Vec<&str> = Vec::with_capacity(8);
        if self.typed_approvals {
            requested.push(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1);
        }
        if self.pane_snapshots {
            requested.push(UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1);
        }
        if self.session_workspace_cwd {
            requested.push(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1);
        }
        if self.session_sandbox {
            requested.push(UI_PROTOCOL_FEATURE_SESSION_SANDBOX_V1);
        }
        if self.session_hydrate {
            requested.push(UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1);
        }
        if self.thread_graph {
            requested.push(UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1);
        }
        if self.turn_state_get {
            requested.push(UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1);
        }
        if self.spawn_complete {
            requested.push(UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1);
        }
        if self.file_attached {
            requested.push(UI_PROTOCOL_FEATURE_FILE_ATTACHED_V1);
        }
        if self.plan_todos {
            requested.push(UI_PROTOCOL_FEATURE_PLAN_TODOS_V1);
        }
        if self.background_activity {
            requested.push(UI_PROTOCOL_FEATURE_BACKGROUND_ACTIVITY_V1);
        }
        if self.projection_envelope {
            requested.push(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1);
        }
        if self.projection_envelope_v2 {
            requested.push(UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2);
        }
        if self.coding_autonomy_v1 {
            requested.push(UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1);
            if self.coding_agent_control_v1 {
                requested.push(UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1);
            }
        }
        if self.context_lifecycle_v1 {
            requested.push(UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1);
            if self.context_semantic_cache_v1 {
                requested.push(UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1);
            }
        }
        if self.review_start_v1 {
            requested.push(UI_PROTOCOL_FEATURE_REVIEW_START_V1);
        }
        if self.user_question_v1 {
            requested.push(UI_PROTOCOL_FEATURE_USER_QUESTION_V1);
        }
        if self.turn_steer_dropped_v1 {
            requested.push(UI_PROTOCOL_FEATURE_TURN_STEER_DROPPED_V1);
        }
        UiProtocolCapabilities::for_negotiated_features(requested)
    }

    fn agent_control_available(self) -> bool {
        !self.header_present || (self.coding_autonomy_v1 && self.coding_agent_control_v1)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn context_lifecycle_available(self) -> bool {
        !self.header_present || self.context_lifecycle_v1
    }

    fn context_semantic_cache_available(self) -> bool {
        self.context_lifecycle_available() && self.context_semantic_cache_v1
    }

    fn advertised_capabilities(self, state: &AppState) -> UiProtocolCapabilities {
        let mut capabilities = self.negotiated_capabilities();
        for method in APPUI_EXTRA_METHODS {
            if *method == APPUI_METHOD_PROFILE_LOCAL_CREATE
                && !supports_local_solo_profile_create(state)
            {
                continue;
            }
            // #1057: `onboarding/workspace_probe` is a local-solo onboarding
            // helper. Tenant/cloud deployments do not expose it because their
            // workspace lifecycle is owned by their control plane, not by
            // per-session canonicalize/probe calls.
            if *method == APPUI_METHOD_ONBOARDING_WORKSPACE_PROBE
                && !supports_local_solo_profile_create(state)
            {
                continue;
            }
            if is_profile_skill_appui_method(method) && state.profile_store.is_none() {
                continue;
            }
            if !capabilities
                .supported_methods
                .iter()
                .any(|existing| existing == method)
            {
                capabilities.supported_methods.push((*method).into());
            }
        }
        push_capability_feature(
            &mut capabilities.supported_features,
            APPUI_FEATURE_PERMISSION_PROFILE_V1,
        );
        push_capability_feature(
            &mut capabilities.supported_features,
            APPUI_FEATURE_RUNTIME_POLICY_STAMP_V1,
        );
        push_capability_feature(
            &mut capabilities.supported_features,
            super::coding_tool_contract::CODING_TOOL_CONTRACT_FEATURE_V1,
        );
        // #972 / M14-B P1: advertise the optional Codex-parity capabilities
        // now that the underlying tools (`view_image`, `tool_search`,
        // `tool_suggest`) are wired through the profile runtime. UPCR-2026-020
        // §3 lets capability-gated fields ride alongside the canonical
        // contract; we omit `image_generation` because no native or skill
        // backend is bound to it.
        push_capability_feature(
            &mut capabilities.supported_features,
            super::coding_tool_contract::CODING_IMAGE_VIEW_CAPABILITY_V1,
        );
        push_capability_feature(
            &mut capabilities.supported_features,
            super::coding_tool_contract::CODING_DYNAMIC_TOOL_SEARCH_CAPABILITY_V1,
        );
        // #1172 — Codex naming-parity aliases. The underlying capabilities
        // ride on `shell` / `exec_command` (for `bash`) and `spawn_agent` +
        // `wait_agent` (for `delegate`). Advertising them lets a Codex-trained
        // client skip the `tool not found` -> `tool_search` round trip on
        // first call.
        push_capability_feature(
            &mut capabilities.supported_features,
            super::coding_tool_contract::CODING_BASH_CAPABILITY_V1,
        );
        push_capability_feature(
            &mut capabilities.supported_features,
            super::coding_tool_contract::CODING_DELEGATE_CAPABILITY_V1,
        );
        if self.context_lifecycle_available() {
            push_capability_feature(
                &mut capabilities.supported_features,
                APPUI_FEATURE_CONTEXT_LIFECYCLE_V1,
            );
            if self.context_semantic_cache_available() {
                push_capability_feature(
                    &mut capabilities.supported_features,
                    UI_PROTOCOL_FEATURE_CONTEXT_SEMANTIC_CACHE_V1,
                );
            }
        }
        // #965 / UPCR-2026-019 — advertise the M13-A canonical capability
        // names alongside the consolidated `coding.agent_control.v1` so
        // clients that look for the spec strings find them. The methods
        // themselves still gate on `coding.agent_control.v1`, so older
        // clients negotiating only that flag keep working unchanged.
        if self.agent_control_available() {
            push_capability_feature(
                &mut capabilities.supported_features,
                octos_core::ui_protocol::UI_PROTOCOL_FEATURE_HARNESS_TASK_SUPERVISION_INSPECTION_V1,
            );
        }
        if supports_local_solo_profile_create(state) {
            push_capability_feature(
                &mut capabilities.supported_features,
                APPUI_FEATURE_PROFILE_LOCAL_CREATE_V1,
            );
            // Nameable profiles: advertise that this server honors the optional
            // `requested_id` field (and optional username/email). Additive on
            // top of the v1 flag, so older clients that negotiate only v1 keep
            // working with the legacy `{name, username, email}` shape.
            push_capability_feature(
                &mut capabilities.supported_features,
                APPUI_FEATURE_PROFILE_LOCAL_CREATE_REQUESTED_ID_V1,
            );
            // Nameable profiles: advertise that this server honors the optional
            // `make_default` field, recording the created profile as the global
            // default. Additive; gates the onboarding make-default prompt.
            push_capability_feature(
                &mut capabilities.supported_features,
                APPUI_FEATURE_PROFILE_LOCAL_CREATE_DEFAULT_V1,
            );
            // #1057: workspace probe ships next to local-solo onboarding so
            // the TUI can advertise the recovery UX only when the backend
            // can answer canonical-path / workspace-policy questions.
            push_capability_feature(
                &mut capabilities.supported_features,
                APPUI_FEATURE_ONBOARDING_WORKSPACE_PROBE_V1,
            );
        }
        if self.stdio_transport {
            apply_stdio_auth_bound_capability_policy(&mut capabilities);
        }
        capabilities
    }
}

/// Stage 4 telemetry retained after the Stage 5 deletion gate.
///
/// The legacy persisted-delivery counter was removed with the legacy lane:
/// new server delivery samples cover canonical v2 envelopes only. Connection
/// mode and v2 replay-gap counters remain useful operational signals.
fn record_ui_protocol_connection_mode(features: ConnectionUiFeatures, transport: &'static str) {
    let mode = if features.projection_envelope_v2 {
        "v2"
    } else {
        "legacy"
    };
    metrics::counter!(
        "octos_ui_protocol_connection_total",
        "mode" => mode,
        "transport" => transport,
    )
    .increment(1);
}

/// A delivery shape that has a Stage-4 telemetry counter. This is classified
/// before serializing a frame and recorded only after its enqueue succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiProtocolDeliveryMetric {
    V2Envelope,
}

fn ui_protocol_delivery_metric(event: &UiProtocolLedgerEvent) -> Option<UiProtocolDeliveryMetric> {
    match event {
        UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(_)) => {
            Some(UiProtocolDeliveryMetric::V2Envelope)
        }
        _ => None,
    }
}

fn record_ui_protocol_delivery_metric(metric: Option<UiProtocolDeliveryMetric>) {
    let Some(metric) = metric else {
        return;
    };

    match metric {
        UiProtocolDeliveryMetric::V2Envelope => {
            metrics::counter!("octos_ui_protocol_v2_envelope_delivered_total").increment(1);
        }
    }
}

fn apply_stdio_auth_bound_capability_policy(capabilities: &mut UiProtocolCapabilities) {
    for method in APPUI_STDIO_AUTH_BOUND_UNAVAILABLE_METHODS {
        capabilities
            .supported_methods
            .retain(|supported| supported != method);
        if capabilities.unsupported_report(method).is_none() {
            capabilities
                .unsupported
                .push(UnsupportedCapabilityReport::method(
                    *method,
                    "unauthenticated stdio transport has no AppUI auth identity",
                ));
        }
    }
}

fn is_profile_skill_appui_method(method: &str) -> bool {
    matches!(
        method,
        APPUI_METHOD_PROFILE_SKILLS_LIST
            | APPUI_METHOD_PROFILE_SKILLS_REGISTRY_SEARCH
            | APPUI_METHOD_PROFILE_SKILLS_INSTALL
            | APPUI_METHOD_PROFILE_SKILLS_REMOVE
    )
}

fn push_capability_feature(features: &mut Vec<String>, feature: &str) {
    if !features.iter().any(|existing| existing == feature) {
        features.push(feature.to_owned());
    }
}

#[derive(Default)]
struct TaskOutputDeltaTracker {
    active_task_id: Option<TaskId>,
    offsets: HashMap<TaskId, u64>,
}

impl TaskOutputDeltaTracker {
    fn observe_progress_event(
        &mut self,
        session_id: &SessionKey,
        event: &Value,
    ) -> Option<TaskOutputDeltaEvent> {
        let event_type = event.get("type").and_then(Value::as_str);
        if event_type == Some("task_started") {
            self.active_task_id = task_id_field(event);
        }

        let task_id = task_id_field(event).or_else(|| self.active_task_id.clone())?;
        let text = task_output_delta_text(event)?;
        let offset = self.offsets.entry(task_id.clone()).or_insert(0);
        let start_offset = *offset;
        let cursor = OutputCursor {
            offset: start_offset,
        };
        *offset = start_offset.saturating_add(text.len() as u64);

        Some(TaskOutputDeltaEvent {
            session_id: session_id.clone(),
            topic: None,
            task_id,
            cursor,
            text,
        })
    }
}

fn active_turns_registry() -> SharedActiveTurns {
    static ACTIVE_TURNS: OnceLock<SharedActiveTurns> = OnceLock::new();
    ACTIVE_TURNS
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(HashMap::new())))
        .clone()
}

/// NEW-16 defense-in-depth: per-turn message-index cursor for the
/// `response.messages` persist loop.
///
/// The main fix is the append-only `turn_output_log` upstream in
/// `loop_runner.rs`, but this cursor exists as a second wall. It
/// tracks the highest message-index successfully persisted for each
/// `(session_id, turn_id)` pair. If the same `(turn, index)` is seen
/// twice (e.g. an edge path drove the persist loop into the SAME
/// turn twice), subsequent indexes are skipped.
///
/// Keyed by `(SessionId.0, TurnId.0)` strings so distinct sessions
/// using the same TurnId string cannot collide. Today TurnIds are
/// UUIDs minted server-side (see `TurnId::new` -> `Uuid::new_v4`)
/// so cross-session collisions are vanishingly unlikely, but the
/// composite key keeps the contract explicit.
///
/// Entries carry a `last_touched_at: Instant` and are evicted
/// opportunistically on every read/write that finds them stale
/// (older than `TURN_PERSIST_CURSOR_TTL`). Codex round 3 caught
/// that an immediate end-of-turn GC has two flaws:
///   1. A queued same-`(session, turn)` re-entry serialised on the
///      session lock can be scheduled AFTER the first outer task
///      already evicted the cursor — the guard would miss.
///   2. Cursor entries leak if the outer `run_standalone_turn`
///      future is aborted (connection close, parent task drop)
///      after the persist block inserted the cursor but before
///      the bottom-of-fn GC runs.
///
/// A TTL covers both: the cursor lives long enough for any
/// realistic queued re-entry to observe it (the TTL is far longer
/// than the inter-invocation window), and stale entries get
/// pruned by subsequent persist activity in the SAME session
/// rather than relying on cancellation-safe cleanup paths.
type TurnPersistCursors = Arc<TokioMutex<HashMap<(String, String), TurnPersistCursorEntry>>>;

#[derive(Clone, Copy, Debug)]
struct TurnPersistCursorEntry {
    /// Highest-index-already-persisted + 1 (i.e. the NEXT index to
    /// write). For the synthetic final-assistant row, this is
    /// `response.messages.len() + 1`.
    next_index: usize,
    last_touched_at: std::time::Instant,
}

/// TTL for per-`(session, turn)` cursor entries. 5 minutes is
/// far longer than any realistic per-turn execution time (the
/// agent loop's `DEFAULT_SESSION_TIMEOUT_SECS` is 1800s, and a
/// queued same-turn re-entry would be serialised on the session
/// lock — typically tens of milliseconds at most). Long enough
/// to make the queued-re-entry guard reliable; short enough that
/// abort/panic leaks bound at ~5 minutes per stale entry.
const TURN_PERSIST_CURSOR_TTL: Duration = Duration::from_secs(300);

fn turn_persist_cursors() -> TurnPersistCursors {
    static CURSORS: OnceLock<TurnPersistCursors> = OnceLock::new();
    CURSORS
        .get_or_init(|| Arc::new(TokioMutex::new(HashMap::new())))
        .clone()
}

/// Codex round-4 P2: throttle the full `HashMap::retain` scan
/// to at most once per `TURN_PERSIST_CURSOR_PRUNE_INTERVAL`.
/// `retain` is O(capacity), and `HashMap` capacity can stay
/// high after a burst. Without the throttle, thousands of
/// concurrent turns/sec would all serialise behind a full-map
/// scan under the global cursor mutex. When the throttle says
/// "too soon," the read returns immediately and stale entries
/// get evicted on the next eligible scan. Memory growth is
/// bounded by `persist rate * (TTL + PRUNE_INTERVAL)` in the
/// worst case — still O(active turns × constant).
const TURN_PERSIST_CURSOR_PRUNE_INTERVAL: Duration = Duration::from_secs(30);

/// Per-process throttle marker for `prune_stale_turn_persist_cursors`.
/// `StdMutex` is fine — we only hold it across an `Instant` read.
fn turn_persist_cursor_prune_throttle() -> Arc<StdMutex<Option<std::time::Instant>>> {
    static THROTTLE: OnceLock<Arc<StdMutex<Option<std::time::Instant>>>> = OnceLock::new();
    THROTTLE
        .get_or_init(|| Arc::new(StdMutex::new(None)))
        .clone()
}

/// Opportunistic prune: remove every entry older than
/// `TURN_PERSIST_CURSOR_TTL`. Called from the persist block
/// itself on every entry/exit, so the map is bounded by
/// "entries created within the last TTL window."
fn prune_stale_turn_persist_cursors(map: &mut HashMap<(String, String), TurnPersistCursorEntry>) {
    let throttle = turn_persist_cursor_prune_throttle();
    let now = std::time::Instant::now();
    {
        let mut last_pruned = throttle.lock().unwrap_or_else(|e| e.into_inner());
        match *last_pruned {
            Some(prev) if now.duration_since(prev) < TURN_PERSIST_CURSOR_PRUNE_INTERVAL => {
                // Within the throttle window — skip the full scan.
                return;
            }
            _ => {
                *last_pruned = Some(now);
            }
        }
    }
    map.retain(|_, entry| now.duration_since(entry.last_touched_at) < TURN_PERSIST_CURSOR_TTL);
}

fn session_workspaces() -> Arc<SessionWorkspaceStore> {
    static SESSION_WORKSPACES: OnceLock<Arc<SessionWorkspaceStore>> = OnceLock::new();
    SESSION_WORKSPACES
        .get_or_init(|| Arc::new(SessionWorkspaceStore::default()))
        .clone()
}

fn session_context_statuses() -> Arc<SessionContextStatusStore> {
    static SESSION_CONTEXT_STATUSES: OnceLock<Arc<SessionContextStatusStore>> = OnceLock::new();
    SESSION_CONTEXT_STATUSES
        .get_or_init(|| Arc::new(SessionContextStatusStore::default()))
        .clone()
}

pub(crate) fn update_session_context_status(session_id: &SessionKey, status: Value) {
    session_context_statuses().set(session_id.clone(), status);
}

type AppUiSessionContextManagerRegistry =
    StdMutex<HashMap<SessionKey, std::sync::Weak<StdMutex<ContextManager>>>>;

/// Live per-session `ContextManager` registry. The turn runner registers its
/// per-turn manager here and the returned guard removes it when the turn
/// unwinds, so delivery-time paths resolve the current session manager rather
/// than a stale `Arc` retained by a long-lived sender closure.
fn appui_session_context_managers() -> &'static AppUiSessionContextManagerRegistry {
    static REGISTRY: OnceLock<AppUiSessionContextManagerRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

#[must_use = "the live registration is removed when this guard drops"]
struct AppUiSessionContextRegistration {
    session_id: SessionKey,
    manager: std::sync::Weak<StdMutex<ContextManager>>,
}

impl Drop for AppUiSessionContextRegistration {
    fn drop(&mut self) {
        let mut registry = appui_session_context_managers()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if registry
            .get(&self.session_id)
            .is_some_and(|current| current.ptr_eq(&self.manager))
        {
            registry.remove(&self.session_id);
        }
    }
}

fn register_appui_session_context_manager(
    session_id: &SessionKey,
    manager: &Arc<StdMutex<ContextManager>>,
) -> AppUiSessionContextRegistration {
    let mut registry = appui_session_context_managers()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    registry.retain(|_, weak| weak.strong_count() > 0);
    registry.insert(session_id.clone(), Arc::downgrade(manager));
    AppUiSessionContextRegistration {
        session_id: session_id.clone(),
        manager: Arc::downgrade(manager),
    }
}

fn live_appui_session_context_manager(
    session_id: &SessionKey,
) -> Option<Arc<StdMutex<ContextManager>>> {
    appui_session_context_managers()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(session_id)
        .and_then(std::sync::Weak::upgrade)
}

type AppUiContextPersistLocks = StdMutex<HashMap<SessionKey, std::sync::Weak<StdMutex<()>>>>;

fn appui_context_persist_locks() -> &'static AppUiContextPersistLocks {
    static LOCKS: OnceLock<AppUiContextPersistLocks> = OnceLock::new();
    LOCKS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Per-session lock serializing load/mutate/persist sequences for the context
/// snapshot so concurrent writers cannot regress the newest generation.
fn appui_context_persist_lock(session_id: &SessionKey) -> Arc<StdMutex<()>> {
    appui_context_persist_lock_from(appui_context_persist_locks(), session_id)
}

fn appui_context_persist_lock_from(
    locks: &AppUiContextPersistLocks,
    session_id: &SessionKey,
) -> Arc<StdMutex<()>> {
    let mut locks = locks.lock().unwrap_or_else(|error| error.into_inner());
    // Each writer (including a waiter) holds an Arc before taking the session
    // mutex. Pruning only expired Weak entries cannot split a live lock.
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(session_id).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(StdMutex::new(()));
    locks.insert(session_id.clone(), Arc::downgrade(&lock));
    lock
}

fn persist_appui_context_snapshot(
    data_dir: &Path,
    session_id: &SessionKey,
    manager: &ContextManager,
) -> Result<PathBuf, String> {
    let lock = appui_context_persist_lock(session_id);
    let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
    persist_context_manager_snapshot(data_dir, &session_id.to_string(), manager)
}

fn appui_context_status_value(manager: &ContextManager) -> Value {
    let state = manager.state();
    let last_compaction = manager.compactions().last().map(|record| {
        json!({
            "compaction_id": record.compaction_id.as_str(),
            "checkpoint_id": record.checkpoint_id.as_str(),
            "status": record.status,
            "policy_id": record.policy_id,
            "trigger": record.trigger,
            "input_generation": record.input_generation,
            "output_generation": record.output_generation,
            "input_transcript_hash": record.input_transcript_hash,
            "replacement_transcript_hash": record.replacement_transcript_hash,
            "installed_transcript_hash": record.installed_transcript_hash,
            "input_item_count": record.input_item_count,
            "retained_count": record.retained_item_ids.len(),
            "dropped_count": record.dropped_item_ids.len(),
            "summary_item_id": record.summary_item_id.as_ref().map(|id| id.as_str()),
            "token_estimate_before": record.token_estimate_before,
            "token_estimate_after": record.token_estimate_after,
            "error": record.error,
        })
    });
    json!({
        "schema": "octos.context.lifecycle.v1",
        "state": state,
        "compaction": {
            "count": manager.compactions().len(),
            "last": last_compaction,
        }
    })
}

fn context_recovery_state_string(state: &impl Serialize) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn ui_context_state_for(session_id: &SessionKey, manager: &ContextManager) -> UiContextState {
    let state = manager.state();
    UiContextState {
        session_id: session_id.clone(),
        thread_id: state.thread_id,
        generation: state.generation,
        transcript_hash: state.transcript_hash,
        item_count: state.item_count,
        token_estimate: state.token_estimate,
        recovery_state: context_recovery_state_string(&state.recovery_state),
        last_checkpoint_id: state
            .last_checkpoint_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        last_compaction_id: state
            .last_compaction_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        cache_epoch_id: state.cache_epoch_id,
        last_cache_invalidation_reason: state.last_cache_invalidation_reason,
        semantic_head_id: state.semantic_head_id,
        semantic_head_kind: state.semantic_head_kind,
    }
}

fn retain_negotiated_semantic_cache_diagnostics(
    state: &mut UiContextState,
    features: ConnectionUiFeatures,
) {
    if features.context_semantic_cache_available() {
        return;
    }
    state.cache_epoch_id = None;
    state.last_cache_invalidation_reason = None;
    state.semantic_head_id = None;
    state.semantic_head_kind = None;
}

fn retain_negotiated_context_payload_diagnostics(
    context: &mut Value,
    features: ConnectionUiFeatures,
) {
    if features.context_semantic_cache_available() {
        return;
    }
    let Some(state) = context.get_mut("state").and_then(Value::as_object_mut) else {
        return;
    };
    state.remove("cache_epoch_id");
    state.remove("last_cache_invalidation_reason");
    state.remove("semantic_head_id");
    state.remove("semantic_head_kind");
}

fn context_snapshot_for_features(
    mut context: Option<Value>,
    mut context_state: Option<UiContextState>,
    features: ConnectionUiFeatures,
) -> (Option<Value>, Option<UiContextState>) {
    if let Some(context) = &mut context {
        retain_negotiated_context_payload_diagnostics(context, features);
    }
    if let Some(context_state) = &mut context_state {
        retain_negotiated_semantic_cache_diagnostics(context_state, features);
    }
    (context, context_state)
}

fn context_event_for_features(
    mut event: UiProtocolLedgerEvent,
    features: ConnectionUiFeatures,
) -> UiProtocolLedgerEvent {
    let UiProtocolLedgerEvent::Notification(notification) = &mut event else {
        return event;
    };
    match notification {
        UiNotification::SessionOpened(opened) => {
            if let Some(context) = &mut opened.context {
                retain_negotiated_context_payload_diagnostics(context, features);
            }
            if let Some(context_state) = &mut opened.context_state {
                retain_negotiated_semantic_cache_diagnostics(context_state, features);
            }
        }
        UiNotification::ContextCompactionCompleted(completed) => {
            retain_negotiated_semantic_cache_diagnostics(&mut completed.context_state, features);
        }
        UiNotification::ContextCompactionStarted(started) => {
            retain_negotiated_semantic_cache_diagnostics(&mut started.context_state, features);
        }
        UiNotification::ContextNormalizationReported(reported) => {
            retain_negotiated_semantic_cache_diagnostics(&mut reported.context_state, features);
        }
        _ => {}
    }
    event
}

fn appui_context_inspection_snapshot(
    data_dir: &Path,
    session_id: &SessionKey,
    history: &[Message],
) -> (Value, UiContextState) {
    // A status/hydrate read during an active turn reports the live
    // model-visible generation and must not overwrite its ledger file. The
    // live check, load, and persist share the session writer lock.
    let persist_lock = appui_context_persist_lock(session_id);
    let persist_guard = persist_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(live) = live_appui_session_context_manager(session_id) {
        // Other paths use manager → persist lock order, so release the writer
        // lock before taking the live-manager mutex.
        drop(persist_guard);
        let manager = live
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        publish_appui_context_status(session_id, &manager);
        return (
            appui_context_status_value(&manager),
            ui_context_state_for(session_id, &manager),
        );
    }
    let _persist_guard = persist_guard;
    let (manager, ledger_status) =
        load_or_rebuild_context_manager(data_dir, session_id.to_string(), None, history);
    tracing::debug!(
        session = %session_id.0,
        ledger_status = ?ledger_status,
        "appui context manager loaded for inspection"
    );
    publish_appui_context_status(session_id, &manager);
    if let Err(error) =
        persist_context_manager_snapshot(data_dir, &session_id.to_string(), &manager)
    {
        warn!(
            session = %session_id.0,
            error = %error,
            "failed to persist appui context manager inspection snapshot"
        );
    }
    (
        appui_context_status_value(&manager),
        ui_context_state_for(session_id, &manager),
    )
}

fn appui_context_status_snapshot_for_session(
    session_id: &SessionKey,
) -> (Option<Value>, Option<UiContextState>) {
    let context = session_context_statuses().get(session_id);
    let context_state = context
        .as_ref()
        .and_then(|context| context.get("state"))
        .and_then(|state| serde_json::from_value::<UiContextState>(state.clone()).ok());
    (context, context_state)
}

async fn appui_context_status_snapshot_for_state(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    session_id: &SessionKey,
) -> (Option<Value>, Option<UiContextState>) {
    // During an active turn, prompt preparation can install and persist a
    // compacted ContextManager generation before the final assistant row is
    // committed. A status read in that window must report the live
    // model-visible generation, not rebuild from the durable user-facing
    // session rows and accidentally overwrite the prompt-time context ledger.
    if let (Some(context), Some(context_state)) =
        appui_context_status_snapshot_for_session(session_id)
    {
        return (Some(context), Some(context_state));
    }
    if let Some(sessions) =
        resolve_sessions_for_lookup(state, connection_profile_id, routed_profile_id, session_id)
            .await
    {
        let snapshot_input = {
            let mut sessions_guard = sessions.lock().await;
            if sessions_guard.session_known(session_id) {
                let data_dir = sessions_guard.data_dir();
                let history = sessions_guard
                    .get_or_create(session_id)
                    .await
                    .messages
                    .clone();
                Some((data_dir, history))
            } else {
                None
            }
        };
        if let Some((data_dir, history)) = snapshot_input {
            let (context, context_state) =
                appui_context_inspection_snapshot(&data_dir, session_id, &history);
            return (Some(context), Some(context_state));
        }
    }
    appui_context_status_snapshot_for_session(session_id)
}

fn ui_context_compaction_record_for(record: &ContextCompactionRecord) -> UiContextCompactionRecord {
    UiContextCompactionRecord {
        compaction_id: record.compaction_id.as_str().to_owned(),
        checkpoint_id: record.checkpoint_id.as_str().to_owned(),
        status: context_recovery_state_string(&record.status),
        policy_id: record.policy_id.clone(),
        trigger: record.trigger.clone(),
        input_generation: record.input_generation,
        output_generation: record.output_generation,
        input_transcript_hash: record.input_transcript_hash.clone(),
        replacement_transcript_hash: record.replacement_transcript_hash.clone(),
        installed_transcript_hash: record.installed_transcript_hash.clone(),
        input_item_count: record.input_item_count,
        retained_count: record.retained_item_ids.len(),
        dropped_count: record.dropped_item_ids.len(),
        summary_item_id: record
            .summary_item_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        token_estimate_before: record.token_estimate_before,
        token_estimate_after: record.token_estimate_after,
        error: record.error.clone(),
    }
}

fn appui_context_compaction_notification(
    session_id: &SessionKey,
    manager: &ContextManager,
    record: &ContextCompactionRecord,
) -> UiNotification {
    UiNotification::ContextCompactionCompleted(ContextCompactionCompletedEvent {
        session_id: session_id.clone(),
        context_state: ui_context_state_for(session_id, manager),
        compaction: ui_context_compaction_record_for(record),
    })
}

fn appui_context_normalization_notification(
    session_id: &SessionKey,
    frame: &PromptFrame,
) -> UiNotification {
    UiNotification::ContextNormalizationReported(ContextNormalizationReportedEvent {
        session_id: session_id.clone(),
        context_state: UiContextState {
            session_id: session_id.clone(),
            thread_id: frame.context_state.thread_id.clone(),
            generation: frame.context_state.generation,
            transcript_hash: frame.context_state.transcript_hash.clone(),
            item_count: frame.context_state.item_count,
            token_estimate: frame.context_state.token_estimate,
            recovery_state: context_recovery_state_string(&frame.context_state.recovery_state),
            last_checkpoint_id: frame
                .context_state
                .last_checkpoint_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            last_compaction_id: frame
                .context_state
                .last_compaction_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            cache_epoch_id: frame.context_state.cache_epoch_id.clone(),
            last_cache_invalidation_reason: frame
                .context_state
                .last_cache_invalidation_reason
                .clone(),
            semantic_head_id: frame.context_state.semantic_head_id.clone(),
            semantic_head_kind: frame.context_state.semantic_head_kind.clone(),
        },
        normalization: UiContextNormalizationReport {
            generation: frame.report.generation,
            input_transcript_hash: frame.report.input_transcript_hash.clone(),
            output_prompt_hash: frame.report.output_prompt_hash.clone(),
            model_capability_id: frame.report.model_capability_id.clone(),
            prompt_message_count: frame.messages.len(),
            token_estimate: frame.report.token_estimate,
            repaired_count: frame.report.repaired_item_ids.len(),
            dropped_count: frame.report.dropped_item_ids.len(),
            synthetic_count: frame.report.synthetic_item_ids.len(),
            truncated_count: frame.report.truncated_item_ids.len(),
        },
    })
}

fn appui_manual_compaction_result(
    session_id: &SessionKey,
    record: &ContextCompactionRecord,
    failure_reason: Option<&str>,
) -> Value {
    let status = match record.status {
        ContextCompactionStatus::Installed => "installed",
        ContextCompactionStatus::Failed => "failed",
    };
    let failed = record.status == ContextCompactionStatus::Failed
        || record.budget_outcome == ContextCompactionBudgetOutcome::RejectedOverBudget;
    let reason = failed.then(|| {
        failure_reason.unwrap_or(match record.budget_outcome {
            ContextCompactionBudgetOutcome::RejectedOverBudget => "rejected_over_budget",
            _ => "compaction_failed",
        })
    });
    serde_json::json!({
        "session_id": session_id.to_string(),
        "compacted": !failed,
        "status": status,
        "reason": reason,
        "input_generation": record.input_generation,
        "output_generation": record.output_generation,
        "token_estimate_before": record.token_estimate_before,
        "token_estimate_after": record.token_estimate_after,
    })
}

fn publish_appui_context_status(session_id: &SessionKey, manager: &ContextManager) {
    update_session_context_status(session_id, appui_context_status_value(manager));
}

fn appui_context_compact_threshold_tokens(llm_provider: &dyn octos_llm::LlmProvider) -> usize {
    appui_compact_threshold_tokens_for(
        env_usize("OCTOS_CONTEXT_COMPACT_THRESHOLD_TOKENS"),
        llm_provider.context_window() as usize * APPUI_CONTEXT_COMPACT_RATIO_NUMERATOR
            / APPUI_CONTEXT_COMPACT_RATIO_DENOMINATOR,
    )
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
}

fn appui_compact_threshold_tokens_for(
    env_override: Option<usize>,
    derived_threshold: usize,
) -> usize {
    env_override.map_or_else(|| derived_threshold.max(1), |requested| requested.max(1))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OupSemanticContextRolloutMode {
    Off,
    Shadow,
    On,
}

fn parse_oup_semantic_context_rollout_mode(raw: Option<&str>) -> OupSemanticContextRolloutMode {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("off") | Some("0") | Some("false") => OupSemanticContextRolloutMode::Off,
        Some("shadow") => OupSemanticContextRolloutMode::Shadow,
        Some("on") | Some("1") | Some("true") => OupSemanticContextRolloutMode::On,
        None => OupSemanticContextRolloutMode::On,
        Some(other) => {
            tracing::warn!(
                value = other,
                "invalid OCTOS_OUP_SEMANTIC_CONTEXT_MODE; using semantic boundary mode"
            );
            OupSemanticContextRolloutMode::On
        }
    }
}

fn oup_semantic_context_rollout_mode() -> OupSemanticContextRolloutMode {
    let value = std::env::var("OCTOS_OUP_SEMANTIC_CONTEXT_MODE").ok();
    parse_oup_semantic_context_rollout_mode(value.as_deref())
}

/// Token budgets shared by every AppUI compaction site for one threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AppUiCompactionBudgets {
    target_after: usize,
    summary_budget: u32,
    semantic_target: usize,
}

fn appui_compaction_budgets_for(
    threshold_tokens: usize,
    target_override: Option<usize>,
) -> AppUiCompactionBudgets {
    let threshold_tokens = threshold_tokens.max(1);
    let derived_target = (threshold_tokens * APPUI_CONTEXT_COMPACT_TARGET_NUMERATOR
        / APPUI_CONTEXT_COMPACT_TARGET_DENOMINATOR)
        .max(1);
    let target_after = match target_override {
        Some(requested) if requested >= 1 && requested < threshold_tokens => requested,
        Some(requested) => {
            warn!(
                requested,
                threshold_tokens,
                derived_target,
                "invalid OCTOS_CONTEXT_COMPACT_TARGET_TOKENS: the post-compaction target must be \
                 at least 1 and below the compaction threshold; using the derived target"
            );
            derived_target
        }
        None => derived_target,
    };
    let summary_budget = (target_after / 3)
        .clamp(256, 4096)
        .min(target_after.saturating_sub(1))
        .max(1);
    let semantic_target = target_after
        .saturating_sub(summary_budget)
        .max(target_after / 2)
        .max(1);
    AppUiCompactionBudgets {
        target_after,
        summary_budget: summary_budget as u32,
        semantic_target,
    }
}

fn appui_compaction_budgets(threshold_tokens: usize) -> AppUiCompactionBudgets {
    appui_compaction_budgets_for(
        threshold_tokens,
        env_usize("OCTOS_CONTEXT_COMPACT_TARGET_TOKENS"),
    )
}

fn appui_semantic_compact_policy(
    trigger: impl Into<String>,
    budgets: AppUiCompactionBudgets,
) -> CompactContextPolicy {
    let mode = oup_semantic_context_rollout_mode();
    appui_semantic_compact_policy_for_mode(
        trigger,
        budgets.target_after,
        budgets.semantic_target,
        mode,
    )
}

fn appui_semantic_compact_policy_for_mode(
    trigger: impl Into<String>,
    target_after: usize,
    semantic_target: usize,
    mode: OupSemanticContextRolloutMode,
) -> CompactContextPolicy {
    CompactContextPolicy {
        policy_id: match mode {
            OupSemanticContextRolloutMode::Off => "legacy-item-boundary-v1",
            OupSemanticContextRolloutMode::Shadow => "semantic-boundary-shadow-v1",
            OupSemanticContextRolloutMode::On => "semantic-boundary-v1",
        }
        .to_owned(),
        trigger: trigger.into(),
        // Reserve room for the summary itself. The newest user turn and any
        // open tool interaction remain raw even if they exceed this soft tail
        // target; ContextManager never splits those semantic blocks.
        keep_recent_tokens: (mode == OupSemanticContextRolloutMode::On).then_some(semantic_target),
        semantic_shadow_keep_recent_tokens: (mode == OupSemanticContextRolloutMode::Shadow)
            .then_some(semantic_target),
        target_tokens_after_compaction: (mode == OupSemanticContextRolloutMode::On)
            .then_some(target_after),
        ..CompactContextPolicy::default()
    }
}

/// Per-session compaction-mode override, set from the `/context` menu via
/// `session/compact/mode/set`. `true` = force the LLM summary path, `false` =
/// force the heuristic. Absent = follow the server `--llm-compaction` default.
/// In-memory per serve process (resets on restart), like the other per-session
/// runtime maps in this module.
fn session_compaction_llm_override() -> &'static std::sync::RwLock<HashMap<SessionKey, bool>> {
    static OVERRIDE: OnceLock<std::sync::RwLock<HashMap<SessionKey, bool>>> = OnceLock::new();
    OVERRIDE.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

/// Effective LLM-vs-heuristic decision for a session: the per-session `/context`
/// override if set, else the server `--llm-compaction` default. Read by all
/// three compaction sites (auto pre-turn, auto in-loop, manual `session/compact`).
fn session_compaction_llm_enabled(session_id: &SessionKey, state: &AppState) -> bool {
    session_compaction_llm_override()
        .read()
        .ok()
        .and_then(|map| map.get(session_id).copied())
        .unwrap_or(state.llm_compaction)
}

/// Persist a per-session compaction-mode override.
fn set_session_compaction_llm(session_id: &SessionKey, llm: bool) {
    if let Ok(mut map) = session_compaction_llm_override().write() {
        map.insert(session_id.clone(), llm);
    }
}

/// Effective compaction mode as a wire string (`"llm"` / `"heuristic"`), for the
/// `session/status/read` payload and the `session/compact*` RPC replies so the
/// `/context` menu can render + reflect the current selection.
fn session_compaction_mode_str(session_id: &SessionKey, state: &AppState) -> &'static str {
    if session_compaction_llm_enabled(session_id, state) {
        "llm"
    } else {
        "heuristic"
    }
}

/// Produce an LLM-summarization compaction summary for the AppUI path, falling
/// back to the deterministic [`compact_messages`] heuristic whenever the LLM
/// summary errors, times out, or the runtime is unsupported — so it can never
/// break a turn. Only invoked when the `--llm-compaction` serve flag is on
/// (`AppState::llm_compaction`); the flag-off path calls the heuristic directly.
/// Returns a plain `String` for the unchanged `compact_context`.
fn appui_compaction_summary(
    llm_provider: &Arc<dyn octos_llm::LlmProvider>,
    frame: &crate::context_manager::PromptFrame,
    budget_tokens: u32,
) -> String {
    if let Some(summary) = octos_agent::compaction::llm_compaction_summary_with_budget(
        llm_provider,
        &frame.messages,
        budget_tokens,
        std::time::Duration::from_secs(
            octos_agent::compaction::DEFAULT_LLM_COMPACTION_TIMEOUT_SECS,
        ),
    ) {
        return summary;
    }
    // Heuristic fallback still uses the summary-size budget (correct there).
    frame.compact_summary(budget_tokens)
}

/// Route identity for the prompt-cache epoch: the `ProviderMetadata`
/// `{provider, model}` pair, i.e. the same label the serving lane reports
/// through `provider_metadata_for_index` once a response arrives. Using
/// `provider_name()` here would compare a `label@host` router tag against
/// the untagged metadata label and rotate the epoch on every call.
pub(crate) fn prompt_cache_lane_identity(
    llm_provider: &dyn octos_llm::LlmProvider,
) -> (String, String) {
    let metadata = llm_provider.provider_metadata();
    (metadata.provider, metadata.model)
}

fn appui_context_prompt_policy(llm_provider: &dyn octos_llm::LlmProvider) -> PromptBuildPolicy {
    PromptBuildPolicy {
        include_reasoning: false,
        supports_media: true,
        max_prompt_token_estimate: None,
        model_capability_id: format!(
            "{}/{}",
            llm_provider.provider_name(),
            llm_provider.model_id()
        ),
    }
}

/// Threshold-gated compaction body shared by the pre-turn prompt path and the
/// session-open snapshot. Compacts `manager` IN PLACE when its estimate
/// exceeds the provider-derived threshold and returns the lifecycle
/// started/completed notifications describing the pass; empty when the
/// estimate is already under threshold.
fn appui_compact_context_if_over_threshold(
    manager: &mut ContextManager,
    session_id: &SessionKey,
    llm_provider: &Arc<dyn octos_llm::LlmProvider>,
    llm_compaction_enabled: bool,
    trigger: &str,
) -> Vec<UiNotification> {
    let threshold = appui_context_compact_threshold_tokens(llm_provider.as_ref());
    let policy = appui_context_prompt_policy(llm_provider.as_ref());
    if !manager.should_auto_compact(threshold) {
        return Vec::new();
    }
    let budgets = appui_compaction_budgets(threshold);
    let summary_budget = budgets.summary_budget;
    let compact_policy = appui_semantic_compact_policy(trigger, budgets);
    if !manager.should_retry_compaction(&compact_policy) {
        return Vec::new();
    }
    let mut lifecycle_notifications = Vec::new();
    // UPCR-2026-026: the started event precedes the (synchronous) pass;
    // clients render the in-progress state from it and must tolerate
    // started/completed arriving in one delivery batch.
    lifecycle_notifications.push(UiNotification::ContextCompactionStarted(
        ContextCompactionStartedEvent {
            session_id: session_id.clone(),
            context_state: ui_context_state_for(session_id, manager),
            trigger: trigger.to_owned(),
            threshold_tokens: threshold,
        },
    ));
    let summary_messages = manager.compaction_input(&compact_policy, &policy);
    if summary_messages.messages.is_empty() {
        let record = manager.record_failed_compaction(
            compact_policy,
            "no closed semantic prefix is safe to compact",
        );
        lifecycle_notifications.push(appui_context_compaction_notification(
            session_id, manager, &record,
        ));
        return lifecycle_notifications;
    }
    let summary = if llm_compaction_enabled {
        appui_compaction_summary(llm_provider, &summary_messages, summary_budget)
    } else {
        summary_messages.compact_summary(summary_budget)
    };
    let record = manager.compact_context(summary, compact_policy);
    info!(
        session = %session_id.0,
        compaction_id = %record.compaction_id.as_str(),
        checkpoint_id = %record.checkpoint_id.as_str(),
        input_generation = record.input_generation,
        output_generation = ?record.output_generation,
        token_estimate_before = record.token_estimate_before,
        token_estimate_after = ?record.token_estimate_after,
        target_tokens_after_compaction = ?record.target_tokens_after_compaction,
        pinned_token_estimate = ?record.pinned_token_estimate,
        budget_outcome = ?record.budget_outcome,
        trigger,
        "appui context manager compact_context finished before model prompt"
    );
    lifecycle_notifications.push(appui_context_compaction_notification(
        session_id, manager, &record,
    ));
    lifecycle_notifications
}

/// Session-open flavor of [`appui_context_inspection_snapshot`]: same
/// load → publish → persist, but first runs the SAME threshold compaction the
/// pre-turn path would run on the next message. Without this, a session whose
/// ledger was REBUILT from a long raw history (legacy/stale/invalid snapshot)
/// publishes an over-window estimate that sits on the client's context gauge
/// from open until the first turn — field report 2026-08-07: a freshly booted
/// serve hydrated a 3k-message session to a 1.17M-token ledger and the TUI
/// read `ctx 1.2M/1M` while idle.
///
/// `llm_provider` is the SAME provider the session's turns resolve (peer lane
/// → profile primary); its `context_window()` derives the threshold. `None`
/// (no materialized session runtime) fails OPEN — publish as-is — because
/// compacting against a guessed default window could destructively
/// over-compact a long-window session's context.
///
/// The compaction here is always the DETERMINISTIC summarizer
/// (`llm_compaction_enabled=false`): `session/open` is an interactive RPC and
/// must not spend an LLM call per session at boot. The returned lifecycle
/// started/completed notifications (empty when nothing compacted) must be
/// appended to the session's ledger BEFORE the open computes its replay head,
/// so the open's own replay delivers them and the client renders the
/// compaction UX — an open-time 1.17M→4K rewrite must not be silent.
fn appui_context_open_snapshot(
    data_dir: &Path,
    session_id: &SessionKey,
    history: &[Message],
    llm_provider: Option<&Arc<dyn octos_llm::LlmProvider>>,
) -> (Value, UiContextState, Vec<UiNotification>) {
    // While a turn owns the ledger, report its live generation and never
    // compact or persist from disk underneath it. The live check, load,
    // compaction, and persist share the session writer lock.
    let persist_lock = appui_context_persist_lock(session_id);
    let persist_guard = persist_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(live) = live_appui_session_context_manager(session_id) {
        drop(persist_guard);
        let manager = live
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        publish_appui_context_status(session_id, &manager);
        return (
            appui_context_status_value(&manager),
            ui_context_state_for(session_id, &manager),
            Vec::new(),
        );
    }
    let _persist_guard = persist_guard;
    let (mut manager, ledger_status) =
        load_or_rebuild_context_manager(data_dir, session_id.to_string(), None, history);
    tracing::debug!(
        session = %session_id.0,
        ledger_status = ?ledger_status,
        "appui context manager loaded for session open"
    );
    let lifecycle_notifications = match llm_provider {
        Some(provider) => appui_compact_context_if_over_threshold(
            &mut manager,
            session_id,
            provider,
            false,
            "appui_open",
        ),
        None => Vec::new(),
    };
    publish_appui_context_status(session_id, &manager);
    if let Err(error) =
        persist_context_manager_snapshot(data_dir, &session_id.to_string(), &manager)
    {
        warn!(
            session = %session_id.0,
            error = %error,
            "failed to persist appui context manager open snapshot"
        );
    }
    (
        appui_context_status_value(&manager),
        ui_context_state_for(session_id, &manager),
        lifecycle_notifications,
    )
}

fn appui_context_history_for_agent(
    data_dir: &Path,
    session_id: &SessionKey,
    history: &[Message],
    llm_provider: &Arc<dyn octos_llm::LlmProvider>,
    llm_compaction_enabled: bool,
    trigger: &str,
) -> (
    Vec<Message>,
    Arc<StdMutex<ContextManager>>,
    Vec<UiNotification>,
    AppUiSessionContextRegistration,
) {
    // Load, optional compaction, persistence, and publication as the live
    // manager are one writer-locked operation for this session.
    let persist_lock = appui_context_persist_lock(session_id);
    let persist_guard = persist_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (mut manager, ledger_status) =
        load_or_rebuild_context_manager(data_dir, session_id.to_string(), None, history);
    tracing::debug!(
        session = %session_id.0,
        ledger_status = ?ledger_status,
        "appui context manager loaded for turn"
    );
    let policy = appui_context_prompt_policy(llm_provider.as_ref());
    let mut lifecycle_notifications = appui_compact_context_if_over_threshold(
        &mut manager,
        session_id,
        llm_provider,
        llm_compaction_enabled,
        trigger,
    );
    publish_appui_context_status(session_id, &manager);
    if let Err(error) =
        persist_context_manager_snapshot(data_dir, &session_id.to_string(), &manager)
    {
        warn!(
            session = %session_id.0,
            error = %error,
            "failed to persist appui context manager snapshot"
        );
    }
    let frame = manager.for_prompt(&policy);
    lifecycle_notifications.push(appui_context_normalization_notification(session_id, &frame));
    let manager = Arc::new(StdMutex::new(manager));
    let registration = register_appui_session_context_manager(session_id, &manager);
    drop(persist_guard);
    (
        frame.messages,
        manager,
        lifecycle_notifications,
        registration,
    )
}

/// Append model-visible runtime facts after durable conversation history and
/// re-project the prompt. These events intentionally bypass the canonical
/// chat transcript (they are wake/snapshot metadata, not user-authored chat
/// rows) but remain durable in the ContextManager v2 snapshot.
fn appui_append_tail_context_events(
    data_dir: &Path,
    session_id: &SessionKey,
    llm_provider: &Arc<dyn octos_llm::LlmProvider>,
    context_manager: &Arc<StdMutex<ContextManager>>,
    history: &mut Vec<Message>,
    events: Vec<(ContextEventKind, &'static str, String)>,
) {
    if events.is_empty() {
        return;
    }
    let mut manager = context_manager
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut appended = 0usize;
    for (event_kind, label, content) in events {
        appended += usize::from(
            manager
                .record_context_event(event_kind, label, content)
                .is_some(),
        );
    }
    if appended == 0 {
        return;
    }

    *history = manager
        .for_prompt(&appui_context_prompt_policy(llm_provider.as_ref()))
        .messages;
    publish_appui_context_status(session_id, &manager);
    if let Err(error) = persist_appui_context_snapshot(data_dir, session_id, &manager) {
        warn!(
            session = %session_id.0,
            error = %error,
            "failed to persist appui tail context events"
        );
    }
    tracing::debug!(
        session = %session_id.0,
        appended,
        "appui appended volatile runtime context at the prompt tail"
    );
}

/// Force a context-compaction pass on a session's ledger, bypassing the
/// auto-compaction threshold. Powers the manual `session/compact` UI method.
///
/// Mirrors the pre-turn path [`appui_context_history_for_agent`] but always runs
/// the compaction body (no `token_estimate > threshold` gate). Returns the
/// lifecycle notifications to emit (`ContextCompactionStarted` + `Completed`)
/// and a small result value (`token_estimate` before/after) for the RPC reply.
fn appui_force_compact_context(
    data_dir: &Path,
    session_id: &SessionKey,
    history: &[Message],
    llm_provider: &Arc<dyn octos_llm::LlmProvider>,
    llm_compaction_enabled: bool,
) -> (Vec<UiNotification>, serde_json::Value) {
    const TRIGGER: &str = "appui_manual_compact";
    // The caller holds the session writer lock across its live-turn check and
    // this load → compact → persist sequence.
    let (mut manager, ledger_status) =
        load_or_rebuild_context_manager(data_dir, session_id.to_string(), None, history);
    tracing::debug!(
        session = %session_id.0,
        ledger_status = ?ledger_status,
        "appui context manager loaded for manual compaction"
    );
    let threshold = appui_context_compact_threshold_tokens(llm_provider.as_ref());
    let policy = appui_context_prompt_policy(llm_provider.as_ref());
    let mut lifecycle_notifications = Vec::new();
    // Forced: unlike the pre-turn path there is no `token_estimate > threshold`
    // gate — the user explicitly asked to compact now.
    lifecycle_notifications.push(UiNotification::ContextCompactionStarted(
        ContextCompactionStartedEvent {
            session_id: session_id.clone(),
            context_state: ui_context_state_for(session_id, &manager),
            trigger: TRIGGER.to_owned(),
            threshold_tokens: threshold,
        },
    ));
    let budgets = appui_compaction_budgets(threshold);
    let summary_budget = budgets.summary_budget;
    let compact_policy = appui_semantic_compact_policy(TRIGGER, budgets);
    let summary_messages = manager.compaction_input(&compact_policy, &policy);
    if summary_messages.messages.is_empty() {
        let record = manager.record_failed_compaction(
            compact_policy,
            "no closed semantic prefix is safe to compact",
        );
        lifecycle_notifications.push(appui_context_compaction_notification(
            session_id, &manager, &record,
        ));
        let result =
            appui_manual_compaction_result(session_id, &record, Some("no_safe_semantic_boundary"));
        publish_appui_context_status(session_id, &manager);
        let _ = persist_context_manager_snapshot(data_dir, &session_id.to_string(), &manager);
        return (lifecycle_notifications, result);
    }
    let summary = if llm_compaction_enabled {
        appui_compaction_summary(llm_provider, &summary_messages, summary_budget)
    } else {
        summary_messages.compact_summary(summary_budget)
    };
    let record = manager.compact_context(summary, compact_policy);
    info!(
        session = %session_id.0,
        compaction_id = %record.compaction_id.as_str(),
        token_estimate_before = record.token_estimate_before,
        token_estimate_after = ?record.token_estimate_after,
        target_tokens_after_compaction = ?record.target_tokens_after_compaction,
        pinned_token_estimate = ?record.pinned_token_estimate,
        budget_outcome = ?record.budget_outcome,
        trigger = TRIGGER,
        "appui manual compact_context finished"
    );
    let result = appui_manual_compaction_result(session_id, &record, None);
    lifecycle_notifications.push(appui_context_compaction_notification(
        session_id, &manager, &record,
    ));
    publish_appui_context_status(session_id, &manager);
    if let Err(error) =
        persist_context_manager_snapshot(data_dir, &session_id.to_string(), &manager)
    {
        warn!(
            session = %session_id.0,
            error = %error,
            "failed to persist appui context manager snapshot after manual compaction"
        );
    }
    (lifecycle_notifications, result)
}

fn appui_manual_compact_session(
    data_dir: &Path,
    session_id: &SessionKey,
    history: &[Message],
    llm_provider: &Arc<dyn octos_llm::LlmProvider>,
    llm_compaction_enabled: bool,
) -> Result<(Vec<UiNotification>, serde_json::Value), RpcError> {
    let persist_lock = appui_context_persist_lock(session_id);
    let _persist_guard = persist_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if live_appui_session_context_manager(session_id).is_some() {
        return Err(RpcError::invalid_request(format!(
            "session {} has an active turn; context compaction is deferred until the turn completes",
            session_id.0
        ))
        .with_data(serde_json::json!({
            "kind": "compaction_deferred_active_turn",
            "session_id": session_id.to_string(),
        })));
    }
    Ok(appui_force_compact_context(
        data_dir,
        session_id,
        history,
        llm_provider,
        llm_compaction_enabled,
    ))
}

/// `session/compact/mode/set`: set the per-session compaction-mode override
/// from the `/context` menu. Params `{ session_id, mode: "llm" | "heuristic" }`.
/// The override wins over the server `--llm-compaction` default for BOTH the
/// automatic compaction and manual `session/compact`.
fn handle_session_compact_mode_set(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
) -> Result<Value, RpcError> {
    let params: RawProfileParams = parse_raw_params(request)?;
    let Some(session_id) = params.session_id.clone() else {
        return Err(RpcError::invalid_params("session_id is required"));
    };
    let llm = match request.params.get("mode").and_then(Value::as_str) {
        Some("llm") => true,
        Some("heuristic") => false,
        _ => {
            return Err(RpcError::invalid_params(
                "mode is required and must be \"llm\" or \"heuristic\"",
            ));
        }
    };
    set_session_compaction_llm(&session_id, llm);
    Ok(json!({
        "session_id": session_id,
        "mode": session_compaction_mode_str(&session_id, state),
    }))
}

/// `session/compact`: force a context-compaction pass on an open session,
/// bypassing the auto-compaction threshold (powers the manual `/compact`
/// command). Resolves the already-open session runtime — a cache hit for the
/// active session — mirroring [`tool_status_list_result`]'s resolution, then
/// runs [`appui_force_compact_context`] and emits its lifecycle notifications
/// so clients render the compaction the same way they do the automatic pass.
async fn handle_session_compact(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    features: ConnectionUiFeatures,
    connection_profile_id: Option<&str>,
    request: &RpcRequest<Value>,
) -> Result<Value, RpcError> {
    let params: RawProfileParams = parse_raw_params(request)?;
    let Some(session_id) = params.session_id.clone() else {
        return Err(RpcError::invalid_params("session_id is required"));
    };
    let profile_id = raw_profile_id(&params, connection_profile_id);
    let Some(profile_runtime) = ensure_session_profile_runtime(state, Some(&profile_id)).await?
    else {
        return Err(runtime_unavailable_error(format!(
            "no runtime for profile {profile_id}; cannot compact session {session_id}"
        )));
    };
    // Epoch BEFORE permission resolution (codex #1639). For an already-open
    // session this is a cache hit that returns the existing runtime.
    let permissions_epoch = state.session_cache.session_generation(&session_id);
    let permissions = effective_permissions_for_session(state, &session_id)?;
    let workspace_hint = session_workspaces().runtime_hint(&profile_id, &session_id);
    let session_runtime = state
        .session_cache
        .get_or_init_with_permissions(
            &profile_runtime,
            session_id.clone(),
            workspace_hint,
            permissions,
            permissions_epoch,
        )
        .await
        .map_err(|error| {
            runtime_unavailable_error(format!("failed to resolve session runtime: {error}"))
        })?;

    let llm_provider = session_runtime.profile.llm.clone();
    // #1666: the per-project ledger lives under `sessions_root`, not
    // `profile.data_dir` — the forced path must touch the SAME ledger the
    // pre-turn path uses.
    let data_dir = session_runtime.sessions_root.clone();
    let history: Vec<Message> = {
        let mut sessions = session_runtime.sessions.lock().await;
        let session = sessions.get_or_create(&session_id).await;
        // Forced compaction has the same source-of-truth requirement as turn
        // start: a bounded tail cannot prove that a persisted ledger covers
        // the canonical session head.
        session.messages.clone()
    };

    let (notifications, result) = appui_manual_compact_session(
        &data_dir,
        &session_id,
        &history,
        &llm_provider,
        session_compaction_llm_enabled(&session_id, state),
    )?;
    for notification in notifications {
        if features.context_lifecycle_available() {
            let _ = send_notification_durable(ws, ledger, notification);
        } else {
            let _ = ledger.append_notification_from(notification, ws.connection_id);
        }
    }
    Ok(result)
}

fn prompt_message_matches(left: &Message, right: &Message) -> bool {
    left.role == right.role
        && left.content == right.content
        && left.media == right.media
        && left.reasoning_content == right.reasoning_content
        && left.tool_call_id == right.tool_call_id
        && tool_call_slices_match(left.tool_calls.as_deref(), right.tool_calls.as_deref())
}

fn record_prompt_messages_not_covered_by_context(
    manager: &mut ContextManager,
    policy: &PromptBuildPolicy,
    messages: &[Message],
) {
    let known_messages = manager.for_prompt(policy).messages;
    let covered = covered_prompt_message_indices(messages, &known_messages);
    for (index, message) in messages.iter().enumerate() {
        if covered[index] {
            continue;
        }
        // Skip System messages: the agent freshly composes its runtime
        // System prompt on every turn via `compose_system_prompt()` and
        // prepends it to `messages[0]`. Recording that as a
        // `SystemInstruction` item makes the manager accumulate one
        // duplicate per turn — `for_prompt` then re-emits all of them
        // into every LLM call, which `normalize_system_messages` later
        // concatenates into a single 4×-bloated `messages[0]`. The
        // runtime System is re-applied at the end of `prepare_prompt`,
        // so dropping it here does not lose it. (Regression introduced
        // by commit 28552bb9d which added the AppUI/gateway bridges
        // without exempting the per-turn runtime System.)
        if message.role == MessageRole::System {
            continue;
        }
        manager.record_message(message);
    }
}

fn covered_prompt_message_indices(messages: &[Message], known_messages: &[Message]) -> Vec<bool> {
    let mut covered = vec![false; messages.len()];
    if known_messages.is_empty() || known_messages.len() > messages.len() {
        return covered;
    }
    let Some(start) = messages.windows(known_messages.len()).position(|window| {
        window
            .iter()
            .zip(known_messages.iter())
            .all(|(left, right)| prompt_message_matches(left, right))
    }) else {
        return covered;
    };
    for slot in covered.iter_mut().skip(start).take(known_messages.len()) {
        *slot = true;
    }
    covered
}

fn tool_call_slices_match(
    left: Option<&[octos_core::ToolCall]>,
    right: Option<&[octos_core::ToolCall]>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) if left.len() == right.len() => {
            left.iter().zip(right.iter()).all(|(left, right)| {
                left.id == right.id
                    && left.name == right.name
                    && left.arguments == right.arguments
                    && left.metadata == right.metadata
            })
        }
        _ => false,
    }
}

#[derive(Clone)]
struct AppUiLoopPromptScratch {
    manager: ContextManager,
    observed_messages: usize,
    /// Cached runtime System captured at TurnStart. Reused on every
    /// `Iteration` phase within the same turn so the merge logic does
    /// not re-capture an already-merged `messages[0]` (which would
    /// duplicate the compaction summary content on each iteration).
    /// Cleared/replaced when a new TurnStart phase fires.
    runtime_system: Option<Message>,
    /// Highest durable source sequence absorbed from the canonical manager.
    /// Rows committed by a concurrent mid-turn path after this watermark are
    /// adopted before the next prompt projection and before scratch copyback.
    source_watermark: Option<usize>,
}

/// Delivery hook for mid-turn (in-loop) compaction lifecycle notifications.
/// Captures the turn's `WsConnection` + ledger + negotiated features so the
/// (sync) bridge can reach the client without owning connection state.
type ContextLifecycleNotify = Arc<dyn Fn(UiNotification) + Send + Sync>;

struct AppUiPromptContextBridge {
    session_id: SessionKey,
    data_dir: PathBuf,
    context_manager: Arc<StdMutex<ContextManager>>,
    scratch: StdMutex<Option<AppUiLoopPromptScratch>>,
    /// UPCR-2026-026 follow-up: the in-loop compaction pass previously ran
    /// SILENTLY (tracing + passive status store only), so a session whose
    /// context fills mid-turn never surfaced any compaction UX — and the
    /// compacted snapshot it persisted then starved the pre-turn (emitting)
    /// site forever. When set, the threshold block in
    /// [`Self::prepare_prompt`] emits `ContextCompactionStarted`/`Completed`
    /// through this hook. `None` in tests and paths without a client.
    context_lifecycle_notify: Option<ContextLifecycleNotify>,
    /// Provider for the OPT-IN LLM-summarization compaction path
    /// (`--llm-compaction` serve flag). `None` = heuristic only (also the
    /// fallback whenever an LLM summary fails, and the flag-off state). Set on
    /// the per-turn bridge only when the flag is on; child/spawn bridges leave
    /// it `None`.
    llm_compaction_provider: Option<Arc<dyn octos_llm::LlmProvider>>,
}

impl AppUiPromptContextBridge {
    fn new(
        session_id: SessionKey,
        data_dir: PathBuf,
        context_manager: Arc<StdMutex<ContextManager>>,
    ) -> Self {
        Self {
            session_id,
            data_dir,
            context_manager,
            scratch: StdMutex::new(None),
            context_lifecycle_notify: None,
            llm_compaction_provider: None,
        }
    }

    fn with_context_lifecycle_notify(mut self, notify: ContextLifecycleNotify) -> Self {
        self.context_lifecycle_notify = Some(notify);
        self
    }

    fn with_llm_compaction_provider(mut self, provider: Arc<dyn octos_llm::LlmProvider>) -> Self {
        self.llm_compaction_provider = Some(provider);
        self
    }

    /// Policy for the FINAL outgoing projection (`for_prompt`) sent to the
    /// model. Kept separate from `prompt_policy` because the coverage/record
    /// and compaction passes MUST run uncapped — capping their `for_prompt`
    /// view would make the manager treat trimmed-off older messages as
    /// "not covered" and re-record them, duplicating the persisted transcript.
    fn outgoing_prompt_policy(&self, request: &PromptContextRequest) -> PromptBuildPolicy {
        Self::prompt_policy(request)
    }

    fn threshold_tokens(request: &PromptContextRequest) -> usize {
        std::env::var("OCTOS_CONTEXT_COMPACT_THRESHOLD_TOKENS")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or_else(|| {
                (request.context_window as usize * APPUI_CONTEXT_COMPACT_RATIO_NUMERATOR
                    / APPUI_CONTEXT_COMPACT_RATIO_DENOMINATOR)
                    .max(1)
            })
    }

    fn prompt_policy(request: &PromptContextRequest) -> PromptBuildPolicy {
        PromptBuildPolicy {
            include_reasoning: false,
            supports_media: true,
            max_prompt_token_estimate: None,
            model_capability_id: format!("{}/{}", request.provider_name, request.model_id),
        }
    }

    fn adopt_canonical_source_rows(
        &self,
        scratch: &mut AppUiLoopPromptScratch,
        canonical: &ContextManager,
    ) -> usize {
        let adopted = scratch
            .manager
            .adopt_source_items_after(canonical, scratch.source_watermark);
        scratch.source_watermark = scratch
            .source_watermark
            .max(canonical.source_high_watermark());
        if !adopted.is_empty() {
            debug!(
                session = %self.session_id.0,
                adopted = adopted.len(),
                source_watermark = ?scratch.source_watermark,
                "appui scratch adopted mid-turn canonical context rows"
            );
        }
        adopted.len()
    }
}

impl PromptContextManager for AppUiPromptContextBridge {
    fn prepare_prompt(
        &self,
        request: PromptContextRequest,
        messages: &mut Vec<Message>,
    ) -> Result<PromptContextReport, String> {
        let messages_before = messages.len();
        let policy = Self::prompt_policy(&request);
        let mut scratch_guard = self
            .scratch
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if request.phase == PromptContextPhase::TurnStart || scratch_guard.is_none() {
            let mut manager = self
                .context_manager
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            record_prompt_messages_not_covered_by_context(&mut manager, &policy, messages);
            let source_watermark = manager.source_high_watermark();
            *scratch_guard = Some(AppUiLoopPromptScratch {
                manager,
                observed_messages: messages.len(),
                runtime_system: None,
                source_watermark,
            });
        }
        let scratch = scratch_guard
            .as_mut()
            .ok_or_else(|| "appui prompt context scratch was not initialized".to_string())?;
        if request.phase != PromptContextPhase::TurnStart
            && scratch.observed_messages < messages.len()
        {
            for message in messages.iter().skip(scratch.observed_messages) {
                scratch.manager.record_message(message);
            }
        } else if scratch.observed_messages > messages.len() {
            scratch.observed_messages = messages.len();
        }
        // Absorb durable rows committed since the last sync after recording
        // loop-local messages, allowing a durable twin to stamp an in-flight
        // row instead of being duplicated.
        {
            let canonical = self
                .context_manager
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.adopt_canonical_source_rows(scratch, &canonical);
        }

        let threshold = Self::threshold_tokens(&request);
        let mut compaction_performed = false;
        // UPCR-2026-026 follow-up: surface the MID-TURN pass to the client.
        // This is where compaction actually happens for a session whose
        // context fills during a long (multi-agent) turn — and the compacted
        // snapshot persisted below starves the pre-turn (emitting) site, so
        // without these events the user never sees any compaction UX at all.
        // Events are COLLECTED here and emitted only after `scratch_guard`
        // drops (codex round-2): on the stdio transport the delivery path can
        // block on a bounded SyncSender under backpressure, and a blocking
        // send while holding the scratch mutex would stall the bridge.
        let mut lifecycle_events: Vec<UiNotification> = Vec::new();
        let compaction_pass = if scratch.manager.should_auto_compact(threshold) {
            let budgets = appui_compaction_budgets(threshold);
            let trigger = format!("agent_loop:{}", request.phase.as_str());
            let compact_policy = appui_semantic_compact_policy(trigger.clone(), budgets);
            scratch
                .manager
                .should_retry_compaction(&compact_policy)
                .then_some((trigger, budgets.summary_budget, compact_policy))
        } else {
            None
        };
        if let Some((trigger, summary_budget, compact_policy)) = compaction_pass {
            if self.context_lifecycle_notify.is_some() {
                lifecycle_events.push(UiNotification::ContextCompactionStarted(
                    ContextCompactionStartedEvent {
                        session_id: self.session_id.clone(),
                        context_state: ui_context_state_for(&self.session_id, &scratch.manager),
                        trigger: trigger.clone(),
                        threshold_tokens: threshold,
                    },
                ));
            }
            let summary_messages = scratch.manager.compaction_input(&compact_policy, &policy);
            let record = if summary_messages.messages.is_empty() {
                scratch.manager.record_failed_compaction(
                    compact_policy,
                    "no closed semantic prefix is safe to compact",
                )
            } else {
                let summary = match &self.llm_compaction_provider {
                    Some(provider) => {
                        appui_compaction_summary(provider, &summary_messages, summary_budget)
                    }
                    None => summary_messages.compact_summary(summary_budget),
                };
                let record = scratch.manager.compact_context(summary, compact_policy);
                compaction_performed = record.output_generation.is_some();
                record
            };
            if self.context_lifecycle_notify.is_some() {
                lifecycle_events.push(appui_context_compaction_notification(
                    &self.session_id,
                    &scratch.manager,
                    &record,
                ));
            }
            info!(
                session = %self.session_id.0,
                phase = request.phase.as_str(),
                iteration = request.iteration,
                compaction_id = %record.compaction_id.as_str(),
                checkpoint_id = %record.checkpoint_id.as_str(),
                token_estimate_before = record.token_estimate_before,
                token_estimate_after = ?record.token_estimate_after,
                target_tokens_after_compaction = ?record.target_tokens_after_compaction,
                pinned_token_estimate = ?record.pinned_token_estimate,
                budget_outcome = ?record.budget_outcome,
                status = ?record.status,
                "appui context manager semantic compaction finished for in-loop model prompt"
            );
            publish_appui_context_status(&self.session_id, &scratch.manager);
        }

        // Capture the caller's runtime System ONCE per turn at
        // TurnStart, then reuse across iterations. Pre-fix the bridge
        // re-captured `messages[0]` on every iteration, but after the
        // first in-place merge `messages[0]` already contained
        // `runtime + compaction_summary`. Re-capturing that on the
        // next iteration and merging again with a fresh frame summary
        // duplicated the summary. The TurnStart cache holds the
        // original, untainted runtime System for the rest of the turn.
        if request.phase == PromptContextPhase::TurnStart {
            scratch.runtime_system = messages
                .first()
                .filter(|m| m.role == MessageRole::System)
                .cloned();
        }
        let runtime_system = scratch.runtime_system.clone();
        // Project the OUTGOING prompt with the (possibly voice-capped) policy.
        // `policy` above stays uncapped for the record/coverage/compaction
        // passes so the persisted transcript is never re-recorded from a
        // trimmed view.
        let out_policy = self.outgoing_prompt_policy(&request);
        let frame = scratch.manager.for_prompt(&out_policy);
        let prompt_replaced = messages.len() != frame.messages.len()
            || messages
                .iter()
                .zip(frame.messages.iter())
                .any(|(left, right)| !prompt_message_matches(left, right));
        *messages = frame.messages;
        if let Some(system) = runtime_system {
            // Re-apply the agent's runtime System prompt. Two cases:
            //
            //   a) `frame.messages` leads with a System message.
            //      Concatenate the runtime System CONTENT into that
            //      existing System rather than inserting a second one.
            //      We do this in-place to avoid producing two
            //      consecutive `System` messages because
            //      `normalize_system_messages` runs BEFORE this bridge
            //      (`loop_compaction.rs:35`) — anything we emit here
            //      goes straight to the provider, and multi-System
            //      payloads are rejected by Anthropic (single `system`
            //      field) and other providers. Since compaction
            //      summaries render as protected User rows (see
            //      `for_prompt`'s CompactionSummary arm), this arm is
            //      effectively a legacy guard.
            //
            //   b) `frame.messages` does not lead with a System (the
            //      normal case, including post-compaction where the
            //      frame leads with the User-role summary). Insert the
            //      runtime System at index 0.
            //
            // Safe to merge unconditionally because
            // `record_message_with_source_ref` early-returns for
            // `System` role (`context_manager.rs:752`) so the frame
            // never contains the runtime System itself.
            match messages.first_mut() {
                Some(first) if first.role == MessageRole::System => {
                    let existing = std::mem::take(&mut first.content);
                    first.content = if existing.is_empty() {
                        system.content
                    } else {
                        format!("{}\n\n{}", system.content, existing)
                    };
                }
                _ => messages.insert(0, system),
            }
        }
        scratch.observed_messages = messages.len();
        {
            let mut canonical = self
                .context_manager
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.adopt_canonical_source_rows(scratch, &canonical);
            *canonical = scratch.manager.clone();
            publish_appui_context_status(&self.session_id, &canonical);
            if let Err(error) =
                persist_appui_context_snapshot(&self.data_dir, &self.session_id, &canonical)
            {
                warn!(
                    session = %self.session_id.0,
                    error = %error,
                    "failed to persist appui prompt context manager snapshot"
                );
            }
        }
        let report = PromptContextReport {
            prompt_replaced,
            compaction_performed,
            messages_before,
            messages_after: messages.len(),
            token_estimate: Some(frame.report.token_estimate),
            generation: Some(frame.context_state.generation),
        };
        // Emit AFTER releasing the scratch lock (see the collection comment
        // above): a blocking stdio send while holding `scratch` would stall
        // any concurrent bridge user. Wire order (started → completed, one
        // delivery batch) matches the pre-turn site, which also returns both
        // events together.
        drop(scratch_guard);
        if let Some(notify) = self.context_lifecycle_notify.as_ref() {
            for event in lifecycle_events {
                notify(event);
            }
        }
        Ok(report)
    }

    fn prompt_cache_epoch_id(&self) -> Option<String> {
        self.context_manager
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cache_epoch()
            .map(|epoch| epoch.epoch_id.clone())
    }

    fn observe_effective_provider_route(&self, provider_name: &str, model_id: &str) {
        // Keep the per-loop scratch and durable canonical manager coherent.
        // `prepare_prompt` locks in this same order before copying scratch back
        // to canonical; rotating only canonical here would therefore be undone
        // on the very next model iteration after a failover.
        let snapshot = {
            let mut scratch_guard = self
                .scratch
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let scratch_rotated = scratch_guard.as_mut().is_some_and(|scratch| {
                scratch
                    .manager
                    .observe_effective_provider_route(provider_name, model_id)
            });
            let mut canonical = self
                .context_manager
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let canonical_rotated =
                canonical.observe_effective_provider_route(provider_name, model_id);
            (scratch_rotated || canonical_rotated).then(|| canonical.clone())
        };

        let Some(snapshot) = snapshot else {
            return;
        };
        let epoch = snapshot
            .cache_epoch()
            .expect("an effective-route rotation retains an initialized epoch");
        tracing::info!(
            session = %self.session_id.0,
            provider = provider_name,
            model = model_id,
            epoch_id = %epoch.epoch_id,
            "appui prompt cache epoch rotated to effective provider route"
        );
        publish_appui_context_status(&self.session_id, &snapshot);
        if let Err(error) =
            persist_appui_context_snapshot(&self.data_dir, &self.session_id, &snapshot)
        {
            warn!(
                session = %self.session_id.0,
                error = %error,
                "failed to persist effective provider-route cache epoch"
            );
        }
    }
}

fn record_appui_context_manager_message(
    data_dir: &Path,
    context_manager: &Arc<StdMutex<ContextManager>>,
    session_id: &SessionKey,
    message: &Message,
    seq: usize,
) {
    let mut manager = context_manager
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let ids = manager.record_persisted_message_merging_prompt_equivalent(message, seq);
    let state = manager.state();
    tracing::debug!(
        session = %session_id.0,
        seq,
        role = message.role.as_str(),
        generated_items = ids.len(),
        generation = state.generation,
        transcript_hash = %state.transcript_hash,
        "appui context manager recorded persisted session message"
    );
    publish_appui_context_status(session_id, &manager);
    if let Err(error) = persist_appui_context_snapshot(data_dir, session_id, &manager) {
        warn!(
            session = %session_id.0,
            error = %error,
            "failed to persist appui context manager snapshot"
        );
    }
}

fn merge_appui_background_row(
    manager: &mut ContextManager,
    session_id: &SessionKey,
    message: &Message,
    seq: usize,
) {
    let ids = manager.record_persisted_message_merging_prompt_equivalent(message, seq);
    manager.mark_source_event_kind(&ids, "background_result");
    debug!(
        session = %session_id.0,
        seq,
        generated_items = ids.len(),
        "appui context manager recorded background row"
    );
}

fn record_appui_context_manager_background_message(
    data_dir: &Path,
    fallback_context_manager: &Arc<StdMutex<ContextManager>>,
    session_id: &SessionKey,
    message: &Message,
    seq: usize,
) {
    if let Some(live) = live_appui_session_context_manager(session_id) {
        let mut manager = live.lock().unwrap_or_else(|error| error.into_inner());
        merge_appui_background_row(&mut manager, session_id, message, seq);
        publish_appui_context_status(session_id, &manager);
        if let Err(error) = persist_appui_context_snapshot(data_dir, session_id, &manager) {
            warn!(
                session = %session_id.0,
                error = %error,
                "failed to persist appui background context boundary"
            );
        }
        return;
    }

    {
        let persist_lock = appui_context_persist_lock(session_id);
        let _persist_guard = persist_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match load_context_manager_snapshot(data_dir, &session_id.to_string()) {
            Ok(Some(mut manager)) => {
                merge_appui_background_row(&mut manager, session_id, message, seq);
                publish_appui_context_status(session_id, &manager);
                if let Err(error) =
                    persist_context_manager_snapshot(data_dir, &session_id.to_string(), &manager)
                {
                    warn!(
                        session = %session_id.0,
                        error = %error,
                        "failed to persist appui background context boundary"
                    );
                }
                return;
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    session = %session_id.0,
                    error = %error,
                    "context ledger snapshot unreadable; merging background row into the last per-turn manager"
                );
            }
        }
    }

    let mut manager = fallback_context_manager
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    merge_appui_background_row(&mut manager, session_id, message, seq);
    publish_appui_context_status(session_id, &manager);
    if let Err(error) = persist_appui_context_snapshot(data_dir, session_id, &manager) {
        warn!(
            session = %session_id.0,
            error = %error,
            "failed to persist appui background context boundary"
        );
    }
}

/// Register the per-project ledger storage scope for a just-materialized
/// [`SessionRuntime`] — the UI-protocol half of `appui.sessions_in_cwd`
/// isolation (#1666).
///
/// The ledger is owned by the application runtime, rooted at its data dir, and
/// it keys each session's ring/dir by the session id alone. With
/// `sessions_in_cwd` the SAME wire id can belong to different projects, so a
/// relocated session (`sessions_root != profile.data_dir`) registers a
/// 16-hex digest of its canonical `sessions_root`; the ledger then keeps that
/// project's events under a distinct storage identity. Flag-OFF (or no cwd
/// hint) resolves `sessions_root == profile.data_dir` → scope `None` →
/// byte-identical legacy behavior (and clears a stale registration if the
/// flag was toggled off).
///
/// MUST be called before the flow replays the session (`session/open` calls
/// it right after the runtime cache materializes, before
/// `replay_after_with_head`) so replay and subsequent appends agree.
fn register_session_ledger_scope(
    state: &AppState,
    ledger: &UiProtocolLedger,
    runtime: &crate::runtime::SessionRuntime,
) {
    if let Some(observer) = state.ui_protocol.commit_observer.get() {
        octos_bus::set_scoped_message_commit_observer(&runtime.sessions_root, observer);
    }
    let scope = (runtime.sessions_root != runtime.profile.data_dir).then(|| {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(runtime.sessions_root.as_os_str().as_encoded_bytes());
        hasher.finalize()[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    });
    ledger.set_session_scope(&runtime.session_key, scope.clone());
    // #1666 residue — mirror the SAME cwd scope into the goal/autonomy store so
    // a goal set in one folder does not leak into a fresh session that reuses
    // this wire key in another folder. The goal store was keyed by the bare
    // wire key while #1666 only cwd-scoped the ledger, so the goal chip leaked
    // across folders (same profile). Keeping the two scope maps in lock-step
    // (registered at the same site, cleared together when the flag is off) is
    // what makes the goal store isolate cwds exactly as the transcript already
    // does. The goal continuation dispatch strips this scope back to the wire
    // key when it reaches the session runtime / actor.
    // Topic-suffixed sessions also emit ledger events under their BASE key:
    // the alpha-9 file/visual bridges deliberately strip the `#topic` before
    // appending so base-bucket subscribers see them (see
    // `ui_protocol_alpha9_bridge.rs`). Register the base alias with the same
    // scope so those appends land in the same per-project dir instead of the
    // unscoped legacy one (codex v2 P2). Same cwd → same scope, so this can
    // never mis-route a different project's base-key session.
    let base = runtime.session_key.base_key();
    if base != runtime.session_key.0 {
        ledger.set_session_scope(&SessionKey(base.to_owned()), scope);
    }
}

/// A multiplexed reconnect may have reopened a peer but not its master. A
/// turn/start carries no cwd: never let a known scoped master bootstrap with
/// a missing hint and fork a seq-1 bare ledger/history. Resume the scoped
/// session only after an explicit session/open. Historical
/// cwd metadata alone cannot restore ephemeral sandbox narrowing safely.
fn require_recovered_scoped_session_open(
    state: &AppState,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    profile_id: &str,
) -> Result<(), RpcError> {
    if !state.session_cache.sessions_in_cwd()
        || session_workspaces()
            .runtime_hint(profile_id, session_id)
            .is_some()
    {
        return Ok(());
    }
    if !ledger.has_recovered_scoped_history(session_id) {
        return Ok(());
    }
    Err(RpcError::invalid_request(
        "session/open with an explicit cwd is required before resuming this scoped session",
    )
    .with_data(json!({"kind":"session_open_required", "session_id":session_id})))
}

/// Event ledger owned by one application runtime.
///
/// First call decides the durability path:
/// - With a `data_dir` from `AppState.sessions`, builds a Path-A durable
///   ledger, runs disk recovery, and spawns the idle-eviction sweep.
/// - Without a sessions manager (unit tests, headless smoke), builds a
///   RAM-only ledger that still enforces the LRU + idle-TTL caps but
///   does not persist.
///
/// Connections within that runtime share the same Arc. Separate embedded
/// runtimes must never inherit the first instance's durability directory.
pub(super) async fn event_ledger(state: &AppState) -> Arc<UiProtocolLedger> {
    // The data_dir read is the only async step; it runs OUTSIDE the once-init
    // (two racers both computing it is benign — the loser's value is dropped).
    let data_dir = match &state.sessions {
        Some(sessions) => Some(sessions.lock().await.data_dir()),
        None => None,
    };
    let (installed, installed_now) =
        event_ledger_init_once(&state.ui_protocol.ledger, data_dir.clone());
    let observer = state
        .ui_protocol
        .commit_observer
        .get_or_init(|| message_commit_observer(installed.clone()));
    if let Some(root) = data_dir {
        octos_bus::set_scoped_message_commit_observer(&root, observer);
    }
    for profile in state.profiles.values() {
        octos_bus::set_scoped_message_commit_observer(&profile.data_dir, observer);
    }
    // The winning initializer starts exactly one sweep per runtime. Scoped
    // registrations above all retain this runtime's single commit observer.
    if installed_now {
        let _handle = spawn_eviction_task(installed.clone());
    }
    installed
}

/// #1973 fix-round — the once-init core of [`event_ledger`]: build the ledger
/// (running DISK RECOVERY for a durable config) strictly INSIDE the
/// `OnceLock::get_or_init` closure, which runs at most once per lock.
///
/// Before, recovery ran BEFORE `get_or_init`: two callers racing the first
/// initialization (the global master-continuation drain and the stdio
/// connection both call [`event_ledger`] at serve boot) could BOTH replay the
/// same on-disk JSONL and BOTH synthesize orphan-terminal records —
/// interleaving appends into the live log before one loser's ledger was
/// discarded. `get_or_init` blocks the losing racer until the winner's
/// closure returns; recovery is fast boot-time disk replay and runs once per
/// runtime, so briefly parking a second initializer is the correct trade
/// (and the pre-existing behavior already ran this same blocking I/O on the
/// async path).
///
/// Returns the installed ledger plus whether THIS call ran the init closure —
/// the caller uses that to spawn the eviction sweep / commit observer exactly
/// once. Extracted (with the `OnceLock` injected) so a test can race N
/// threads against a fresh lock and pin the exactly-once guarantee.
fn event_ledger_init_once(
    once: &OnceLock<Arc<UiProtocolLedger>>,
    data_dir: Option<PathBuf>,
) -> (Arc<UiProtocolLedger>, bool) {
    let mut installed_now = false;
    let installed = once
        .get_or_init(|| {
            installed_now = true;
            let config = match data_dir {
                Some(dir) => LedgerConfig::durable(dir),
                None => LedgerConfig::ephemeral(EVENT_LEDGER_RETAINED_PER_SESSION),
            };
            if config.data_dir.is_some() {
                let outcome = UiProtocolLedger::recover(config);
                info!(
                    target = "octos::ledger",
                    sessions_recovered = outcome.sessions_recovered,
                    events_recovered = outcome.events_recovered,
                    "ui protocol ledger initialized with durable backing"
                );
                outcome.ledger
            } else {
                Arc::new(UiProtocolLedger::with_config(config))
            }
        })
        .clone();
    (installed, installed_now)
}

/// Install the durable-commit observer that records every successful
/// `add_message_with_seq` commit as a canonical v2 projection envelope.
///
/// Per UPCR-2026-012 the observer fires AFTER `add_message_with_seq`'s
/// disk write returned Ok and the in-memory mirror was updated, so any
/// recorded notification always reflects a row that is durably visible.
/// A commit failure (size cap, fsync error) returns Err from
/// `append_to_disk` and the observer is skipped — the
/// "MUST NOT emit on commit failure" invariant.
///
/// The v2 emitter takes the per-session global lock, assigns the durable
/// cursor during append, and serializes concurrent commits in commit order.
///
/// Delivery model: the envelope is persisted to the ledger ring (disk +
/// in-memory). Clients receive the canonical v2 projection via two paths,
/// whichever wins the race: (a) cursor-based replay on
/// `session/open { after: <cursor> }`, or (b) the per-session live
/// publish-subscribe broadcast (`UiProtocolLedger::subscribe`) drained
/// by `spawn_live_forwarder` for currently connected WebSocket clients.
/// Both paths are reconciled by the forwarder's `baseline_seq` filter
/// (replay snapshot head) and `from_connection` self-suppression so
/// each event reaches each WS exactly once. Issue #760 / PR #761
/// closed the original "no live fan-out" gap; clients that go offline
/// still resync via cursor on reconnect.
/// Bounded channel capacity for the per-session `SendFileTool` sink. Each
/// session drains its own channel into the canonical-persist path, so 64
/// pending messages is generous; if a runaway tool ever exceeds this we'd
/// rather backpressure the agent loop than balloon memory.
const SEND_FILE_CHANNEL_CAPACITY: usize = 64;

/// Context supplied by the background-result path to the post-commit
/// observer. The observer is the only place that can turn a successful
/// canonical session write into a durable UI event, so this avoids a second
/// post-persist notification lane and keeps commit ordering authoritative.
#[derive(Clone)]
struct BackgroundChildProjection {
    parent_turn_id: String,
    response_to_client_message_id: Option<String>,
    task_id: Option<String>,
    tool_call_id: Option<String>,
    media: Vec<String>,
}

/// Projection disposition for one canonical message commit.
#[derive(Clone)]
enum MessageProjectionOverride {
    /// Exact assistant iteration supplied by the immutable Agent output log.
    AssistantSegment(String),
    /// Emit the committed row as a linked v2 background child stream.
    BackgroundChild(BackgroundChildProjection),
    /// The persisted row is a per-file companion already represented by the
    /// following background-child envelope, so keep it transcript-only.
    Suppress,
}

tokio::task_local! {
    static MESSAGE_PROJECTION_OVERRIDE: Option<MessageProjectionOverride>;
}

/// Pre-stamp `thread_id` on a row about to be persisted by the standalone
/// turn loop so every User/Assistant/Tool row from the same turn shares the
/// originating `TurnId`-derived thread id.
///
/// Caller-supplied `thread_id` values are preserved. System rows are left
/// alone (they aren't thread-scoped). For `User`/`Assistant`/`Tool` rows
/// missing a `thread_id`, the supplied `turn_thread_id` is stamped.
///
/// **M10 Phase 6.1**: extending this from Assistant/Tool only to also cover
/// `User` closes the empty-placeholder bubble. `process_message_inner`
/// builds the user row with `client_message_id: None`, so without the
/// pre-stamp `derive_thread_id_for_new_write` falls back to a fresh
/// `now_v7()` for the user row while assistant rows are stamped with the
/// `TurnId`. The SPA reducer keys threads on `thread_id`; a divergent user
/// thread leaves an empty pending bubble in the user's thread and creates
/// an orphan thread for the assistant rows.
fn pre_stamp_turn_thread_id(message: Message, turn_thread_id: &str) -> Message {
    let mut to_save = message;
    if to_save.thread_id.is_none()
        && matches!(
            to_save.role,
            MessageRole::User | MessageRole::Assistant | MessageRole::Tool
        )
    {
        to_save.thread_id = Some(turn_thread_id.to_owned());
    }
    to_save
}

/// Shared persist helper used by the api/serve background-result sender
/// (spawn_only completions) and the `send_file` sink. Builds an assistant
/// `Message` with the given content + media + thread_id, writes it through
/// the canonical session helper (which serialises with other writers via
/// the per-key Tokio mutex and triggers `MessageCommitObserver`), then
/// invalidates the cached `SessionManager` entry so subsequent
/// `session/hydrate` and `/api/sessions/:id/messages` reads pick up the
/// new row instead of the pre-persist snapshot. Mirrors the gateway's
/// `session_actor.rs::deliver_background_notification` post-write
/// invalidate at `api_channel.rs:1503`.
///
/// Returns the exact committed message and sequence on success so callers
/// that own an OUP ContextManager can advance its canonical source head in
/// the same commit path. `None` signals a persist failure (already logged).
async fn persist_assistant_with_media(
    sessions: &Arc<TokioMutex<octos_bus::SessionManager>>,
    data_dir: &Path,
    session_id: &SessionKey,
    content: String,
    media: Vec<String>,
    thread_id: String,
    label: &str,
) -> Option<(Message, usize)> {
    let mut message = Message::assistant_with_thread(content, octos_core::ThreadId::new(thread_id));
    message.media = media;
    let committed_message = message.clone();
    // Capture the stamped timestamp BEFORE the canonical persist
    // consumes the message — `MessageCommitObserver` derives the wire
    // `message_id` from `(session_id, committed_seq, message.timestamp)`
    // for the canonical background-child payload.
    let committed_seq = match octos_bus::session::persist_message_through_canonical_path(
        data_dir, session_id, message,
    )
    .await
    {
        Ok(seq) => seq,
        Err(error) => {
            tracing::warn!(
                session = %session_id.0,
                label,
                error = %error,
                "api/serve: failed to persist background-delivered message"
            );
            return None;
        }
    };

    sessions.lock().await.invalidate_cache(session_id);
    Some((committed_message, committed_seq))
}

/// M9-γ-7 (issue #844): the agent loop's iterative tool-calling pattern
/// commits an Assistant `Message` per LLM iteration. When the LLM returns
/// only `tool_calls` (no text content) and no media — the metadata-only
/// shape that bracketed every `tool/started` → `tool/completed` cycle —
/// the persisted row is invisible to the user but still triggers
/// `MessageCommitObserver`. Pre-fix the ledger emitted N
/// assistant-persisted projection envelopes per turn for an N-iteration loop, all
/// carrying the same `thread_id`. The web reducer keyed off `thread_id`
/// merged them into a "phantom" empty assistant bubble that briefly
/// flickered into the chat pane (the 2026-05-09 phantom-bubble bug).
///
/// The defensive web-side fix in octos-web #92 hid those bubbles. The
/// authoritative server-side fix is to suppress the v2 assistant-persisted
/// emit for these intermediate metadata-only assistant rows so the wire
/// surface emits exactly one canonical assistant-persisted envelope per turn for the final
/// user-visible assistant text.
///
/// Filter: skip emission when the row is `Assistant`, content is
/// empty after `trim()`, and `media` is empty. Tool messages (role
/// `Tool`) and assistant rows with text or media are unaffected. Once
/// the SSE chat path is deleted (α-5/α-6) and the WS turn loop is sole
/// transport, this filter remains correct because the filtering criteria
/// describe a metadata-only row (no rendering surface) regardless of
/// transport.
fn is_metadata_only_assistant_row(message: &octos_core::Message) -> bool {
    message.role == octos_core::MessageRole::Assistant
        && message.content.trim().is_empty()
        && message.media.is_empty()
}

fn message_commit_observer(ledger: Arc<UiProtocolLedger>) -> octos_bus::MessageCommitObserver {
    let observer: octos_bus::MessageCommitObserver =
        Arc::new(move |session_key, message, committed_seq| {
            if is_metadata_only_assistant_row(message) {
                return;
            }
            let projection_override = MESSAGE_PROJECTION_OVERRIDE
                .try_with(|value| value.clone())
                .ok()
                .flatten();
            if matches!(
                &projection_override,
                Some(MessageProjectionOverride::Suppress)
            ) {
                return;
            }

            let message_id = format!(
                "{}:{committed_seq}:{}",
                session_key.0,
                message.timestamp.timestamp_nanos_opt().unwrap_or(0)
            );
            if let Some(MessageProjectionOverride::BackgroundChild(context)) = projection_override {
                let task_id = context
                    .task_id
                    .unwrap_or_else(|| format!("untracked-{message_id}"));
                let child_stream_id = format!("{}:background:{task_id}", context.parent_turn_id);
                let _ = ledger.emit_envelope_v2(
                    session_key,
                    child_stream_id,
                    PayloadV2::BackgroundChildCompleted {
                        parent_turn_id: context.parent_turn_id,
                        response_to_client_message_id: context.response_to_client_message_id,
                        task_id,
                        content: message.content.clone(),
                        tool_call_id: context.tool_call_id,
                        message_id,
                        source: "background".to_owned(),
                        persisted_at: message.timestamp,
                        media: context.media,
                    },
                    None,
                );
                return;
            }

            let Some(thread_id) = message.thread_id.clone() else {
                return;
            };
            match message.role {
                MessageRole::Assistant => {
                    let assistant_segment_id = match projection_override {
                        Some(MessageProjectionOverride::AssistantSegment(identity)) => identity,
                        // Legacy/other canonical writers provide no provable stream
                        // correlation. Preserve their unique durable row identity;
                        // never let one uncorrelated row finalize another's bubble.
                        _ => format!("{thread_id}:assistant:canonical:{message_id}"),
                    };
                    let payload = PayloadV2::AssistantPersisted {
                        text: message.content.clone(),
                        assistant_segment_id,
                        meta: MessageMeta {
                            message_id,
                            persisted_at: message.timestamp,
                            media: message.media.clone(),
                        },
                    };
                    let _ = ledger.emit_envelope_v2(session_key, thread_id, payload, None);
                }
                MessageRole::User => {
                    let files: Vec<FileRef> = message
                        .media
                        .iter()
                        .map(|path| {
                            let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                            FileRef {
                                path: path.clone(),
                                mime: "application/octet-stream".into(),
                                size_bytes,
                            }
                        })
                        .collect();
                    let payload = PayloadV2::UserMessage {
                        text: message.content.clone(),
                        files,
                    };
                    let _ = ledger.emit_envelope_v2(
                        session_key,
                        thread_id,
                        payload,
                        message.client_message_id.clone(),
                    );
                }
                MessageRole::Tool | MessageRole::System => {
                    // Tool lifecycle remains represented by its canonical
                    // ToolStart/ToolProgress/ToolEnd projection path; system
                    // rows do not render in chat.
                }
            }
        });
    observer
}

/// Process-global pending diff-preview store. Mirrors
/// [`event_ledger`]'s lazy initialization: with a `data_dir` from the
/// sessions manager, the first call hydrates from disk and installs a
/// durable store; without one we install an ephemeral fallback.
/// Subsequent calls return the same `Arc` regardless of the
/// `state` they're given — by design, the store is process-singleton.
async fn diff_preview_store(
    state: &AppState,
    contracts: &UiProtocolContractStores,
) -> Arc<PendingDiffPreviewStore> {
    let data_dir = match &state.sessions {
        Some(sessions) => Some(sessions.lock().await.data_dir()),
        None => None,
    };
    contracts.diff_previews(data_dir.as_deref())
}

struct AbortOnDrop {
    abort: AbortHandle,
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

struct BoundedChannelReporter {
    tx: tokio::sync::mpsc::Sender<String>,
    /// Mirrors WS-layer drops: when the progress channel is full the agent
    /// produced an event the WS layer will never see. Without this counter
    /// the cursor would lie. Surfaced opportunistically as `protocol/replay_lossy`
    /// from the consuming task.
    progress_dropped: Arc<AtomicU64>,
    /// PR F (M8.10 thread-binding): bound `thread_id` for every progress
    /// event this reporter emits. Set once at turn-start to the originating
    /// `TurnId`; from then on every JSON payload carries `thread_id` so the
    /// SPA reducer can demultiplex without a sticky-map fallback. `None`
    /// preserves the legacy untagged path for callers that haven't migrated.
    thread_id: Option<String>,
}

impl BoundedChannelReporter {
    fn new(tx: tokio::sync::mpsc::Sender<String>, progress_dropped: Arc<AtomicU64>) -> Self {
        Self {
            tx,
            progress_dropped,
            thread_id: None,
        }
    }

    /// PR F: bind a `thread_id` to this reporter. Typically the originating
    /// `TurnId` (the `params.turn_id` passed into `run_standalone_turn`),
    /// stamped into every emitted SSE payload so wire events are routed
    /// to the right per-turn bubble on the client.
    fn with_thread_id(mut self, thread_id: Option<String>) -> Self {
        self.thread_id = thread_id.filter(|s| !s.is_empty());
        self
    }
}

impl octos_agent::ProgressReporter for BoundedChannelReporter {
    fn report(&self, event: octos_agent::ProgressEvent) {
        let json = match serde_json::to_string(&super::events::event_to_json(
            &event,
            self.thread_id.as_deref(),
        )) {
            Ok(json) => json,
            Err(_) => return,
        };
        if let Err(err) = self.tx.try_send(json) {
            self.progress_dropped.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("ws.send.drop.backpressure", "method" => "progress").increment(1);
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                reason = ?err,
                "progress event dropped before reaching ws layer"
            );
        }
    }

    /// Issue #960 fix: expose the per-turn `thread_id` bound at
    /// construction (`with_thread_id(Some(turn_id.0.to_string()))`) so
    /// `agent/execution.rs`'s spawn_only intercept can capture it as
    /// `bg_originating_client_message_id` and surface it on the
    /// `turn/spawn_complete` envelope's
    /// `response_to_client_message_id`. Without this override the
    /// default `None` implementation strips the binding even though the
    /// struct field is populated, and the SPA reducer's thread-map
    /// lookup falls through and silently drops the completion bubble.
    fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }
}

/// Forward a `BackgroundTask` snapshot from `TaskSupervisor::set_on_change`
/// into the per-turn progress channel.
///
/// **Terminal updates** (`completed` / `failed` / `cancelled`) MUST NOT be
/// dropped under WebSocket backpressure — dropping one leaves the UI
/// stuck on `running` indefinitely even though the agent has long since
/// moved on (M9 review finding #6). For these, a `try_send` failure
/// upgrades to a spawned `tx.send().await` with a [`TERMINAL_TASK_SEND_TIMEOUT`]
/// budget so the update is durable through ordinary backpressure but does
/// not pile up zombies if the consumer is permanently gone.
///
/// **Non-terminal updates** are coalesce-friendly: the next update will
/// overwrite, so a drop has no correctness impact and we keep the
/// non-blocking `try_send` fast-path.
///
/// `progress_dropped` increments on the immediate `try_send` failure (so
/// the `protocol/replay_lossy` machinery is informed), regardless of
/// terminal status. The dedicated `ws.send.timeout.terminal` metric fires
/// only when even the awaited send hits the timeout — i.e., the case the
/// fix exists to make observable.
fn forward_task_progress_to_channel(
    tx: &tokio::sync::mpsc::Sender<String>,
    progress_dropped: &Arc<AtomicU64>,
    task: &octos_agent::BackgroundTask,
    _runtime_profile_id: Option<&str>,
) {
    let event = background_task_to_progress_json(task);
    let Ok(json) = serde_json::to_string(&event) else {
        return;
    };
    forward_task_progress_json_to_channel(tx, progress_dropped, task, "task_progress", json);
}

fn forward_task_progress_json_to_channel(
    tx: &tokio::sync::mpsc::Sender<String>,
    progress_dropped: &Arc<AtomicU64>,
    task: &octos_agent::BackgroundTask,
    method: &'static str,
    json: String,
) {
    if tx.try_send(json.clone()).is_ok() {
        return;
    }
    progress_dropped.fetch_add(1, Ordering::Relaxed);
    metrics::counter!("ws.send.drop.backpressure", "method" => method).increment(1);
    if !task.status.is_terminal() {
        // Non-terminal: drop is fine, next update overwrites.
        return;
    }
    // Terminal: spawn a durable awaited send. The runtime owns the JoinHandle,
    // so this survives the sync callback returning. A `tx.send().await` failure
    // means the receiver was dropped (turn over) — nothing to deliver to. The
    // timeout protects against a permanently-stuck consumer.
    let tx = tx.clone();
    let task_id = task.id.clone();
    let lifecycle = task.lifecycle_state();
    tokio::spawn(async move {
        match tokio::time::timeout(TERMINAL_TASK_SEND_TIMEOUT, tx.send(json)).await {
            Ok(Ok(())) => {}
            Ok(Err(_send_err)) => {
                // Receiver dropped; nothing observable to deliver. Not a bug.
                tracing::debug!(
                    target: "octos::ui_protocol::ws",
                    %task_id,
                    ?lifecycle,
                    "terminal task update dropped: progress receiver gone"
                );
            }
            Err(_elapsed) => {
                metrics::counter!(
                    "ws.send.timeout.terminal",
                    "method" => method
                )
                .increment(1);
                tracing::warn!(
                    target: "octos::ui_protocol::ws",
                    %task_id,
                    ?lifecycle,
                    timeout_ms = TERMINAL_TASK_SEND_TIMEOUT.as_millis() as u64,
                    "terminal task update timed out under sustained backpressure"
                );
            }
        }
    });
}

struct UiProtocolApprovalRequester {
    ws: WsConnection,
    ledger: Arc<UiProtocolLedger>,
    contracts: Arc<UiProtocolContractStores>,
    /// Held so the FIX-07 audit log can resolve `<data_dir>/audit/` from
    /// `state.sessions.lock().data_dir()` on the auto-resolved decision
    /// path (and any future direct-decision paths).
    state: Arc<AppState>,
    session_id: SessionKey,
    turn_id: TurnId,
    features: ConnectionUiFeatures,
}

#[async_trait::async_trait]
impl octos_agent::ToolApprovalRequester for UiProtocolApprovalRequester {
    async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        let approval_id = ApprovalId::new();
        let event = approval_event_from_tool_request(
            request,
            self.session_id.clone(),
            approval_id.clone(),
            self.turn_id.clone(),
            self.features,
        );

        // Scope-policy short circuit: if the user previously chose
        // `approve_for_*` for a matching tool/turn/session, resolve this
        // approval automatically. Emit BOTH:
        //   1. `approval/auto_resolved` (FIX-06): informational, carries
        //      the scope/match identifiers so the client can reason about
        //      *why* the request did not surface.
        //   2. `approval/decided` (FIX-07): the canonical durable record
        //      of the decision; flagged with `auto_resolved = true` and
        //      a `policy_id` so audit/replay treat it identically to a
        //      manual decision.
        // The audit log writer also runs here so auto-resolved decisions
        // appear in the JSON-Lines log next to manual ones (compliance
        // requirement: every decision is recorded).
        if let Some(hit) =
            self.contracts
                .scopes
                .lookup(&self.session_id, &event.tool_name, &self.turn_id)
        {
            // FIX-01: `ApprovalDecision` is non-Copy because of `Unknown(String)`;
            // clone for the wire payload so the original survives for the
            // runtime decision below.
            let auto = ApprovalAutoResolvedEvent {
                session_id: self.session_id.clone(),
                topic: self.session_id.topic().map(ToOwned::to_owned),
                approval_id: approval_id.clone(),
                turn_id: self.turn_id.clone(),
                tool_name: event.tool_name.clone(),
                scope: hit.scope_wire().to_owned(),
                scope_match: hit.scope_match.clone(),
                decision: hit.decision.clone(),
            };
            // Best-effort: if the notification fails to send (connection
            // closed) we still apply the recorded decision — the runtime
            // already trusts the policy. Per FIX-04, `approval/auto_resolved`
            // is durable: drops surface as `protocol/replay_lossy`.
            let _ = send_notification_durable(
                &self.ws,
                &self.ledger,
                UiNotification::ApprovalAutoResolved(auto),
            );

            // FIX-07: build + emit the canonical `approval/decided` record.
            // `decided_by` is empty because the decision is system-issued
            // (matches the spec's "system-decided" convention).
            let policy_id = format!("policy:{}:{}", hit.scope_wire(), hit.scope_match);
            let decided_event = ApprovalDecidedEvent {
                session_id: self.session_id.clone(),
                topic: self.session_id.topic().map(ToOwned::to_owned),
                approval_id: approval_id.clone(),
                turn_id: self.turn_id.clone(),
                decision: hit.decision.clone(),
                scope: Some(hit.scope_wire().to_owned()),
                decided_at: Utc::now(),
                decided_by: String::new(),
                auto_resolved: true,
                policy_id: Some(policy_id),
                client_note: None,
            };
            log_decision_tracing(&decided_event, Some(event.tool_name.as_str()));
            if let Some(sessions) = self.state.sessions.as_ref() {
                let data_dir = sessions.lock().await.data_dir();
                let audit = self.contracts.audit_log(&data_dir);
                if let Err(error) = audit.record(&decided_event, Some(event.tool_name.as_str())) {
                    tracing::warn!(
                        target: "octos.approvals.decision",
                        approval_id = %decided_event.approval_id.0,
                        error = %error,
                        "failed to append approval audit log entry (auto-resolved)"
                    );
                }
            }
            let _ = send_notification_durable(
                &self.ws,
                &self.ledger,
                UiNotification::ApprovalDecided(decided_event),
            );

            return match hit.decision {
                ApprovalDecision::Approve => ToolApprovalDecision::Approve,
                ApprovalDecision::Deny => ToolApprovalDecision::Deny,
                // FIX-01: forward-compat fallback. A recorded decision the
                // current server doesn't understand fails closed.
                ApprovalDecision::Unknown(_) => ToolApprovalDecision::Deny,
            };
        }

        let response_rx = self.contracts.approvals.request_runtime(event.clone());

        // #1449 drop-guard: arm a guard keyed to THIS pending approval the
        // instant it is registered. If our future is dropped before a clean
        // resolution (per-tool timeout, turn interrupt, panic, connection
        // close), the guard's `Drop` cancels the entry — guaranteeing the
        // kept-pending-on-`Closed` wait below can never hang the turn forever
        // on an approval no client will answer. Disarmed on every clean exit.
        let mut waiter_guard = PendingApprovalWaiterGuard::new(
            self.contracts.clone(),
            self.session_id.clone(),
            approval_id.clone(),
            self.turn_id.clone(),
        );

        // #peer-respond — a peer parking on an approval is surfaced as
        // `awaiting_input` and answered via `peer_respond` PURELY through the
        // process-global pending store: `request_runtime` above already
        // registered this `(approval_id, session)` entry, which peer_list /
        // peer_respond read authoritatively. Nothing is written to the peer's
        // filesystem here, so a peer can never park invisibly on an fs failure.

        // #peer-awaiting-wake — the peer is now GENUINELY parked (its pending
        // oneshot is registered by `request_runtime` above). WAKE its originator
        // (master) with an autonomous continuation so an IDLE master is notified
        // to answer it via peer_list → peer_respond, instead of only discovering
        // the block if it happens to be taking turns. This runs ONLY on the real
        // park path: the scope-policy AUTO-RESOLVE short-circuit returned far
        // above (before `request_runtime`), so an auto-approved request never
        // reaches here and never wakes the master. No-op for a non-peer session
        // or an unresolvable originator. The enqueue is a quick scheduler push —
        // it does NOT block; we await `response_rx` below exactly as before, and
        // the woken master resolves that oneshot from a different task.

        // Approvals are durable: if the WS drop strands the request, the
        // ledger still records it and the client can rehydrate.
        if let Err(err) = send_notification_durable(
            &self.ws,
            &self.ledger,
            UiNotification::ApprovalRequested(event),
        ) {
            match err {
                SendError::Closed | SendError::FatalClosed => {
                    // Keep the approval pending: a reconnecting client replays
                    // the ledger and can still answer. `waiter_guard` is the
                    // backstop that releases the wait if it never reconnects.
                    tracing::warn!(
                        target: "octos::ui_protocol::ws",
                        error = ?err,
                        "approval/requested direct delivery failed; waiting for reconnect/replay"
                    );
                }
                other => {
                    cancel_approval_after_request_send_failure(
                        self.contracts.as_ref(),
                        &self.ws,
                        &self.ledger,
                        &self.session_id,
                        &approval_id,
                        &self.turn_id,
                    );
                    // Explicit cancel already ran; disarm so `Drop` does not
                    // re-cancel (a no-op on a now-cancelled entry, but cleaner).
                    waiter_guard.disarm();
                    tracing::warn!(
                        target: "octos::ui_protocol::ws",
                        error = ?other,
                        "approval/requested notification not delivered; denying"
                    );
                    return ToolApprovalDecision::Deny;
                }
            }
        }

        // Await boundary: the turn stays paused until the client decides
        // (resolves the oneshot) or the entry is cancelled (sender dropped →
        // Err → Deny). If OUR future is dropped here, `waiter_guard` cancels
        // the entry so this can never block indefinitely.
        let decision = response_rx.await.unwrap_or(ApprovalDecision::Deny);
        // Clean resolution — disarm so the guard does not re-cancel.
        waiter_guard.disarm();
        match decision {
            ApprovalDecision::Approve => ToolApprovalDecision::Approve,
            ApprovalDecision::Deny => ToolApprovalDecision::Deny,
            // FIX-01 added Unknown(_) for forward-compat. Treat any
            // unrecognized decision as Deny — fail closed at the trust
            // boundary.
            ApprovalDecision::Unknown(_) => ToolApprovalDecision::Deny,
        }
    }
}

fn cancel_approval_after_request_send_failure(
    contracts: &UiProtocolContractStores,
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    approval_id: &ApprovalId,
    turn_id: &TurnId,
) {
    let Some(cancelled) = contracts.approvals.cancel_pending_approval(
        session_id,
        approval_id,
        turn_id,
        APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED,
    ) else {
        return;
    };

    let _ = send_notification_durable(
        ws,
        ledger,
        UiNotification::ApprovalCancelled(ApprovalCancelledEvent {
            session_id: session_id.clone(),
            topic: session_id.topic().map(ToOwned::to_owned),
            approval_id: cancelled.approval_id,
            turn_id: cancelled.turn_id,
            reason: APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED.to_owned(),
        }),
    );
}

/// RAII drop-guard around the approval requester's wait on `response_rx`
/// (#1449) — the approval-store analogue of [`PendingQuestionWaiterGuard`].
///
/// #1449 keeps an approval PENDING when the direct `approval/requested` send
/// fails with `Closed`/`FatalClosed` (betting on a reconnect + ledger replay to
/// redeliver it), instead of failing closed with an immediate deny. That is the
/// right call for a transient disconnect, but it removes the one thing that
/// previously guaranteed the waiting tool future could not block forever. If
/// the client never reconnects AND the turn is autonomous (so it outlives the
/// connection and the `cancel_pending_for_turn` drain never fires),
/// `response_rx.await` would otherwise park indefinitely on an approval no
/// client can answer.
///
/// This guard ties cleanup to the lifetime of the waiting future itself: if the
/// future is dropped before a clean resolution — a per-tool timeout firing, a
/// turn interrupt aborting the task, a panic unwinding, or the connection
/// closing — `Drop` CANCELS the matching pending approval. Cancellation drops
/// the entry's runtime sender, so `response_rx.await` resolves to `Err` and the
/// tool fails closed (`Deny`) rather than hanging. It also closes the same
/// cancel-on-interrupt race the question guard does (an approval inserted after
/// the turn-interrupt drain would otherwise leak).
///
/// DISARMED on a clean resolution (a decision arrived, or an explicit
/// send-failure cancel already ran). `cancel_pending_approval` only acts on a
/// still-`Pending` entry, so an armed drop after a clean resolution is harmless.
struct PendingApprovalWaiterGuard {
    contracts: Arc<UiProtocolContractStores>,
    session_id: SessionKey,
    approval_id: ApprovalId,
    turn_id: TurnId,
    armed: bool,
}

impl PendingApprovalWaiterGuard {
    fn new(
        contracts: Arc<UiProtocolContractStores>,
        session_id: SessionKey,
        approval_id: ApprovalId,
        turn_id: TurnId,
    ) -> Self {
        Self {
            contracts,
            session_id,
            approval_id,
            turn_id,
            armed: true,
        }
    }

    /// Mark the wait as resolved cleanly so the guard does not cancel on drop.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingApprovalWaiterGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Resolve the waiter by cancelling the still-pending entry (drops its
        // runtime sender → `response_rx` errs → Deny). Best-effort and without
        // a wire notification here: on a dropped future the connection is
        // typically gone, and the turn-interrupt path emits `approval/cancelled`
        // wherever a live client still exists.
        self.contracts.approvals.cancel_pending_approval(
            &self.session_id,
            &self.approval_id,
            &self.turn_id,
            APPROVAL_CANCELLED_REASON_WAITER_DROPPED,
        );
    }
}

/// UPCR-2026-023 bridge: implements the agent's [`UserQuestionRequester`]
/// trait by minting a `question_id`, parking the request in the
/// [`PendingQuestionStore`], emitting `user_question/requested`, and awaiting
/// the oneshot the `user_question/respond` handler resolves. Mirrors
/// [`UiProtocolApprovalRequester`] end-to-end.
///
/// The requester is gated on the connection negotiating `user_question.v1`:
/// when the feature is NOT negotiated this requester is never installed (the
/// task-local stays unset), so the `ask_user_question` tool degrades to its
/// structured-metadata fallback and the turn never hard-blocks.
struct SessionUserQuestionRequester {
    ws: WsConnection,
    ledger: Arc<UiProtocolLedger>,
    contracts: Arc<UiProtocolContractStores>,
    session_id: SessionKey,
    turn_id: TurnId,
}

/// RAII drop-guard around the requester's wait on `response_rx`
/// (UPCR-2026-023, fix #2). If the `ask_user_question` tool's future is
/// dropped while it is parked on the answer — a per-tool timeout firing, a
/// turn interrupt aborting the task, a panic unwinding, or the connection
/// closing — this guard's `Drop` CANCELS the matching pending-question store
/// entry (resolving the waiter as cancelled and removing the entry) instead
/// of leaking it forever.
///
/// This closes the cancel-on-interrupt race codex flagged: the turn-interrupt
/// drain (`cancel_pending_for_turn`) only cancels entries that are visible at
/// drain time, so a question inserted AFTER the drain (the narrow window
/// between drain and the agent task actually stopping) would otherwise leak.
/// The guard makes cancellation robust regardless of drain timing because it
/// is keyed to the lifetime of the waiting future itself, not to a one-shot
/// sweep.
///
/// The guard is DISARMED on a clean resolution (`Answered`/`Cancelled`/wire
/// send failure), where the store entry has already moved out of `Pending`
/// and re-cancelling would be a no-op anyway. `cancel_pending_question` only
/// acts on a still-`Pending` entry, so an armed drop after a clean resolution
/// is harmless — disarming just skips the redundant lock.
///
/// (Approvals lack an equivalent guard today — a latent approval gap; not
/// fixed here.)
struct PendingQuestionWaiterGuard {
    contracts: Arc<UiProtocolContractStores>,
    session_id: SessionKey,
    question_id: octos_core::ui_protocol::QuestionId,
    armed: bool,
}

impl PendingQuestionWaiterGuard {
    fn new(
        contracts: Arc<UiProtocolContractStores>,
        session_id: SessionKey,
        question_id: octos_core::ui_protocol::QuestionId,
    ) -> Self {
        Self {
            contracts,
            session_id,
            question_id,
            armed: true,
        }
    }

    /// Mark the wait as resolved cleanly so the guard does not cancel on drop.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingQuestionWaiterGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.contracts.user_questions.cancel_pending_question(
            &self.session_id,
            &self.question_id,
            USER_QUESTION_CANCELLED_REASON_WAITER_DROPPED,
        );
    }
}

#[async_trait::async_trait]
impl octos_agent::UserQuestionRequester for SessionUserQuestionRequester {
    async fn request_user_question(&self, request: UserQuestionRequest) -> UserQuestionOutcome {
        let question_id = octos_core::ui_protocol::QuestionId::new();
        let event = UserQuestionRequestedEvent {
            session_id: self.session_id.clone(),
            topic: self.session_id.topic().map(ToOwned::to_owned),
            question_id: question_id.clone(),
            turn_id: self.turn_id.clone(),
            title: request.title,
            body: request.body,
            questions: request.questions,
        };

        let response_rx = self.contracts.user_questions.request_runtime(event.clone());

        // #2 — RAII drop-guard. Arm a guard keyed to THIS pending entry the
        // instant it is registered. If our future is dropped before a clean
        // resolution (per-tool timeout firing, turn interrupt aborting the
        // task, panic, connection close) the guard's `Drop` cancels the entry,
        // closing the cancel-on-interrupt race (an entry inserted after the
        // turn-interrupt drain would otherwise leak). We disarm it on every
        // clean exit path below.
        let mut waiter_guard = PendingQuestionWaiterGuard::new(
            self.contracts.clone(),
            self.session_id.clone(),
            question_id.clone(),
        );

        // #peer-respond — a peer parking on a question is surfaced as
        // `awaiting_input` and answered via `peer_respond` PURELY through the
        // process-global pending store: `request_runtime` above registered this
        // `(question_id, session)` entry (with its full per-question options),
        // which peer_list / peer_respond read authoritatively. No peer-filesystem
        // write happens here, so an OPEN peer can never park invisibly — and a
        // CLOSED one never gets here at all (#1842 park gate above).

        // #peer-awaiting-wake — the peer is now GENUINELY parked (its pending
        // oneshot is registered by `request_runtime` above). WAKE its originator
        // (master) so an IDLE master is notified to answer via peer_list →
        // peer_respond. Unlike the approval requester there is no auto-resolve
        // short-circuit for questions — every question genuinely parks — so the
        // wake fires for each. No-op for a non-peer session or an unresolvable
        // originator. The enqueue does NOT block; we await `response_rx` below
        // exactly as before, and the woken master resolves it from a different
        // task.

        // The event is durable: if the WS drop strands the request, the ledger
        // still records it and a reconnecting client can rehydrate. We cancel
        // the pending entry and degrade to the fallback so the turn continues
        // rather than hanging on a dead channel.
        if let Err(err) = send_notification_durable(
            &self.ws,
            &self.ledger,
            UiNotification::UserQuestionRequested(event),
        ) {
            // Explicit send-failure cancellation records the precise reason;
            // disarm the guard so its `Drop` does not also fire (a no-op on a
            // now-cancelled entry, but cleaner to skip).
            self.contracts.user_questions.cancel_pending_question(
                &self.session_id,
                &question_id,
                APPROVAL_CANCELLED_REASON_REQUEST_SEND_FAILED,
            );
            waiter_guard.disarm();
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                error = ?err,
                "user_question/requested notification not delivered; degrading to fallback"
            );
            return UserQuestionOutcome::Unsupported;
        }

        // This is the await boundary: the turn stays paused until the client
        // answers via user_question/respond (resolves the oneshot) or the turn
        // is interrupted (cancel_pending_for_turn drops the sender → Err). If
        // OUR future is dropped here, `waiter_guard` cancels the entry.
        let outcome = match response_rx.await {
            Ok(answers) => UserQuestionOutcome::Answered(answers),
            Err(_) => UserQuestionOutcome::Cancelled,
        };
        // Clean resolution — the store entry has already left `Pending`
        // (Answered) or its sender was dropped by the turn-interrupt drain
        // (Cancelled). Disarm so the guard does not re-cancel.
        waiter_guard.disarm();
        outcome
    }
}

fn approval_event_from_tool_request(
    request: ToolApprovalRequest,
    session_id: SessionKey,
    approval_id: ApprovalId,
    turn_id: TurnId,
    features: ConnectionUiFeatures,
) -> ApprovalRequestedEvent {
    let mut event = ApprovalRequestedEvent::generic(
        session_id,
        approval_id,
        turn_id,
        request.tool_name,
        request.title,
        request.body,
    );

    if features.typed_approvals {
        // Risk is derived from the tool manifest, not from the tool's own
        // payload — a malicious tool cannot self-attest as `low`. Default
        // `unspecified` makes "manifest didn't say" visible in the UI badge
        // instead of silently advertising `medium`. This applies to every
        // tool surface (shell, plugin, future MCP) — audit #715: previously
        // gated on `tool_name == "shell"`, leaving plugin approvals with no
        // risk classification on the wire even though manifest-driven gating
        // engaged server-side (PR #712).
        event.risk = Some(server_risk_for(&event.tool_name));

        if event.tool_name == "shell" {
            let command = request.command;
            if command.is_some() || request.cwd.is_some() {
                event.approval_kind = Some(approval_kinds::COMMAND.to_owned());
                // `cwd` is path-shaped: sanitise before it lands in display
                // strings (typed_details, render hints).
                let safe_cwd = request.cwd.as_deref().map(sanitize_display_path);
                event.typed_details = Some(ApprovalTypedDetails::command(
                    ApprovalCommandDetails {
                        argv: Vec::new(),
                        command_line: command,
                        cwd: safe_cwd,
                        env_keys: Vec::new(),
                        tool_call_id: Some(request.tool_id),
                    },
                    None,
                ));
                event.render_hints = Some(ApprovalRenderHints {
                    default_decision: Some("deny".to_owned()),
                    primary_label: Some("Approve".to_owned()),
                    secondary_label: Some("Deny".to_owned()),
                    danger: Some(false),
                    monospace_fields: vec![
                        "typed_details.command.command_line".to_owned(),
                        "typed_details.command.cwd".to_owned(),
                    ],
                });
            }
        }
    }

    event
}

/// Resolve the manifest-declared risk for `tool_name`. Falls back to
/// `unspecified` when the registry has no entry.
fn server_risk_for(tool_name: &str) -> String {
    octos_core::ui_protocol::tool_approval_risk(tool_name)
}

#[cfg(test)]
fn register_tool_risk_for_test(tool_name: &str, risk: &str) {
    octos_core::ui_protocol::register_tool_approval_risk(tool_name, risk);
}

#[cfg(test)]
fn clear_tool_risk_registry_for_test() {
    octos_core::ui_protocol::clear_tool_approval_risks_for_test();
}

#[cfg(test)]
fn tool_risk_registry_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Cap on how long stdio shutdown waits for in-flight turns to finalize.
const STDIO_SHUTDOWN_TURN_DRAIN_MAX: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait (bounded by `max`) until none of THIS connection's turns is still
/// live in the process-global `active_turns` registry.
///
/// Stdin EOF ends the stdio loop, and returning from `stdio_connection`
/// exits the process — dropping the runtime and CANCELLING any in-flight
/// turn task mid-finalization (assistant persistence, usage records, the
/// terminal `turn/completed` ledger append). The client is already gone so
/// nothing new can start; draining briefly turns "the answer and the turn's
/// terminal state were lost" into "the next hydrate shows the completed
/// turn". Scoped to `connection_turns` so turns owned by other connections
/// never block this exit, and terminal-but-not-yet-cleaned entries do not
/// hold shutdown open. Returns `false` on deadline.
async fn drain_connection_turns_for_shutdown(
    active_turns: &SharedActiveTurns,
    connection_turns: &SharedConnectionTurns,
    max: std::time::Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        let owned = connection_turns.lock().await.clone();
        let mut live: Vec<SessionKey> = Vec::new();
        {
            let active = active_turns.lock().await;
            for (session_id, registered) in &owned {
                let Some(turn) = active.get(session_id) else {
                    continue;
                };
                if !registered.matches(turn) {
                    continue;
                }
                // Terminal entries stay registered until cleanup — the turn
                // already finalized, so it must not hold shutdown open.
                let state = turn.state.lock().await;
                if !matches!(*state, TurnState::Terminal(_)) {
                    live.push(session_id.clone());
                }
            }
        }
        if live.is_empty() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                target: "octos::ui_protocol::stdio",
                sessions = ?live,
                "stdio shutdown: in-flight turns did not finalize before the drain deadline"
            );
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

pub(crate) async fn stdio_connection(state: Arc<AppState>) -> eyre::Result<()> {
    stdio_connection_with_io(state, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Lifecycle owned by a local frontend, not a second execution policy.
/// Admission starts reserved for its foreground input; the adapter explicitly
/// enables background dispatch while it is listening for that work.
#[derive(Clone, Default)]
pub(crate) struct EmbeddedStdioControl {
    pub shutdown: tokio_util::sync::CancellationToken,
    pub continuations_enabled: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) async fn embedded_stdio_connection_with_io<R, W>(
    state: Arc<AppState>,
    stdin_reader: R,
    stdout_writer: W,
    control: EmbeddedStdioControl,
) -> eyre::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    stdio_connection_with_io_policy(state, stdin_reader, stdout_writer, Some(control)).await
}

pub(crate) async fn stdio_connection_with_io<R, W>(
    state: Arc<AppState>,
    stdin_reader: R,
    stdout_writer: W,
) -> eyre::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    stdio_connection_with_io_policy(state, stdin_reader, stdout_writer, None).await
}

async fn stdio_connection_with_io_policy<R, W>(
    state: Arc<AppState>,
    stdin_reader: R,
    stdout_writer: W,
    embedded: Option<EmbeddedStdioControl>,
) -> eyre::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (writer_tx, writer_rx) =
        std::sync::mpsc::sync_channel::<WsMessage>(WS_WRITER_CHANNEL_CAPACITY);
    let ws = WsConnection::new_stdio(writer_tx);
    let (writer_done_tx, mut writer_done_rx) = oneshot::channel();
    let writer_failure_signal = ws.failure_signal();
    let writer_handle = std::thread::Builder::new()
        .name("octos-appui-stdio-writer".into())
        .spawn(move || {
            let result = stdio_writer_loop_sync_to(writer_rx, stdout_writer, writer_failure_signal);
            let _ = writer_done_tx.send(result);
        })
        .map_err(|error| eyre::eyre!("failed to spawn AppUI stdio writer: {error}"))?;
    let active_turns = active_turns_registry();
    let connection_turns: SharedConnectionTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let live_forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let contracts = contract_stores();
    let ledger = event_ledger(&state).await;
    let _ = diff_preview_store(&state, contracts.as_ref()).await;
    let mut features = ConnectionUiFeatures::stdio_defaults();
    // A stdio client may send client_hello as its first request. Defer the
    // one connection sample until that first request so a v2 hello is counted
    // as v2 rather than as the temporary legacy default. A non-hello first
    // request resolves to the stdio defaults and is recorded as legacy.
    let mut connection_mode_recorded = false;
    // See the ws loop's twin: detached asides must not outlive this stdio
    // connection (they hold the writer sender → EOF shutdown would block).
    let mut btw_aside_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut connection_profile_id_owned: Option<String> = None;
    let mut appui_continuation_tick = tokio::time::interval(Duration::from_secs(2));
    appui_continuation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let failed_notify = ws.failed_notify();
    let mut writer_finished = false;
    let mut writer_exit_error: Option<eyre::Report> = None;

    let mut stdin = StdioNdjsonReader::new(stdin_reader);
    let dispatch = async {
        loop {
            if ws.is_failed() {
                break;
            }
            let notified = failed_notify.notified();
            tokio::pin!(notified);
            if ws.is_failed() {
                break;
            }
            let text = tokio::select! {
                biased;
                _ = &mut notified => {
                    break;
                }
                writer_result = &mut writer_done_rx => {
                    ws.mark_failed();
                    writer_finished = true;
                    writer_exit_error = Some(match writer_result {
                        Ok(Ok(())) => eyre::eyre!("AppUI stdio writer stopped before stdin closed"),
                        Ok(Err(error)) => eyre::eyre!("AppUI stdio writer failed: {error}"),
                        Err(error) => eyre::eyre!("AppUI stdio writer thread failed: {error}"),
                    });
                    break;
                }
                frame = stdin.next_frame() => {
                    match frame? {
                        StdioFrameRead::Frame(text) => text,
                        StdioFrameRead::Eof => break,
                        StdioFrameRead::TooLarge => {
                            let _ = send_rpc_error(&ws, None, app_ui_codec::frame_too_large_error());
                            continue;
                        }
                    }
                }
            };
            if ws.is_failed() {
                break;
            }
            let request = match parse_ws_text_frame(text.as_str()) {
                Ok(ParsedFrame::Request(request)) => request,
                Ok(ParsedFrame::Notification(method)) => {
                    if !is_known_inbound_notification(&method) {
                        tracing::debug!(
                            target: "octos::ui_protocol::stdio",
                            method = %method,
                            "ignoring unknown inbound notification"
                        );
                    }
                    continue;
                }
                Err(error) => {
                    let _ = send_rpc_error(&ws, None, error);
                    continue;
                }
            };
            append_appui_transcript_frame(
                "client_to_server",
                serde_json::to_value(&request).unwrap_or_else(|_| json!({ "malformed": true })),
            );
            #[cfg(test)]
            record_stdio_dispatch_for_test();
            let id = request.id.clone();
            // stdio transport is never a session-ingress socket.
            if handle_client_hello_rpc(&ws, &state, id.clone(), &request, &mut features) {
                if !connection_mode_recorded {
                    record_ui_protocol_connection_mode(features, "stdio");
                    connection_mode_recorded = true;
                }
                continue;
            }
            if !connection_mode_recorded {
                record_ui_protocol_connection_mode(features, "stdio");
                connection_mode_recorded = true;
            }
            let connection_profile_id = connection_profile_id_owned.as_deref();

            if handle_raw_appui_rpc(
                &ws,
                &state,
                &ledger,
                &contracts,
                &active_turns,
                &connection_turns,
                features,
                connection_profile_id,
                id.clone(),
                &request,
            )
            .await
            {
                continue;
            }

            let command = match route_rpc_command(request, features) {
                Ok(command) => command,
                Err(error) => {
                    let _ = send_rpc_error(&ws, Some(id), error);
                    continue;
                }
            };

            match command {
                UiCommand::ProfileLocalCreate(params) => {
                    match create_or_get_local_solo_profile(&state, params) {
                        Ok(result) => {
                            let _ = send_ui_rpc_result(
                                &ws,
                                id,
                                UiRpcResult::ProfileLocalCreate(result),
                            );
                        }
                        Err(error) => {
                            let _ = send_rpc_error(&ws, Some(id), error);
                        }
                    }
                }
                UiCommand::LaunchResolve(params) => {
                    handle_launch_resolve(
                        &ws,
                        &state,
                        connection_profile_id_owned.as_deref(),
                        features,
                        id,
                        params,
                    )
                    .await;
                }
                UiCommand::SessionOpen(params) => {
                    let next_connection_profile_id = stdio_session_open_candidate_profile(
                        &params,
                        connection_profile_id_owned.as_deref(),
                    );
                    let opened = handle_session_open(
                        &ws,
                        &state,
                        &ledger,
                        &contracts.approvals,
                        &contracts.user_questions,
                        &live_forwarders,
                        next_connection_profile_id.as_deref(),
                        // NOT pinned: stdio rebinds `connection_profile_id_owned`
                        // after every successful open, so a later open under
                        // another profile retargets this session's turns exactly
                        // as `session_open_profile_id` does on the WS path.
                        None,
                        features,
                        id,
                        params,
                    )
                    .await;
                    if opened {
                        connection_profile_id_owned = next_connection_profile_id;
                    }
                }
                UiCommand::TurnStart(params) => {
                    let turn_profile_id = params
                        .session_id
                        .profile_id()
                        .map(ToOwned::to_owned)
                        .or_else(|| connection_profile_id_owned.clone());
                    handle_turn_start(
                        &ws,
                        &state,
                        &ledger,
                        &contracts,
                        &active_turns,
                        &connection_turns,
                        turn_profile_id.as_deref(),
                        None,
                        features,
                        id,
                        params,
                    )
                    .await;
                }
                UiCommand::TurnInterrupt(params) => {
                    handle_turn_interrupt(&ws, &ledger, &active_turns, &contracts, id, params)
                        .await;
                }
                UiCommand::ApprovalRespond(params) => {
                    handle_approval_respond(
                        &ws,
                        &state,
                        &ledger,
                        &contracts,
                        connection_profile_id_owned.as_deref(),
                        id,
                        params,
                    )
                    .await;
                }
                UiCommand::ApprovalScopesList(params) => {
                    handle_approval_scopes_list(
                        &ws,
                        &contracts.scopes,
                        connection_profile_id_owned.as_deref(),
                        id,
                        params,
                    )
                    .await;
                }
                UiCommand::UserQuestionRespond(params) => {
                    handle_user_question_respond(
                        &ws,
                        &contracts,
                        connection_profile_id_owned.as_deref(),
                        id,
                        params,
                    )
                    .await;
                }
                UiCommand::SessionHydrate(params) => {
                    handle_session_hydrate(
                        &ws,
                        &state,
                        &ledger,
                        &contracts.approvals,
                        &contracts.user_questions,
                        &active_turns,
                        connection_profile_id_owned.as_deref(),
                        None,
                        features,
                        id,
                        params,
                    )
                    .await;
                }
                UiCommand::SessionRollback(params) => {
                    handle_session_rollback(
                        &ws,
                        &state,
                        &ledger,
                        &active_turns,
                        connection_profile_id_owned.as_deref(),
                        None,
                        id,
                        params,
                    )
                    .await;
                }
                UiCommand::SessionFork(params) => {
                    handle_session_fork(
                        &ws,
                        &state,
                        connection_profile_id_owned.as_deref(),
                        None,
                        id,
                        params,
                    )
                    .await;
                }
                UiCommand::ThreadGraphGet(params) => {
                    handle_thread_graph_get(
                        &ws,
                        &state,
                        &ledger,
                        &active_turns,
                        connection_profile_id_owned.as_deref(),
                        None,
                        id,
                        params,
                    )
                    .await;
                }
                UiCommand::TurnStateGet(params) => {
                    handle_turn_state_get(
                        &ws,
                        &state,
                        &ledger,
                        &active_turns,
                        connection_profile_id_owned.as_deref(),
                        None,
                        features,
                        id,
                        params,
                    )
                    .await;
                }
                UiCommand::SessionBtw(params) => {
                    let aside = handle_session_btw(
                        &ws,
                        &state,
                        &ledger,
                        &active_turns,
                        connection_profile_id_owned.as_deref(),
                        None,
                        id,
                        params,
                    )
                    .await;
                    if let Some(task) = aside {
                        btw_aside_tasks.retain(|task| !task.is_finished());
                        btw_aside_tasks.push(task);
                    }
                }
                UiCommand::PermissionProfileList(params) => {
                    let result = permission_profile_list_result(&state, params);
                    let _ = send_ui_rpc_result(&ws, id, UiRpcResult::PermissionProfileList(result));
                }
                UiCommand::PermissionProfileSet(params) => {
                    let session_id = params.session_id.clone();
                    match permission_profile_set_result(&state, params) {
                        Ok(result) => {
                            // Session-scoped eviction across all profiles + the
                            // in-flight generation guard — see the sibling
                            // dispatcher above (codex P1 ×2 on #1639).
                            state.session_cache.invalidate_session(&session_id).await;
                            let _ = send_ui_rpc_result(
                                &ws,
                                id,
                                UiRpcResult::PermissionProfileSet(result),
                            );
                        }
                        Err(error) => {
                            let _ = send_rpc_error(&ws, Some(id), error);
                        }
                    }
                }
            }
        }
        Ok::<(), eyre::Report>(())
    };
    // Cancel dispatch cooperatively, including a pending RPC handler, then run
    // the SAME connection-owned cleanup. Never abort the cleanup future itself.
    let dispatch_result = if let Some(control) = embedded.as_ref() {
        tokio::select! {
            biased;
            _ = control.shutdown.cancelled() => Ok(()),
            result = dispatch => result,
        }
    } else {
        dispatch.await
    };

    // Let in-flight turns finalize (persist + ledger terminal) before the
    // process exit cancels their tasks — see drain_connection_turns_for_shutdown.
    // An explicit embedded close instead cancels its own pending work below.
    if embedded.is_none() {
        drain_connection_turns_for_shutdown(
            &active_turns,
            &connection_turns,
            STDIO_SHUTDOWN_TURN_DRAIN_MAX,
        )
        .await;
    }
    abort_btw_aside_tasks(&mut btw_aside_tasks).await;
    cleanup_stdio_connection_resources(
        &active_turns,
        &connection_turns,
        &live_forwarders,
        contracts.as_ref(),
        ledger.as_ref(),
    )
    .await;
    // All accepted output is ahead of this FIFO Close. Do not wait for every
    // background sender clone to disappear before letting the writer exit.
    ws.mark_failed();
    if !writer_finished && let Some(writer) = ws.stdio_writer.as_ref() {
        let _ = writer.send(WsMessage::Close(None));
    }
    drop(ws);
    if writer_finished {
        if writer_handle.join().is_err() {
            return Err(eyre::eyre!("AppUI stdio writer thread panicked"));
        }
        if let Some(error) = writer_exit_error {
            return Err(error);
        }
        return dispatch_result;
    }
    let writer_result = writer_done_rx.await;
    if writer_handle.join().is_err() {
        return Err(eyre::eyre!("AppUI stdio writer thread panicked"));
    }
    match writer_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(eyre::eyre!("AppUI stdio writer failed: {error}")),
        Err(error) => return Err(eyre::eyre!("AppUI stdio writer thread failed: {error}")),
    }
    dispatch_result
}

#[cfg(test)]
static STDIO_DISPATCH_COUNT_FOR_TEST: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn reset_stdio_dispatch_count_for_test() {
    STDIO_DISPATCH_COUNT_FOR_TEST.store(0, Ordering::SeqCst);
}

#[cfg(test)]
fn stdio_dispatch_count_for_test() -> usize {
    STDIO_DISPATCH_COUNT_FOR_TEST.load(Ordering::SeqCst)
}

#[cfg(test)]
fn record_stdio_dispatch_for_test() {
    STDIO_DISPATCH_COUNT_FOR_TEST.fetch_add(1, Ordering::SeqCst);
}

enum StdioFrameRead {
    Frame(String),
    Eof,
    TooLarge,
}

struct StdioNdjsonReader<R> {
    reader: R,
    buffer: Vec<u8>,
}

impl<R> StdioNdjsonReader<R>
where
    R: AsyncRead + Unpin,
{
    fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::with_capacity(8 * 1024),
        }
    }

    async fn next_frame(&mut self) -> eyre::Result<StdioFrameRead> {
        loop {
            if let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
                if line.len() > MAX_TEXT_FRAME_BYTES + 1 {
                    return Ok(StdioFrameRead::TooLarge);
                }
                app_ui_codec::strip_ndjson_line_ending_bytes(&mut line);
                if line.len() > MAX_TEXT_FRAME_BYTES {
                    return Ok(StdioFrameRead::TooLarge);
                }
                let text = String::from_utf8(line)
                    .map_err(|err| eyre::eyre!("AppUI stdio frame is not UTF-8: {err}"))?;
                return Ok(StdioFrameRead::Frame(text));
            }

            if self.buffer.len() == MAX_TEXT_FRAME_BYTES {
                let mut byte = [0_u8; 1];
                let read = self.reader.read(&mut byte).await?;
                if read == 0 {
                    let line = std::mem::take(&mut self.buffer);
                    let text = String::from_utf8(line)
                        .map_err(|err| eyre::eyre!("AppUI stdio frame is not UTF-8: {err}"))?;
                    return Ok(StdioFrameRead::Frame(text));
                }
                if byte[0] == b'\n' {
                    let mut line = std::mem::take(&mut self.buffer);
                    app_ui_codec::strip_ndjson_line_ending_bytes(&mut line);
                    let text = String::from_utf8(line)
                        .map_err(|err| eyre::eyre!("AppUI stdio frame is not UTF-8: {err}"))?;
                    return Ok(StdioFrameRead::Frame(text));
                }
                self.buffer.clear();
                self.drain_until_line_boundary().await?;
                return Ok(StdioFrameRead::TooLarge);
            }

            let mut chunk = [0_u8; 8192];
            let remaining_before_limit = MAX_TEXT_FRAME_BYTES.saturating_sub(self.buffer.len());
            let read_len = remaining_before_limit.min(chunk.len()).max(1);
            let read = self.reader.read(&mut chunk[..read_len]).await?;
            if read == 0 {
                if self.buffer.is_empty() {
                    return Ok(StdioFrameRead::Eof);
                }
                let line = std::mem::take(&mut self.buffer);
                if line.len() > MAX_TEXT_FRAME_BYTES {
                    return Ok(StdioFrameRead::TooLarge);
                }
                let text = String::from_utf8(line)
                    .map_err(|err| eyre::eyre!("AppUI stdio frame is not UTF-8: {err}"))?;
                return Ok(StdioFrameRead::Frame(text));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    async fn drain_until_line_boundary(&mut self) -> eyre::Result<()> {
        let mut chunk = [0_u8; 8192];
        loop {
            let read = self.reader.read(&mut chunk).await?;
            if read == 0 || chunk[..read].contains(&b'\n') {
                return Ok(());
            }
        }
    }
}

fn stdio_writer_loop_sync_to<W>(
    rx: std::sync::mpsc::Receiver<WsMessage>,
    writer: W,
    failure_signal: ConnectionFailureSignal,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| {
            std::io::Error::other(format!("build AppUI stdio writer runtime: {error}"))
        })?;
    runtime.block_on(async move {
        let mut stdout = BufWriter::new(writer);
        while let Ok(message) = rx.recv() {
            if !write_stdio_message(&mut stdout, message, &failure_signal).await? {
                break;
            }
        }
        Ok(())
    })
}

async fn write_stdio_message<W>(
    stdout: &mut BufWriter<W>,
    message: WsMessage,
    failure_signal: &ConnectionFailureSignal,
) -> std::io::Result<bool>
where
    W: AsyncWrite + Unpin,
{
    match message {
        WsMessage::Text(text) => {
            if let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) {
                append_appui_transcript_frame("server_to_client", frame);
            }
            let write_result = async {
                stdout.write_all(text.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await
            }
            .await;
            if let Err(error) = write_result {
                failure_signal.mark_failed();
                return Err(error);
            }
            Ok(true)
        }
        WsMessage::Close(_) => Ok(false),
    }
}

/// Abort every detached `session/btw` aside this connection spawned and await
/// each JoinHandle (abort → future dropped → the in-flight guard's Drop frees
/// the busy slot, and the task's WsConnection clone releases the writer).
async fn abort_btw_aside_tasks(tasks: &mut Vec<tokio::task::JoinHandle<()>>) {
    for task in tasks.drain(..) {
        task.abort();
        let _ = task.await;
    }
}

async fn cleanup_stdio_connection_resources(
    active_turns: &SharedActiveTurns,
    connection_turns: &SharedConnectionTurns,
    live_forwarders: &SharedLiveForwarders,
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
) {
    abort_connection_turns(
        active_turns,
        connection_turns,
        &contracts.scopes,
        ledger,
        &contracts.approvals,
        &contracts.user_questions,
    )
    .await;
    abort_live_forwarders(live_forwarders, ledger).await;
}

async fn abort_live_forwarders(forwarders: &SharedLiveForwarders, ledger: &UiProtocolLedger) {
    let drained: Vec<(SessionKey, tokio::task::JoinHandle<()>)> = {
        let mut guard = forwarders.lock().await;
        guard.drain().collect()
    };
    if drained.is_empty() {
        return;
    }
    // #923.2 + #924 NIT 8: abort every forwarder, then `await` each
    // JoinHandle so the receiver-drop has provably happened before we
    // prune. The old `yield_now()` was a single scheduling hint and
    // could lose the race under load — leaving the ledger
    // broadcaster believing it still had a live subscriber. Awaiting
    // the JoinHandle is the canonical "task is fully done" signal in
    // tokio. We ignore the JoinError for aborted tasks (that's the
    // expected shape).
    let mut drained_sessions: Vec<SessionKey> = Vec::with_capacity(drained.len());
    for (session_id, handle) in drained {
        handle.abort();
        let _ = handle.await;
        drained_sessions.push(session_id);
    }
    for session_id in drained_sessions {
        ledger.prune_subscriber_if_idle(&session_id);
    }
}

/// #922.1: JSON-RPC envelopes with no `id` are notifications, not
/// requests. The protocol's bridge sends a `ping` notification every
/// 30s; the legacy parser required `RpcRequest.id: String`, so the
/// server replied with a `parse_error` for every keepalive. The
/// resulting noise also masked real parse errors.
///
/// #924 NIT 6: distinguish notifications from requests by KEY
/// PRESENCE on `id`, not by null-check. A JSON-RPC envelope with
/// `id: null` is malformed (per spec §4 the `id` of a request must
/// be a String / Number / NULL only for the response correlation
/// reserved use); routing it as a "notification" silently swallowed
/// what should be a loud parse error. The rule:
///
/// - `id` absent       → Notification (today's `ping`/etc.)
/// - `id` is String    → Request (our server's expected shape)
/// - `id` is Number    → Reject with parse_error (we require String)
/// - `id` is null/etc. → Reject with parse_error
///
/// Unparseable frames continue to return `Err(RpcError)` so the
/// existing "lifecycle: client violated wire contract" branch fires.
#[derive(Debug)]
enum ParsedFrame {
    Request(RpcRequest<Value>),
    Notification(String),
}

fn parse_ws_text_frame(text: &str) -> Result<ParsedFrame, RpcError> {
    match app_ui_codec::parse_text_frame(text)? {
        AppUiFrame::Request(request) => Ok(ParsedFrame::Request(request)),
        AppUiFrame::Notification(notification) => {
            Ok(ParsedFrame::Notification(notification.method))
        }
        AppUiFrame::Response(_) | AppUiFrame::Error(_) => Err(RpcError::parse_error(
            "client frame must be a JSON-RPC request or notification",
        )),
    }
}

#[cfg(test)]
fn parse_rpc_request(text: &str) -> Result<RpcRequest<Value>, RpcError> {
    match app_ui_codec::parse_text_frame(text)? {
        AppUiFrame::Request(request) => Ok(request),
        AppUiFrame::Notification(_) | AppUiFrame::Response(_) | AppUiFrame::Error(_) => {
            Err(RpcError::parse_error("expected JSON-RPC request"))
        }
    }
}

/// Inbound notifications the server accepts (no reply emitted).
fn is_known_inbound_notification(method: &str) -> bool {
    matches!(method, "ping")
}

#[derive(Debug, Default, Deserialize)]
struct RawClientHelloParams {
    #[serde(default)]
    transport: Option<String>,
    #[serde(default, alias = "features", alias = "requested_features")]
    supported_features: Vec<String>,
    #[serde(default)]
    client: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProfileParams {
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    session_id: Option<SessionKey>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProfileSkillsListParams {
    #[serde(default)]
    profile_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProfileSkillsRegistrySearchParams {
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default, alias = "query")]
    q: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawProfileSkillsInstallParams {
    #[serde(default)]
    profile_id: Option<String>,
    repo: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize)]
struct RawProfileSkillsRemoveParams {
    #[serde(default)]
    profile_id: Option<String>,
    name: String,
}

/// One provider route on the AppUI `profile/llm/*` wire (#2166 typed
/// schema). Unknown keys are rejected by [`reject_unknown_llm_upsert_fields`]
/// before serde sees them (so the error lists EVERY rejected field), and
/// `deny_unknown_fields` here is the belt-and-braces second layer.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmRoute {
    #[serde(default)]
    route_id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    api_type: Option<String>,
}

/// The typed main-model selection + inference-parameter schema shared by
/// `profile/llm/upsert` and `profile/llm/test`
/// (#2166). Test and Save parse the identical shape, so a payload that
/// probes successfully is byte-for-byte the payload that persists.
///
/// Absent/null semantics for every optional inference field: `absent ≡
/// null ≡ inherit` (clear any prior override — the upsert payload is the
/// COMPLETE inference configuration for the addressed selection); an
/// explicit value is an override. Omitted fields always mean "defer to the
/// next tier of the precedence chain", never "silently keep serving a value
/// the caller cannot see in the list response".
///
/// Ownership (what this schema deliberately does NOT accept):
/// - `max_output_tokens` → owned by the profile gateway contract
///   (`[gateway] max_output_tokens`); rejected with
///   `kind: "llm_param_owned_elsewhere"`.
/// - Per-session reasoning overrides → owned by the durable
///   session/turn contract (`ui_protocol_reasoning_effort.rs`); this schema
///   only sets the per-MODEL default tier.
/// - Arbitrary provider request-body keys → owned by the gateway
///   `llm_sampling_params` passthrough (#2176); rejected here as unknown.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmSelection {
    #[serde(default)]
    family_id: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    route: RawLlmRoute,
    /// Typed compatibility metadata (mirrors
    /// [`octos_llm::openai::ModelHints`]) — never a request-body bag.
    #[serde(default)]
    model_hints: Option<octos_llm::openai::ModelHints>,
    /// Local runtime context budget override (#2142). Reaches the runtime
    /// `ContextWindowOverride` via the durable selection; NOT an upstream
    /// request field.
    #[serde(default)]
    context_window: Option<u32>,
    /// Per-model default sampling temperature (finite, 0.0..=2.0).
    #[serde(default)]
    temperature: Option<f64>,
    /// Per-model default nucleus-sampling ceiling (finite, 0.0..=1.0).
    #[serde(default)]
    top_p: Option<f64>,
    /// Per-model default reasoning effort (the session/turn override tier
    /// wins over this).
    #[serde(default)]
    reasoning_effort: Option<octos_llm::ReasoningEffort>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfileLlmSelectParams {
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    session_id: Option<SessionKey>,
    #[serde(default)]
    family_id: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    route_id: Option<String>,
}

/// Typed AppUI `profile/llm/upsert` / `test` params
/// (#2166). Unknown keys are rejected with their full dotted paths —
/// `never return applied:true after discarding requested settings`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfileLlmUpsertParams {
    #[serde(default)]
    profile_id: Option<String>,
    selection: RawLlmSelection,
    #[serde(default)]
    api_key: Option<Value>,
    #[serde(default)]
    set_primary: bool,
}

/// Known keys per container level of the AppUI LLM schema (#2166). The
/// pre-pass in [`reject_unknown_llm_upsert_fields`] walks these so ONE
/// error can name every rejected field instead of only the first.
const LLM_UPSERT_KNOWN_TOP: &[&str] = &["profile_id", "selection", "api_key", "set_primary"];
const LLM_SELECTION_KNOWN: &[&str] = &[
    "family_id",
    "model_id",
    "route",
    "model_hints",
    "context_window",
    "temperature",
    "top_p",
    "reasoning_effort",
];
const LLM_ROUTE_KNOWN: &[&str] = &["route_id", "label", "base_url", "api_key_env", "api_type"];
const LLM_MODEL_HINTS_KNOWN: &[&str] = &[
    "uses_completion_tokens",
    "fixed_temperature",
    "lacks_vision",
    "merge_system_messages",
    "reasoning_style",
];
/// Fields a client may plausibly send on this RPC that are REAL but owned
/// by a different contract tier. They get a dedicated typed error pointing
/// at the owner instead of a generic "unknown field".
const LLM_FOREIGN_FIELDS: &[(&str, &str)] = &[
    (
        "selection.max_output_tokens",
        "profile gateway `max_output_tokens` ([gateway] max_output_tokens in the \
         profile config) — a per-model output cap is not part of the AppUI model \
         schema",
    ),
    (
        "max_output_tokens",
        "profile gateway `max_output_tokens` ([gateway] max_output_tokens in the \
         profile config)",
    ),
];

/// Pre-pass for the AppUI LLM mutation RPCs (#2166): collect EVERY unknown
/// field (with its dotted path) and every foreign-owned field BEFORE any
/// store mutation, so the caller gets one typed `invalid_params` naming all
/// of them and the prior configuration is untouched. serde's
/// `deny_unknown_fields` (on the structs above) remains as the second
/// layer for shapes this walk does not model.
fn reject_unknown_llm_upsert_fields(params: &Value) -> Result<(), RpcError> {
    let Some(obj) = params.as_object() else {
        return Ok(()); // non-object falls through to serde's own type error
    };
    let selection_obj = obj.get("selection").and_then(Value::as_object);
    let contains_foreign = |path: &str| match path.split_once('.') {
        Some(("selection", key)) => {
            selection_obj.is_some_and(|selection| selection.contains_key(key))
        }
        _ => obj.contains_key(path),
    };
    let mut unknown: Vec<String> = obj
        .keys()
        .filter(|key| !LLM_UPSERT_KNOWN_TOP.contains(&key.as_str()))
        .filter(|key| {
            !LLM_FOREIGN_FIELDS
                .iter()
                .any(|(path, _)| *path == key.as_str())
        })
        .map(String::clone)
        .collect();

    if let Some(selection) = selection_obj {
        for key in selection.keys() {
            let path = format!("selection.{key}");
            if LLM_FOREIGN_FIELDS.iter().any(|(known, _)| *known == path) {
                continue;
            }
            if !LLM_SELECTION_KNOWN.contains(&key.as_str()) {
                unknown.push(path);
            }
        }
        if let Some(route) = selection.get("route").and_then(Value::as_object) {
            unknown.extend(
                route
                    .keys()
                    .filter(|key| !LLM_ROUTE_KNOWN.contains(&key.as_str()))
                    .map(|key| format!("selection.route.{key}")),
            );
        }
        if let Some(hints) = selection.get("model_hints").and_then(Value::as_object) {
            unknown.extend(
                hints
                    .keys()
                    .filter(|key| !LLM_MODEL_HINTS_KNOWN.contains(&key.as_str()))
                    .map(|key| format!("selection.model_hints.{key}")),
            );
        }
    }

    let foreign: Vec<&str> = LLM_FOREIGN_FIELDS
        .iter()
        .filter(|(path, _)| contains_foreign(path))
        .map(|(path, _)| *path)
        .collect();

    if !foreign.is_empty() {
        let owners: Vec<Value> = foreign
            .iter()
            .map(|path| {
                let owner = LLM_FOREIGN_FIELDS
                    .iter()
                    .find(|(known, _)| known == path)
                    .map(|(_, owner)| *owner)
                    .unwrap_or("another configuration contract");
                json!({ "field": path, "owner": owner })
            })
            .collect();
        return Err(RpcError::invalid_params(format!(
            "field(s) {} belong to a different configuration contract and are not \
             accepted by profile/llm/upsert",
            foreign.join(", ")
        ))
        .with_data(json!({
            "kind": "llm_param_owned_elsewhere",
            "rejected_fields": foreign,
            "owners": owners,
        })));
    }

    if !unknown.is_empty() {
        unknown.sort();
        return Err(RpcError::invalid_params(format!(
            "unknown field(s): {} — profile/llm/upsert accepts a typed schema and \
             never silently discards fields",
            unknown.join(", ")
        ))
        .with_data(json!({
            "kind": "llm_unknown_fields",
            "rejected_fields": unknown,
        })));
    }
    Ok(())
}

/// Range/finite validation for the typed inference fields (#2166): runs
/// BEFORE any store mutation, so an invalid value leaves the prior
/// configuration untouched. (JSON cannot carry NaN/Inf, but the guard keeps
/// the invariant local to the schema instead of trusting the transport.)
fn validate_llm_inference_fields(selection: &RawLlmSelection) -> Result<(), RpcError> {
    fn check_f64_range(
        value: Option<f64>,
        field: &str,
        range: &str,
        min: f64,
        max: f64,
    ) -> Result<(), RpcError> {
        let Some(value) = value else {
            return Ok(());
        };
        if !value.is_finite() {
            return Err(RpcError::invalid_params(format!(
                "selection.{field} must be a finite number"
            ))
            .with_data(json!({
                "kind": "llm_param_non_finite",
                "field": format!("selection.{field}"),
            })));
        }
        if !(min..=max).contains(&value) {
            return Err(RpcError::invalid_params(format!(
                "selection.{field} must be in {range}, got {value}"
            ))
            .with_data(json!({
                "kind": "llm_param_out_of_range",
                "field": format!("selection.{field}"),
                "range": range,
            })));
        }
        Ok(())
    }
    check_f64_range(selection.temperature, "temperature", "0.0..=2.0", 0.0, 2.0)?;
    check_f64_range(selection.top_p, "top_p", "0.0..=1.0", 0.0, 1.0)?;
    if selection.context_window == Some(0) {
        return Err(
            RpcError::invalid_params("selection.context_window must be >= 1 token").with_data(
                json!({
                    "kind": "llm_param_out_of_range",
                    "field": "selection.context_window",
                    "range": ">=1",
                }),
            ),
        );
    }
    Ok(())
}

/// Shared param pipeline for `profile/llm/upsert` / `test` /
/// `test`/`upsert`: unknown-field pre-pass → typed deserialize (with
/// `deny_unknown_fields`) → range validation. One place so Test and Save
/// can never drift.
fn parse_llm_selection_params(
    request: &RpcRequest<Value>,
) -> Result<RawProfileLlmUpsertParams, RpcError> {
    reject_unknown_llm_upsert_fields(&request.params)?;
    let params: RawProfileLlmUpsertParams = parse_raw_params(request)?;
    validate_llm_inference_fields(&params.selection)?;
    Ok(params)
}

/// `profile/llm/delete`: remove one configured model (primary or fallback) by
/// its address (family + model + route).
#[derive(Debug, Deserialize)]
struct RawProfileLlmDeleteParams {
    #[serde(default)]
    profile_id: Option<String>,
    family_id: String,
    model_id: String,
    route_id: String,
}

/// One named provider lane on the wire (mirrors [`crate::config::SubProviderConfig`]).
/// `key` is required; the rest are optional so the TUI can send whatever the
/// user filled in.
#[derive(Debug, Default, Deserialize)]
struct RawSubProvider {
    key: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default_context_window: Option<u32>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    api_type: Option<String>,
}

/// `profile/sub_providers/upsert`: add or replace (by `key`) one named lane.
#[derive(Debug, Default, Deserialize)]
struct RawProfileSubProvidersUpsertParams {
    #[serde(default)]
    profile_id: Option<String>,
    sub_provider: RawSubProvider,
    /// Optional secret to store under the lane's `api_key_env` (never persisted
    /// to plaintext config — relocated to the OS keychain like the llm path).
    #[serde(default)]
    api_key: Option<Value>,
}

/// `profile/sub_providers/remove`: drop the lane with this `key`.
#[derive(Debug, Deserialize)]
struct RawProfileSubProvidersRemoveParams {
    #[serde(default)]
    profile_id: Option<String>,
    key: String,
}

fn parse_raw_params<T>(request: &RpcRequest<Value>) -> Result<T, RpcError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(request.params.clone())
        .map_err(|err| RpcError::invalid_params(format!("{} params: {err}", request.method)))
}

fn parse_optional_raw_params<T>(request: &RpcRequest<Value>) -> Result<T, RpcError>
where
    T: serde::de::DeserializeOwned + Default,
{
    if request.params.is_null() {
        return Ok(T::default());
    }
    parse_raw_params(request)
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

fn secret_from_value(value: Option<Value>) -> Option<String> {
    match value? {
        Value::String(secret) => nonempty(Some(secret)),
        Value::Object(mut object) => object
            .remove("value")
            .or_else(|| object.remove("secret"))
            .or_else(|| object.remove("api_key"))
            .and_then(|value| value.as_str().map(str::to_owned))
            .and_then(|value| nonempty(Some(value))),
        _ => None,
    }
}

fn raw_profile_id(params: &RawProfileParams, connection_profile_id: Option<&str>) -> String {
    nonempty(params.profile_id.clone())
        .or_else(|| {
            params
                .session_id
                .as_ref()
                .and_then(|session_id| session_id.profile_id().map(ToOwned::to_owned))
        })
        .or_else(|| connection_profile_id.map(ToOwned::to_owned))
        .unwrap_or_else(|| MAIN_PROFILE_ID.to_string())
}

/// Whether the no-password local-solo profile primitive (`profile/local/create`,
/// the TUI onboarding path) is available.
///
/// SECURITY: this requires the explicit `solo_login_enabled` opt-in, NOT just
/// Local mode. The hosted fleet runs Local mode behind a Caddy reverse proxy,
/// so a proxied client reaches the daemon over loopback; without this gate it
/// could create a top-level profile over EITHER transport. Fleet configs
/// never set the opt-in, so solo stays off there; a genuine solo install runs
/// `octos serve --solo` / `OCTOS_SOLO_LOGIN=1`.
pub(crate) fn supports_local_solo_profile_create(state: &AppState) -> bool {
    state.solo_login_enabled && state.profile_store.is_some()
}

/// Whether this server is a genuine local single-user box that may opt into
/// dangerous permission profiles ("yolo" / `danger_full_access`, approvals
/// `never`, network `allow`).
///
/// SECURITY KEYSTONE (yolo GAP #1): this requires the explicit `--solo`
/// opt-in (`solo_login_enabled`) IN ADDITION TO `deployment_mode == Local`.
/// Bare Local mode is NOT sufficient — a hosted fleet daemon runs Local mode
/// behind a Caddy reverse proxy, so mapping Local unconditionally onto the
/// dangerous `RuntimeMode::Solo` would let a proxied client be talked into
/// full host access even though the daemon's *solo login* is separately
/// gated off. Tying the dangerous-profile relaxation to the SAME opt-in that
/// gates `profile/local/create` (see [`supports_local_solo_profile_create`])
/// removes that asymmetry: a fleet config that never sets `--solo` can reach
/// neither surface. Unlike the solo-login predicate this does NOT require the
/// profile store, because a dangerous session runtime is bootstrapped
/// independently of the no-password login primitive.
fn local_solo_danger_allowed(state: &AppState) -> bool {
    state.solo_login_enabled
}

fn local_profile_error(kind: &str, message: impl Into<String>) -> RpcError {
    RpcError::invalid_params(message).with_data(json!({ "kind": kind }))
}

fn profile_unresolved_error(profile_id: &str) -> RpcError {
    RpcError::invalid_params(format!(
        "profile '{profile_id}' is not configured for this AppUI session"
    ))
    .with_data(json!({
        "kind": "profile_unresolved",
        "profile_id": profile_id,
        "recoverable": true,
        "recovery": "create or select a local profile before opening the session",
    }))
}

fn auth_unavailable_error(method: &str) -> RpcError {
    RpcError::permission_denied(format!("{method}: authenticated user identity required"))
        .with_data(json!({
            "kind": "auth_unavailable",
            "recoverable": true,
            "recovery": "authenticate before calling this method",
        }))
}

fn profile_is_known(state: &AppState, profile_id: &str) -> bool {
    state
        .profile_store
        .as_ref()
        .and_then(|store| store.get(profile_id).ok().flatten())
        .is_some()
        || resolve_session_profile_runtime(state, Some(profile_id)).is_some()
        || (profile_id == MAIN_PROFILE_ID && state.profile_store.is_none())
}

fn ensure_known_profile(state: &AppState, profile_id: &str) -> Result<(), RpcError> {
    if profile_is_known(state, profile_id) {
        Ok(())
    } else {
        Err(profile_unresolved_error(profile_id))
    }
}

fn local_profile_permission_error(
    kind: &str,
    message: impl Into<String>,
    state: &AppState,
) -> RpcError {
    RpcError::permission_denied(message).with_data(json!({
        "kind": kind,
        "runtime_mode": runtime_mode_for_state(state),
    }))
}

fn runtime_mode_for_state(_state: &AppState) -> &'static str {
    "solo"
}

fn validate_local_name(name: &str) -> Result<String, RpcError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(local_profile_error(
            "profile_local_invalid_name",
            "name is required",
        ));
    }
    if trimmed.len() > 128 || trimmed.chars().any(char::is_control) {
        return Err(local_profile_error(
            "profile_local_invalid_name",
            "name must be 1-128 printable characters",
        ));
    }
    Ok(trimmed.to_owned())
}

fn normalize_local_username(username: &str) -> Result<String, RpcError> {
    let trimmed = username.trim();
    if trimmed.is_empty() {
        return Err(local_profile_error(
            "profile_local_invalid_username",
            "username is required",
        ));
    }
    let mut normalized = String::with_capacity(trimmed.len());
    let mut last_was_hyphen = false;
    for c in trimmed.chars() {
        if c.is_ascii_alphanumeric() {
            normalized.push(c.to_ascii_lowercase());
            last_was_hyphen = false;
        } else if matches!(c, '-' | '_' | '.') {
            if !last_was_hyphen {
                normalized.push('-');
                last_was_hyphen = true;
            }
        } else {
            return Err(local_profile_error(
                "profile_local_invalid_username",
                "username may contain only ASCII letters, digits, hyphen, underscore, or dot",
            ));
        }
    }
    let normalized = normalized.trim_matches('-').to_owned();
    if normalized.is_empty() || normalized.len() > 64 {
        return Err(local_profile_error(
            "profile_local_invalid_username",
            "normalized username must be 1-64 characters",
        ));
    }
    // The username becomes BOTH the user id and the profile id.
    // Profile ids reject reserved channel names (ambiguous session
    // keys — see profiles::validate_profile_id), so reject here BEFORE
    // any record persists: previously the user record was saved first
    // and only the later profile save failed, leaving a valid user
    // with no usable profile (codex #1613 r5).
    if octos_core::is_reserved_channel_name(&normalized) {
        return Err(local_profile_error(
            "profile_local_invalid_username",
            "username must not be a reserved channel name (e.g. api, slack, line)",
        ));
    }
    Ok(normalized)
}

fn profile_metadata_from_file(
    store: &crate::profiles::ProfileStore,
    profile_id: &str,
) -> Result<Option<String>, RpcError> {
    let path = store.profile_path(profile_id);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => {
            return Err(runtime_unavailable_error(format!(
                "failed to read profile metadata: {error}"
            )));
        }
    };
    let value: Value = serde_json::from_str(&content).map_err(|error| {
        runtime_unavailable_error(format!("failed to parse profile metadata: {error}"))
    })?;
    Ok(value
        .get("username")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned))
}

fn write_local_profile_metadata(
    store: &crate::profiles::ProfileStore,
    profile: &crate::profiles::UserProfile,
    username: &str,
) -> Result<(), RpcError> {
    let path = store.profile_path(&profile.id);
    let mut value = serde_json::to_value(profile).map_err(|error| {
        runtime_unavailable_error(format!("failed to serialize profile metadata: {error}"))
    })?;
    let Some(object) = value.as_object_mut() else {
        return Err(runtime_unavailable_error(
            "serialized profile metadata was not an object",
        ));
    };
    object.insert("username".to_owned(), Value::String(username.to_owned()));
    let content = serde_json::to_string_pretty(&value).map_err(|error| {
        runtime_unavailable_error(format!("failed to encode profile metadata: {error}"))
    })?;
    std::fs::write(&path, content)
        .map_err(|error| runtime_unavailable_error(format!("failed to write profile: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&path, perms);
    }
    Ok(())
}

fn profile_collision_error(profile_id: &str, reason: &str) -> RpcError {
    local_profile_error(
        "profile_local_collision",
        format!("local profile '{profile_id}' already exists with different {reason}"),
    )
    .with_data(json!({
        "kind": "profile_local_collision",
        "profile_id": profile_id,
        "reason": reason,
    }))
}

fn ensure_existing_local_profile_matches(
    profile_id: &str,
    profile: Option<&crate::profiles::UserProfile>,
    expected_name: &str,
) -> Result<(), RpcError> {
    if let Some(profile) = profile {
        if profile.name != expected_name {
            return Err(profile_collision_error(profile_id, "name"));
        }
    }
    Ok(())
}

/// Maximum profile-id slug length (mirrors `profiles::validate_slug_shape`).
const LOCAL_PROFILE_ID_MAX_LEN: usize = 64;

/// Upper bound on the numeric suffix tried when auto-suffixing a requested
/// profile id (`glm`, `glm-2`, ..., `glm-10000`). A store with more collisions
/// than this falls back to a uuid-based id, so id assignment always
/// terminates instead of spinning on a pathological store.
const MAX_LOCAL_PROFILE_ID_SUFFIX: u32 = 10_000;

/// Serializes local-solo profile CREATION (id assignment through
/// persistence). Without it, two concurrent creates for the same
/// `requested_id` can BOTH pass the free-id checks before either save runs,
/// then write the same record and clash on the shared `.json.tmp` paths
/// instead of one getting `glm` and the other `glm-2`. A single serve process
/// shares one profiles dir, so a process-wide lock is the reservation
/// boundary that makes find-free + create atomic. Held only across
/// synchronous store I/O (never across an `.await`); poison is recovered
/// because one panicking creator must not brick all future ones.
static LOCAL_PROFILE_CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Normalize a user-requested profile id into slug shape: lowercase ASCII
/// alphanumerics are kept, every other run collapses to a single `-`, edge
/// `-` are trimmed, and the result is capped at 64 bytes. Returns `None` when
/// nothing usable remains, or when the result would collide with a reserved
/// id (a session-key channel name or the synthetic `MAIN_PROFILE_ID`), so the
/// caller falls back to a generated id. The output always satisfies
/// `profiles::validate_profile_id`.
fn normalize_requested_profile_id(requested: &str) -> Option<String> {
    let mut slug = String::with_capacity(requested.len());
    let mut last_hyphen = false;
    for c in requested.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_hyphen = false;
        } else if !last_hyphen {
            slug.push('-');
            last_hyphen = true;
        }
    }
    // All bytes are ASCII, so `truncate` never splits a char; trim any hyphen
    // the cut (or the edges) left behind.
    slug.truncate(LOCAL_PROFILE_ID_MAX_LEN);
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty()
        || slug == octos_core::MAIN_PROFILE_ID
        || octos_core::is_reserved_channel_name(&slug)
    {
        return None;
    }
    Some(slug)
}

/// The synthesized owner email reported for a no-email local profile:
/// `<id>@solo.local`. The id is already a lowercase slug ≤64 chars, so the
/// local part (= the id) is ≤64 bytes — within lettre's RFC-5321 limit. With
/// the single-identity model there is no email registry, so this is purely a
/// wire-contract placeholder, never checked for uniqueness.
fn synthesized_local_email(profile_id: &str) -> String {
    format!("{profile_id}@solo.local")
}

/// Whether `id` is free to claim: not reserved, no profile file, and no
/// profile record. Normalized slug input assumed.
fn local_profile_candidate_is_free(
    id: &str,
    profile_store: &crate::profiles::ProfileStore,
) -> bool {
    if id == octos_core::MAIN_PROFILE_ID || octos_core::is_reserved_channel_name(id) {
        return false;
    }
    if profile_store.profile_path(id).exists() {
        return false;
    }
    matches!(profile_store.get(id), Ok(None))
}

/// Assign a unique local profile id from a normalized `base` slug by trying
/// `base`, then `base-2`, `base-3`, ... until a free id is found (bounded by
/// [`MAX_LOCAL_PROFILE_ID_SUFFIX`]). Returns `None` if the bound is
/// exhausted, so the caller can fall back to a uuid-based id.
fn assign_unique_local_profile_id(
    base: &str,
    profile_store: &crate::profiles::ProfileStore,
) -> Option<String> {
    for n in 1..=MAX_LOCAL_PROFILE_ID_SUFFIX {
        let candidate = if n == 1 {
            base.to_owned()
        } else {
            // Keep `base-N` within the slug budget by trimming the base (not
            // the numeric tag), then drop any hyphen the trim exposed.
            let tag = format!("-{n}");
            let budget = LOCAL_PROFILE_ID_MAX_LEN.saturating_sub(tag.len());
            let mut trimmed = base.to_owned();
            trimmed.truncate(budget);
            let trimmed = trimmed.trim_end_matches('-');
            if trimmed.is_empty() {
                continue;
            }
            format!("{trimmed}{tag}")
        };
        if local_profile_candidate_is_free(&candidate, profile_store) {
            return Some(candidate);
        }
    }
    None
}

/// Generate a guaranteed-unique, slug-valid local profile id when no usable
/// requested id or username is available: derive a slug from `seed` (a display
/// name) when possible, else a uuid-based id. Always collision-free.
fn generated_local_profile_id(seed: &str, profile_store: &crate::profiles::ProfileStore) -> String {
    if let Some(id) = normalize_requested_profile_id(seed)
        .and_then(|base| assign_unique_local_profile_id(&base, profile_store))
    {
        return id;
    }
    loop {
        // A v7 UUID renders as lowercase hex + hyphens with no leading/trailing
        // hyphen, so `profile-<uuid>` is always slug-valid and well within 64
        // chars. The loop guards the (practically impossible) collision.
        let candidate = format!("profile-{}", uuid::Uuid::now_v7());
        if local_profile_candidate_is_free(&candidate, profile_store) {
            return candidate;
        }
    }
}

/// Resolve the display name for a nameable local profile without erroring: the
/// provided `name` when it is a valid 1-128 printable string, else the trimmed
/// original `requested_id` when valid, else the assigned `profile_id`. A
/// profile therefore always ends up with SOME display name.
fn resolve_local_display_name(
    params: &octos_core::ui_protocol::ProfileLocalCreateParams,
    profile_id: &str,
) -> String {
    let is_displayable = |candidate: &str| {
        let trimmed = candidate.trim();
        !trimmed.is_empty() && trimmed.len() <= 128 && !trimmed.chars().any(char::is_control)
    };
    if is_displayable(&params.name) {
        return params.name.trim().to_owned();
    }
    if let Some(requested) = params.requested_id.as_deref() {
        if is_displayable(requested) {
            return requested.trim().to_owned();
        }
    }
    profile_id.to_owned()
}

/// Create a fresh, uniquely-named local solo profile. Used when the client
/// sends a `requested_id` (normalized + collision-suffixed) or omits the legacy
/// `username` entirely (id derived from the display name, else generated).
/// Unlike the legacy username path this always creates a NEW profile: the
/// assigned id is guaranteed free, so there is never an existing record to
/// reconcile. Uniqueness is enforced against the profile store only — the
/// single-identity model has no user/email registry.
fn create_fresh_local_solo_profile(
    profile_store: &crate::profiles::ProfileStore,
    params: &octos_core::ui_protocol::ProfileLocalCreateParams,
    requested_slug: Option<String>,
) -> Result<octos_core::ui_protocol::ProfileLocalCreateResult, RpcError> {
    let profile_id = match requested_slug {
        Some(base) => assign_unique_local_profile_id(&base, profile_store)
            .unwrap_or_else(|| generated_local_profile_id(&base, profile_store)),
        None => generated_local_profile_id(params.name.trim(), profile_store),
    };

    let name = resolve_local_display_name(params, &profile_id);
    let username = profile_id.clone();
    let email = synthesized_local_email(&profile_id);
    let now = Utc::now();

    let profile = crate::profiles::UserProfile {
        id: profile_id.clone(),
        name: name.clone(),
        public_subdomain: None,
        enabled: true,
        data_dir: None,
        parent_id: None,
        config: crate::profiles::ProfileConfig::default(),
        created_at: now,
        updated_at: now,
    };
    profile_store
        .save(&profile)
        .map_err(|error| runtime_unavailable_error(format!("failed to save profile: {error}")))?;
    write_local_profile_metadata(profile_store, &profile, &username)?;
    persist_default_if_requested(profile_store, params, &profile_id)?;

    Ok(octos_core::ui_protocol::ProfileLocalCreateResult {
        profile_id: profile_id.clone(),
        user_id: profile_id,
        name,
        username,
        email,
        created: true,
        runtime_mode: "solo".to_owned(),
    })
}

/// Record the just-created/resolved profile as the machine's global default
/// when the client set `make_default`. A no-op when the flag is absent or
/// false. Called on every `profile/local/create` success path so "make default"
/// applies whether the profile was freshly minted or already existed.
fn persist_default_if_requested(
    profile_store: &crate::profiles::ProfileStore,
    params: &octos_core::ui_protocol::ProfileLocalCreateParams,
    profile_id: &str,
) -> Result<(), RpcError> {
    if params.make_default.unwrap_or(false) {
        profile_store
            .set_default_profile(profile_id)
            .map_err(|error| {
                runtime_unavailable_error(format!("failed to set default profile: {error}"))
            })?;
    }
    Ok(())
}

pub(crate) fn create_or_get_local_solo_profile(
    state: &AppState,
    params: octos_core::ui_protocol::ProfileLocalCreateParams,
) -> Result<octos_core::ui_protocol::ProfileLocalCreateResult, RpcError> {
    if !supports_local_solo_profile_create(state) {
        return Err(local_profile_permission_error(
            "profile_local_unsupported",
            "profile/local/create is available only in local solo mode",
            state,
        ));
    }

    let profile_store = profile_store(state)?;

    // Reserve the id and persist under one process-wide lock so a concurrent
    // double-submit cannot both pass the free-id checks before either save
    // runs. Covers BOTH the fresh (requested_id / generated) path and the
    // legacy username path (whose `.json.tmp` write-then-rename would
    // otherwise race a same-username peer). Held only across synchronous store
    // I/O below — the function returns (dropping the guard) before any caller
    // `.await`s.
    let _create_guard = LOCAL_PROFILE_CREATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // User-nameable onboarding: an explicit `requested_id` assigns a fresh,
    // collision-suffixed id; a request that omits the legacy `username` derives
    // the id from the display name (or generates one). Both create a NEW
    // profile — the assigned id is guaranteed free — instead of reconciling
    // against an existing username the way the legacy path below does. A
    // requested_id that normalizes to nothing usable (empty / reserved) is
    // treated as absent, deferring to the username path when one was sent.
    let requested_slug = params
        .requested_id
        .as_deref()
        .and_then(normalize_requested_profile_id);
    let has_legacy_username = !params.username.trim().is_empty();
    if requested_slug.is_some() || !has_legacy_username {
        return create_fresh_local_solo_profile(&profile_store, &params, requested_slug);
    }

    // ---- Legacy path: id derived from the provided username (idempotent
    // create-or-get; behavior unchanged apart from the removed user/email
    // registry). ----
    let name = validate_local_name(&params.name)?;
    let profile_id = normalize_local_username(&params.username)?;
    let username = profile_id.clone();
    let email = synthesized_local_email(&profile_id);

    let existing_profile = profile_store
        .get(&profile_id)
        .map_err(|error| runtime_unavailable_error(format!("failed to read profile: {error}")))?;
    ensure_existing_local_profile_matches(&profile_id, existing_profile.as_ref(), &name)?;

    if let Some(profile) = existing_profile.as_ref() {
        let stored_username = profile_metadata_from_file(&profile_store, &profile_id)?;
        if stored_username.is_some() && stored_username.as_deref() != Some(username.as_str()) {
            return Err(profile_collision_error(&profile_id, "username"));
        }
        // Idempotent re-create: the profile (and its metadata) already match.
        write_local_profile_metadata(&profile_store, profile, &username)?;
        persist_default_if_requested(&profile_store, &params, &profile_id)?;
        return Ok(octos_core::ui_protocol::ProfileLocalCreateResult {
            profile_id: profile_id.clone(),
            user_id: profile_id,
            name,
            username,
            email,
            created: false,
            runtime_mode: "solo".to_owned(),
        });
    }

    let now = Utc::now();
    let profile = crate::profiles::UserProfile {
        id: profile_id.clone(),
        name: name.clone(),
        public_subdomain: None,
        enabled: true,
        data_dir: None,
        parent_id: None,
        config: crate::profiles::ProfileConfig::default(),
        created_at: now,
        updated_at: now,
    };
    profile_store
        .save(&profile)
        .map_err(|error| runtime_unavailable_error(format!("failed to save profile: {error}")))?;
    write_local_profile_metadata(&profile_store, &profile, &username)?;
    persist_default_if_requested(&profile_store, &params, &profile_id)?;

    Ok(octos_core::ui_protocol::ProfileLocalCreateResult {
        profile_id: profile_id.clone(),
        user_id: profile_id,
        name,
        username,
        email,
        created: true,
        runtime_mode: "solo".to_owned(),
    })
}

fn default_profile(profile_id: &str) -> crate::profiles::UserProfile {
    let now = Utc::now();
    crate::profiles::UserProfile {
        id: profile_id.to_owned(),
        name: profile_id.to_owned(),
        public_subdomain: None,
        enabled: true,
        data_dir: None,
        parent_id: None,
        config: crate::profiles::ProfileConfig::default(),
        created_at: now,
        updated_at: now,
    }
}

fn profile_store(state: &AppState) -> Result<Arc<crate::profiles::ProfileStore>, RpcError> {
    state
        .profile_store
        .clone()
        .ok_or_else(|| runtime_unavailable_error("profile store not available"))
}

fn configured_provider_json(
    selection: &crate::profiles::LlmModelSelectionConfig,
    env_vars: &HashMap<String, String>,
    selected: bool,
) -> Value {
    let route = selection.route.clone().unwrap_or_default();
    let family_id = selection.family_id.clone();
    let model_id = selection.model_id.clone();
    let route_id = route.route_id.clone();
    let api_key_env = route.api_key_env.clone();
    let mut provider = json!({
        "provider": family_id.clone().unwrap_or_default(),
        "model": model_id.clone().unwrap_or_default(),
        "family_id": family_id,
        "model_id": model_id,
        "route": route,
        "route_id": route_id,
        "base_url": route.base_url,
        "api_key_env": api_key_env,
        "has_api_key": api_key_env
            .as_deref()
            .is_some_and(|key| env_vars.get(key).is_some_and(|value| !value.is_empty())),
        "selected": selected,
        "available": true,
    });
    // #2166 typed inference schema round-trip: echo back every configured
    // inference/routing field so a client can distinguish "saved" from
    // "inherited". Keys are added ONLY when configured — a selection with no
    // overrides serializes exactly as before #2166 (unconfigured default
    // behavior unchanged), and `null` on the wire always means the same as
    // an absent key: inherit.
    let object = provider
        .as_object_mut()
        .expect("configured_provider_json builds an object");
    if let Some(context_window) = selection.context_window {
        object.insert("context_window".into(), json!(context_window));
    }
    if let Some(temperature) = selection.temperature {
        object.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = selection.top_p {
        object.insert("top_p".into(), json!(top_p));
    }
    if let Some(reasoning_effort) = selection.reasoning_effort {
        object.insert("reasoning_effort".into(), json!(reasoning_effort));
    }
    if let Some(model_hints) = selection.model_hints.clone() {
        object.insert("model_hints".into(), json!(model_hints));
    }
    if let Some(cost_per_m) = selection.cost_per_m {
        object.insert("cost_per_m".into(), json!(cost_per_m));
    }
    if let Some(strong) = selection.strong {
        object.insert("strong".into(), json!(strong));
    }
    provider
}

fn permission_profile_supported_selections(
    state: &AppState,
    session_id: &SessionKey,
) -> Vec<octos_core::ui_protocol::PermissionProfileSelection> {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
        PermissionProfileSelection as Selection,
    };

    let mut profiles = vec![
        Selection {
            mode: Mode::ReadOnly,
            network: Network::Deny,
        },
        Selection {
            mode: Mode::WorkspaceWrite,
            network: Network::Deny,
        },
    ];
    // #1162 codex P2 — keep list output consistent with the
    // session-scope gate added to `permission_profile_set`. On a Local
    // server we only advertise `danger_full_access` for sessions whose
    // structural profile_id is NOT tenant/cloud-scoped; tenant-scoped
    // sessions get `permission_profile_disallowed` from `set`, so the
    // list must omit the dead option rather than render it as a
    // selectable profile.
    //
    // SECURITY KEYSTONE (yolo GAP #1): additionally require the `--solo`
    // opt-in — a Caddy-fronted fleet daemon runs Local mode but must NOT
    // advertise `danger_full_access` as selectable, mirroring the same gate
    // in `permission_selection_allowed` / `effective_permissions_for_session`.
    let server_is_local = local_solo_danger_allowed(state);
    let session_is_non_solo_scoped = session_id_encodes_non_solo_scope(session_id);
    if server_is_local && !session_is_non_solo_scoped {
        profiles.push(Selection {
            mode: Mode::DangerFullAccess,
            network: Network::Allow,
        });
    }
    profiles
}

fn permission_selection_policy_fields(
    selection: octos_core::ui_protocol::PermissionProfileSelection,
    approval_policy: Option<octos_agent::ApprovalPolicy>,
) -> (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
) {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
    };

    let approval_policy = if selection.mode == Mode::DangerFullAccess
        || approval_policy == Some(octos_agent::ApprovalPolicy::Never)
    {
        "never"
    } else {
        "on-request"
    };
    let network = if selection.network == Network::Allow {
        "allowed"
    } else {
        "blocked"
    };

    match selection.mode {
        Mode::DangerFullAccess => (
            approval_policy,
            "danger-full-access",
            "danger_full_access",
            "host",
            network,
        ),
        Mode::ReadOnly => (
            approval_policy,
            "read-only",
            "read_only",
            "workspace",
            network,
        ),
        Mode::WorkspaceWrite => (
            approval_policy,
            "workspace-write",
            "workspace_write",
            "workspace",
            network,
        ),
    }
}

fn parse_permission_approval_policy(
    value: Option<&str>,
) -> Result<Option<octos_agent::ApprovalPolicy>, RpcError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    match value {
        "never" => Ok(Some(octos_agent::ApprovalPolicy::Never)),
        "ask" | "on-request" | "on_request" => Ok(Some(octos_agent::ApprovalPolicy::Ask)),
        other => Err(
            RpcError::invalid_params(format!("unsupported approval_policy '{other}'")).with_data(
                json!({
                    "kind": "permission_profile_invalid_approval_policy",
                    "approval_policy": other,
                }),
            ),
        ),
    }
}

fn permission_profile_disallowed_error(
    state: &AppState,
    selection: octos_core::ui_protocol::PermissionProfileSelection,
    approval_policy: Option<octos_agent::ApprovalPolicy>,
) -> RpcError {
    let (approval_policy, _, permission_profile, _, network) =
        permission_selection_policy_fields(selection, approval_policy);
    RpcError::permission_denied(
        "requested permission profile is not allowed outside local solo mode",
    )
    .with_data(json!({
        "kind": "permission_profile_disallowed",
        "runtime_mode": runtime_mode_for_state(state),
        "permission_profile": permission_profile,
        "approval_policy": approval_policy,
        "network": network,
    }))
}

/// #1162 — Returns `true` when the supplied session key carries a
/// non-solo tenant/cloud scope marker in a STRUCTURAL position of the
/// base key. The M12-G soak negative probe creates sessions like
/// `tenant-a:api:m12-negative#…` and `coding:tenant:m12-negative#…`
/// and asserts that the gate rejects `danger_full_access` even when
/// the client omits the `runtime_mode` override (UPCR-2026-018 /
/// #1086). This helper layers the session-shape signal on top of the
/// override gate so a non-conforming or older client cannot slip
/// dangerous mode past the policy boundary.
///
/// Two structural positions of the base key are checked. Parsing
/// does NOT rely on `SessionKey::profile_id()` because that helper's
/// recognition list is narrower than the gateway's set of supported
/// channels (codex P1 round 3 #1167 caught the gap for `line`,
/// `wechat`, etc.) — we always interpret the FIRST segment as the
/// candidate `profile_id` and disambiguate against the registered
/// channel list locally.
///
/// 1. First segment when it is NOT a registered channel — treat as a
///    candidate `profile_id` and match `tenant` / `cloud` exactly
///    (case-insensitive, trimmed) or anything starting with
///    `tenant-` / `cloud-`. This catches the `with_profile` /
///    `with_profile_topic` shape (`tenant-a:api:…`,
///    `tenant--child:api:…`, `cloud--root:api:…`, `tenant-a:line:…`).
/// 2. Second-of-three segment when it equals exactly `tenant` or
///    `cloud` AND the 1st segment is NOT a registered channel —
///    catches the M12-G soak shape `{profile}:tenant:{chat}` /
///    `{profile}:cloud:{chat}` (`coding:tenant:m12-negative`,
///    `m12solo:cloud:m12-negative`). The match is EXACT (no
///    `starts_with`) so chat-id text like `_main:api:tenant-demo` is
///    not affected (2nd slot there is `api`, a registered channel,
///    so the structural form there is `profile=_main`, `channel=api`,
///    `chat=tenant-demo`). The 1st-segment guard also prevents the
///    false positive on legacy `{channel}:{chat_id_with_colons}`
///    sessions like `local:tenant:123` where the chat-id text itself
///    contains `tenant:`.
///
/// Codex review history on #1167:
/// - P2 round 1: restrict the scan to STRUCTURAL slots (chat-id text
///   is arbitrary; `local:cloud-migration` is legitimate).
/// - P1 round 2: also catch the soak's `{profile}:tenant:{chat}` shape
///   where `tenant`/`cloud` sits in the channel slot.
/// - P2 round 2: the channel-slot check must NOT fire when the 1st
///   segment is a registered channel — that's a legacy
///   `{channel}:{chat_id_with_colons}` session, not a tenant scope.
/// - P1 round 3: don't rely on `SessionKey::profile_id()` — it returns
///   `None` for profiled sessions on channels that aren't in the core
///   recognition list (`line`, `wechat`, …), so the gate must parse
///   the first segment with the full registered-channel set locally.
fn session_id_encodes_non_solo_scope(session_id: &SessionKey) -> bool {
    fn token_marks_non_solo(raw: &str) -> bool {
        let token = raw.trim().to_ascii_lowercase();
        token == "tenant"
            || token == "cloud"
            || token.starts_with("tenant-")
            || token.starts_with("cloud-")
    }

    let mut parts = session_id.base_key().splitn(3, ':');
    let first = parts.next();
    let second = parts.next();
    let third = parts.next();

    // (1) Candidate profile_id slot: 1st segment when it is NOT a
    // registered channel name. This is structural — chat-id text
    // cannot occupy this slot because a 2-segment base key with a
    // registered-channel 1st segment is a legacy
    // `{channel}:{chat_id}` shape (and we don't read the 1st segment
    // as profile_id in that case).
    if let Some(first) = first {
        if !is_registered_channel_name(first) && token_marks_non_solo(first) {
            return true;
        }

        // (2) Channel-slot scope marker: 2nd segment when it equals
        // exactly `tenant`/`cloud` AND the 1st segment is NOT a
        // registered channel. The 1st-segment guard prevents the
        // false positive on `local:tenant:123` where `tenant:123` is
        // chat-id text.
        if !is_registered_channel_name(first) {
            if let (Some(second), Some(_third)) = (second, third) {
                let channel_slot = second.trim().to_ascii_lowercase();
                if channel_slot == "tenant" || channel_slot == "cloud" {
                    return true;
                }
            }
        }
    }

    false
}

/// Registered gateway-channel name set used by
/// `session_id_encodes_non_solo_scope` to distinguish a legacy
/// `{channel}:{chat_id_with_colons}` session from a tenant-scoped
/// `{profile}:tenant:{chat}` session. This is a SUPERSET of
/// `octos_core::types::is_channel_name` — core's list omits
/// feature-gated channels (`line`, `wechat`, `mock`) that the
/// gateway / bus crates emit in `BusMessage.channel`. Codex round 4
/// review (#1167) caught the gap: a legacy
/// `SessionKey::new("line", "tenant:123")` whose chat-id text starts
/// with `tenant:` would be misclassified as tenant-scoped without
/// recognising `line` here. Keep in sync when new gateway channels
/// are added.
fn is_registered_channel_name(value: &str) -> bool {
    matches!(
        value,
        "api"
            | "cli"
            | "dingtalk"
            | "discord"
            | "email"
            | "feishu"
            | "line"
            | "local"
            | "matrix"
            | "mock"
            | "qq-bot"
            | "slack"
            | "system"
            | "telegram"
            | "test"
            | "twilio"
            | "wechat"
            | "wecom"
            | "wecom-bot"
            | "whatsapp"
    )
}

fn permission_selection_allowed(
    state: &AppState,
    session_id: &SessionKey,
    selection: octos_core::ui_protocol::PermissionProfileSelection,
    approval_policy: Option<octos_agent::ApprovalPolicy>,
    requested_runtime_mode: Option<&str>,
) -> bool {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
    };

    // Only treat the request as solo-relaxed when it explicitly says so
    // (or omits the override). The UPCR-2026-018 runtime_mode override
    // contract is "an explicit value may only tighten, never relax", so
    // the relax path triggers only on `None`, empty, or exactly `solo`.
    // Codex P1 #1086 first extended this to reject `local`/`tenant`/`cloud`
    // explicit non-solo values. Codex P2 re-review on #1121 then flagged
    // that any UNRECOGNIZED override (e.g. `multi_tenant`, whitespace-
    // wrapped, future spec values) was falling through to the relaxed
    // branch — we now fail closed by inverting the predicate: the gate
    // relaxes ONLY for explicit solo / omitted, and tightens for
    // everything else.
    // Codex P2 re-review #3 on #1121: a blank-string or whitespace-only
    // override is an explicit malformed value, not "omitted". Treat it
    // as non-solo so the gate tightens. Only `None` (truly absent) and
    // a normalized `solo` keep the relaxed path on a Local server.
    let normalized_override = requested_runtime_mode.map(|raw| raw.trim().to_ascii_lowercase());
    let request_relaxes_to_solo = matches!(normalized_override.as_deref(), None | Some("solo"));
    // SECURITY KEYSTONE (yolo GAP #1): the relax path requires the explicit
    // `--solo` opt-in, NOT bare Local mode. A Caddy-fronted fleet daemon runs
    // Local mode, so `deployment_mode == Local` alone is not a safe proxy for
    // "single-user box" — see `local_solo_danger_allowed`.
    let server_is_local = local_solo_danger_allowed(state);
    // #1162: defense-in-depth. A session whose key encodes a non-solo
    // tenant scope (e.g. `coding:tenant:m12-negative`) MUST tighten the
    // gate even when the client omitted the `runtime_mode` override.
    // The M12-G soak negative probe relies on this signal so a
    // non-conforming client cannot slip dangerous mode past the policy
    // boundary by simply leaving the override out.
    let session_is_non_solo_scoped = session_id_encodes_non_solo_scope(session_id);
    let effective_local = server_is_local && request_relaxes_to_solo && !session_is_non_solo_scoped;

    effective_local
        || (selection.mode != Mode::DangerFullAccess
            && selection.network != Network::Allow
            && approval_policy != Some(octos_agent::ApprovalPolicy::Never))
}

fn effective_permissions_for_session(
    state: &AppState,
    session_id: &SessionKey,
) -> Result<octos_agent::EffectivePermissions, RpcError> {
    use octos_core::ui_protocol::{
        PermissionNetworkPolicy as Network, PermissionProfileMode as Mode,
    };

    let permission_state = effective_session_permission_state(state, session_id);
    let requested = match permission_state.selection.mode {
        Mode::ReadOnly => octos_agent::PermissionProfile::ReadOnly,
        Mode::WorkspaceWrite => octos_agent::PermissionProfile::WorkspaceWrite,
        Mode::DangerFullAccess => octos_agent::PermissionProfile::DangerFullAccess,
    };
    // SECURITY KEYSTONE (yolo GAP #1): map Local onto the dangerous-capable
    // `RuntimeMode::Solo` ONLY when the operator opted in via `--solo`.
    // Without the opt-in, a Local server resolves as `RuntimeMode::Local`, so
    // `EffectivePermissions::for_runtime` rejects `danger_full_access` (a
    // Caddy-fronted fleet daemon can never bootstrap a dangerous session
    // runtime). See `local_solo_danger_allowed`.
    let runtime_mode = if local_solo_danger_allowed(state) {
        octos_agent::RuntimeMode::Solo
    } else {
        octos_agent::RuntimeMode::Local
    };
    let mut permissions = octos_agent::EffectivePermissions::for_runtime(requested, runtime_mode)
        .map_err(|err| {
        RpcError::permission_denied(err.to_string()).with_data(json!({
            "kind": "permission_profile_disallowed",
            "runtime_mode": runtime_mode_for_state(state),
            "permission_profile": format!("{:?}", requested),
        }))
    })?;
    if let Some(approval_policy) = permission_state.approval_policy {
        permissions = permissions.with_approval_policy(approval_policy);
    }
    if permission_state.selection.network == Network::Allow {
        permissions.network = octos_agent::NetworkPolicy::Allowed;
    }
    Ok(permissions)
}

fn permission_profile_list_result(
    state: &AppState,
    params: octos_core::ui_protocol::PermissionProfileListParams,
) -> octos_core::ui_protocol::PermissionProfileListResult {
    let current = effective_session_permission_state(state, &params.session_id).selection;
    let profiles = permission_profile_supported_selections(state, &params.session_id);
    octos_core::ui_protocol::PermissionProfileListResult {
        session_id: params.session_id,
        current,
        profiles,
    }
}

fn permission_profile_set_result(
    state: &AppState,
    params: octos_core::ui_protocol::PermissionProfileSetParams,
) -> Result<octos_core::ui_protocol::PermissionProfileSetResult, RpcError> {
    let store = session_permission_profiles();
    let previous_state = effective_session_permission_state(state, &params.session_id);
    let previous = previous_state.selection;
    let approval_policy =
        parse_permission_approval_policy(params.update.approval_policy.as_deref())?
            .or(previous_state.approval_policy);
    let requested = params.update.apply_to(previous);
    if !permission_selection_allowed(
        state,
        &params.session_id,
        requested,
        approval_policy,
        params.runtime_mode.as_deref(),
    ) {
        return Err(permission_profile_disallowed_error(
            state,
            requested,
            approval_policy,
        ));
    }
    store.set(params.session_id.clone(), requested, approval_policy);
    Ok(octos_core::ui_protocol::PermissionProfileSetResult {
        session_id: params.session_id,
        current: requested,
        applied: requested != previous || approval_policy != previous_state.approval_policy,
    })
}

fn coding_tool_policy_view(
    selection: octos_core::ui_protocol::PermissionProfileSelection,
    approval_policy: Option<octos_agent::ApprovalPolicy>,
) -> super::coding_tool_contract::ToolPolicyView<'static> {
    let (approval_policy, sandbox_mode, _, _, _) =
        permission_selection_policy_fields(selection, approval_policy);
    super::coding_tool_contract::ToolPolicyView {
        tool_policy_id: "profile",
        sandbox_mode,
        approval_policy,
    }
}

fn model_visible_tool_names(registry: Option<&octos_agent::ToolRegistry>) -> Vec<String> {
    let mut names: Vec<String> = registry
        .map(|registry| registry.specs().into_iter().map(|spec| spec.name).collect())
        .unwrap_or_else(|| {
            super::coding_tool_contract::OCTOS_KNOWN_MODEL_VISIBLE_TOOLS
                .iter()
                .map(|name| (*name).to_owned())
                .collect()
        });
    names.sort();
    names.dedup();
    names
}

fn registered_tool_names(registry: Option<&octos_agent::ToolRegistry>) -> Vec<String> {
    let mut names = registry
        .map(|registry| registry.tool_names())
        .unwrap_or_else(|| model_visible_tool_names(None));
    names.sort();
    names.dedup();
    names
}

/// RFC-0 (#1289): LRU tool deferral was removed, so no tool is ever deferred.
/// Retained (returning an empty set) so the coding tool contract's
/// disabled-vs-deferred bookkeeping keeps compiling; every registered-but-
/// -not-visible tool is now genuinely disabled (internal-hidden / policy).
fn deferred_model_tool_names(_registry: Option<&octos_agent::ToolRegistry>) -> Vec<String> {
    Vec::new()
}

async fn tool_status_list_result(
    state: &Arc<AppState>,
    session_id: &SessionKey,
    active_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let profile_runtime = ensure_session_profile_runtime(state, active_profile_id).await?;
    let session_runtime = if let Some(profile_runtime) = profile_runtime.as_ref() {
        // Epoch BEFORE permission resolution so a concurrent downgrade
        // can't pair a post-bump epoch with pre-bump permissions on a
        // preempted caller (codex P1 round 4 on #1639).
        let permissions_epoch = state.session_cache.session_generation(session_id);
        let permissions = effective_permissions_for_session(state, session_id)?;
        let workspace_profile_id = workspace_profile_scope(active_profile_id, session_id);
        let workspace_hint = session_workspaces().runtime_hint(&workspace_profile_id, session_id);
        Some(
            state
                .session_cache
                .get_or_init_with_permissions(
                    profile_runtime,
                    session_id.clone(),
                    workspace_hint,
                    permissions,
                    permissions_epoch,
                )
                .await
                .map_err(|error| {
                    runtime_unavailable_error(format!(
                        "failed to bootstrap session runtime: {error}"
                    ))
                })?,
        )
    } else {
        None
    };
    let registry = session_runtime
        .as_ref()
        .map(|runtime| runtime.tools.as_ref())
        .or_else(|| {
            profile_runtime
                .as_ref()
                .map(|runtime| runtime.tool_specs.as_ref())
        });
    let tool_names = model_visible_tool_names(registry);
    let registered_names = registered_tool_names(registry);
    let deferred_names = deferred_model_tool_names(registry);
    // Codex BLOCK (#1419 review): `deferred_model_tool_names` returns the RAW
    // deferred set — `ToolRegistry::deferred_tool_names()` applies no
    // provider-policy/context filter. A tool that is both deferred AND
    // policy-denied can never be recovered by `activate_tools`
    // (`is_tool_visible_post_activation` stays false), so it must NOT count as
    // a recoverable "deferred" tool: keep it in the disabled set
    // (`disabled_by_policy`) and drop it from the deferred listing the LLM
    // sees. Mirrors the PR #865 standard ("the LLM never sees a name it can't
    // actually call"). Only deferred names that WOULD become visible after
    // activation are the genuine recoverable-deferred set.
    let recoverable_deferred: Vec<String> = deferred_names
        .iter()
        .filter(|name| registry.is_some_and(|r| r.is_tool_visible_post_activation(name)))
        .cloned()
        .collect();
    let visible_names: HashSet<&str> = tool_names.iter().map(String::as_str).collect();
    let deferred_set: HashSet<&str> = recoverable_deferred.iter().map(String::as_str).collect();
    // A registered-but-not-visible tool is either disabled by the effective
    // tool policy OR merely LRU-deferred (registered, filtered out of
    // `specs()`, recoverable via `activate_tools`). The coding tool contract
    // checks the disabled set before the deferred set, so the recoverable
    // deferred names MUST be excluded here — otherwise the canonical P0
    // runtime/subagent tools regress to `disabled_by_policy` and the contract
    // drops to `status: incomplete` on a healthy solo session. A deferred tool
    // that policy still denies is intentionally NOT excluded (it stays
    // disabled_by_policy). #970 / M14-E.
    let disabled_tool_names = registered_names
        .iter()
        .filter(|name| {
            !visible_names.contains(name.as_str()) && !deferred_set.contains(name.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    let tool_name_refs: Vec<&str> = tool_names.iter().map(String::as_str).collect();
    let disabled_tool_refs: Vec<&str> = disabled_tool_names.iter().map(String::as_str).collect();
    let deferred_name_refs: Vec<&str> = recoverable_deferred.iter().map(String::as_str).collect();
    let permission_state = effective_session_permission_state(state, session_id);
    let session_id_wire = session_id.to_string();
    let profile_id = active_profile_id.unwrap_or(MAIN_PROFILE_ID).to_owned();

    Ok(super::coding_tool_contract::tool_status_list_payload(
        super::coding_tool_contract::ToolStatusListContext {
            profile_id: Some(profile_id.as_str()),
            session_id: session_id_wire.as_str(),
            policy: coding_tool_policy_view(
                permission_state.selection,
                permission_state.approval_policy,
            ),
            available_model_tools: &tool_name_refs,
            disabled_model_tools: &disabled_tool_refs,
            deferred_model_tools: &deferred_name_refs,
            include_coding_tool_contract: true,
        },
    ))
}

fn mcp_status_list_result(session_id: &SessionKey, active_profile_id: Option<&str>) -> Value {
    let session_id_wire = session_id.to_string();
    let profile_id = active_profile_id.unwrap_or(MAIN_PROFILE_ID).to_owned();
    super::coding_tool_contract::mcp_status_list_payload(
        super::coding_tool_contract::McpStatusListContext {
            profile_id: Some(profile_id.as_str()),
            session_id: session_id_wire.as_str(),
            servers: &[],
        },
    )
}

fn runtime_policy_stamp_for_profile(
    state: &AppState,
    profile_id: &str,
    session_id: Option<&SessionKey>,
    profile: Option<&crate::profiles::UserProfile>,
) -> Value {
    let runtime = state.profiles.get(profile_id);
    let primary = profile.and_then(|profile| profile.config.primary_llm());
    // The stamp reports the model that will actually SERVE the next turn,
    // matching `resolve_session_profile_runtime`'s precedence:
    // - a profile pinned in startup-config `state.profiles` keeps serving
    //   that immutable runtime until restart (`profile/llm/select` only
    //   warns for these), so the boot snapshot is the truth even when the
    //   stored file has since changed;
    // - a store-backed (dynamic) profile re-bootstraps from the FILE after
    //   select evicts, so the file's primary is the truth — the old
    //   unconditional runtime-first order made every status/read stomp a
    //   freshly-applied selection back to the boot-time model.
    let (model, provider) = if let Some(runtime) = runtime {
        (
            Some(runtime.primary_model_id.clone()),
            Some(runtime.provider_name.clone()),
        )
    } else {
        (
            primary.and_then(|selection| selection.model_id.clone()),
            primary.and_then(|selection| selection.family_id.clone()),
        )
    };
    let permission_state = session_id
        .map(|session_id| effective_session_permission_state(state, session_id))
        .unwrap_or_default();
    let (approval_policy, sandbox_mode, permission_profile, filesystem_scope, network) =
        permission_selection_policy_fields(
            permission_state.selection,
            permission_state.approval_policy,
        );
    let workspace_root = session_id
        .and_then(|session_id| session_workspaces().get(profile_id, session_id))
        .map(|path| path.to_string_lossy().to_string());
    let mut stamp = json!({
        "runtime_mode": runtime_mode_for_state(state),
        "profile_id": profile_id,
        "workspace_root": workspace_root,
        "approval_policy": approval_policy,
        "sandbox_mode": sandbox_mode,
        "permission_profile": permission_profile,
        "filesystem_scope": filesystem_scope,
        "network": network,
        "model": model,
        "provider": provider,
        "tool_policy_id": "profile",
        "mcp_servers": [],
        "memory_scope": "profile-session",
        "qoe_policy": "profile",
        "queue_mode": "adaptive",
    });
    super::coding_tool_contract::apply_coding_runtime_policy_stamp_extensions(
        &mut stamp,
        super::coding_tool_contract::RuntimePolicyStampContext {
            policy: super::coding_tool_contract::ToolPolicyView {
                tool_policy_id: "profile",
                sandbox_mode,
                approval_policy,
            },
            mcp_servers: &[],
        },
    );
    stamp
}

fn profile_llm_list_result(
    state: &AppState,
    profile_id: &str,
    profile: Option<&crate::profiles::UserProfile>,
) -> Value {
    let primary = profile.and_then(|profile| {
        profile
            .config
            .primary_llm()
            .map(|selection| configured_provider_json(selection, &profile.config.env_vars, true))
    });
    let fallbacks = profile
        .and_then(|profile| profile.config.llm.as_ref().map(|llm| (profile, llm)))
        .map(|(profile, llm)| {
            llm.fallbacks
                .iter()
                .map(|selection| {
                    configured_provider_json(selection, &profile.config.env_vars, false)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "profile_id": profile_id,
        "primary": primary.clone(),
        "fallbacks": fallbacks.clone(),
        "llm": {
            "primary": primary,
            "fallbacks": fallbacks,
        },
        "runtime_policy_stamp": runtime_policy_stamp_for_profile(state, profile_id, None, profile),
    })
}

fn configured_model_status_json(provider: &Value, selected: bool) -> Value {
    json!({
        "model": provider.get("model_id").and_then(Value::as_str).unwrap_or("unknown"),
        "provider": provider.get("family_id").and_then(Value::as_str).unwrap_or("unknown"),
        "title": format!(
            "{} / {}",
            provider.get("family_id").and_then(Value::as_str).unwrap_or("unknown"),
            provider.get("model_id").and_then(Value::as_str).unwrap_or("unknown")
        ),
        "family": provider.get("family_id").cloned(),
        "route": provider.get("route_id").cloned(),
        "selected": selected,
        "available": true,
        "queue_mode": "adaptive",
        "qoe_policy": "profile",
    })
}

fn model_list_result(
    state: &AppState,
    session_id: SessionKey,
    profile_id: &str,
    profile: Option<&crate::profiles::UserProfile>,
) -> Value {
    // EVERY configured model rides the list — the primary as `selected`,
    // fallbacks as switchable alternatives (`profile/llm/select` promotes
    // one to primary). A single-primary profile keeps its one-entry shape.
    let listed = profile_llm_list_result(state, profile_id, profile);
    let primary = listed
        .get("primary")
        .cloned()
        .filter(|value| !value.is_null());
    let fallbacks = listed
        .get("fallbacks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut models = primary
        .into_iter()
        .map(|provider| configured_model_status_json(&provider, true))
        .collect::<Vec<_>>();
    models.extend(
        fallbacks
            .iter()
            .map(|provider| configured_model_status_json(provider, false)),
    );
    json!({ "session_id": session_id, "models": models })
}

fn raw_catalog_result(_state: &AppState, _profile_id: Option<&str>) -> Result<Value, RpcError> {
    // The onboarding catalog is the compiled-in canonical model_catalog.json
    // (the model-provisioning SSOT) projected through the provider REGISTRY for
    // family key-env metadata. providers.json is retired (now a derived,
    // web-only artifact).
    //
    // The runtime QoS catalogs (per-profile data-dir + `~/.octos` seed) are
    // deliberately NOT unioned here. Configured providers are a *subset* of the
    // catalog, so a live QoS catalog can only ever (a) re-introduce a model that
    // was deliberately curated out — it still lists whatever was once
    // configured — or (b) zero out a researched context window with a QoS entry
    // that never carried one. Neither is desirable: the canonical catalog is the
    // single source of truth for what is provisionable, and new models are added
    // by editing it (and shipping the updated file), which is the whole point of
    // an SSOT. Live QoS still drives *routing* via the adaptive chain; that path
    // is untouched.
    //
    // Families with no registry entry have no key-env/route metadata and can't
    // be onboarded, so they're skipped.
    let canonical: octos_llm::QosCatalog = serde_json::from_str(CANONICAL_MODEL_CATALOG)
        .map_err(|error| RpcError::internal_error(format!("canonical model catalog: {error}")))?;

    // `endpoints` (alternative provisioning routes — e.g. AutoDL/WiseModel) is
    // onboarding metadata that `ModelCatalogEntry` doesn't model, so read it
    // straight from the canonical JSON keyed by `provider`. This keeps the TUI
    // onboarding route-picker (Official + alternatives) that the retired
    // providers.json used to supply, without polluting the routing catalog type.
    let endpoints_by_provider: std::collections::HashMap<String, Value> =
        serde_json::from_str::<Value>(CANONICAL_MODEL_CATALOG)
            .ok()
            .and_then(|catalog| catalog.get("models").and_then(Value::as_array).cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|model| {
                let provider = model.get("provider")?.as_str()?.to_owned();
                let endpoints = model.get("endpoints")?.clone();
                Some((provider, endpoints))
            })
            .collect();

    let mut families = serde_json::Map::new();
    for entry in &canonical.models {
        let Some((family_id, model_id)) = entry.provider.split_once('/') else {
            continue;
        };
        // Canonicalize the family via the registry (handles aliases:
        // google->gemini, qwen->dashscope, glm->zhipu). A family with no
        // registry entry has no key-env/route and can't be onboarded.
        let Some(reg) = octos_llm::registry::lookup(family_id) else {
            continue;
        };
        let family = families
            .entry(reg.name.to_owned())
            .or_insert_with(|| json!({ "env": reg.api_key_env.unwrap_or(""), "models": [] }));
        let Some(models) = family.get_mut("models").and_then(Value::as_array_mut) else {
            continue;
        };
        let known = models
            .iter()
            .any(|model| model.get("id").and_then(Value::as_str) == Some(model_id));
        if !known {
            let mut model_json = json!({
                "id": model_id,
                "input": entry.cost_in,
                "output": entry.cost_out,
                "max_output": entry.max_output,
                "context_window": entry.context_window,
            });
            if let Some(endpoints) = endpoints_by_provider.get(&entry.provider) {
                model_json["endpoints"] = endpoints.clone();
            }
            models.push(model_json);
        }
    }
    Ok(json!({ "families": Value::Object(families) }))
}

/// Cumulative token usage for one session, for the `usage` field of
/// `session/status/read`.
///
/// This used to be a hardcoded `{}`, so every field of octoscode's
/// `SessionUsageStatus` decoded to `None` on every read — the whole usage
/// readout was dead, and `cached_input_tokens` in particular meant operators
/// had no way to tell whether prompt caching (the largest cost lever, on by
/// default) was working at all.
///
/// Sourced from the persistent usage ledger so the figures survive runtime
/// rebuilds and restarts, matching the REST endpoints in `api::usage`. Reads
/// are best-effort: a missing or unreadable ledger yields `{}` exactly as
/// before rather than failing the whole status read, which also keeps
/// deployments with no ledger configured working unchanged.
///
/// `session/status/read` is event-driven and deduped client-side
/// (`enqueue_session_status_probe`), not interval-polled, so opening the
/// ledger here costs roughly what the existing `/api/usage` handlers already
/// pay per request.
async fn session_usage_status(state: &Arc<AppState>, profile_id: &str, session_id: &str) -> Value {
    let Some(store) = state.profile_store.as_ref() else {
        return json!({});
    };
    let Ok(Some(profile)) = store.get(profile_id) else {
        return json!({});
    };
    let data_dir = store.resolve_data_dir(&profile);
    let ledger = match PersistentUsageLedger::open(&data_dir).await {
        Ok(ledger) => ledger,
        Err(error) => {
            debug!(
                data_dir = %data_dir.display(),
                error = %error,
                "usage ledger unavailable for session status; reporting empty usage"
            );
            return json!({});
        }
    };
    let totals = match ledger.session_totals(session_id).await {
        Ok(totals) => totals,
        Err(error) => {
            debug!(
                session = %session_id,
                error = %error,
                "failed to read session usage totals; reporting empty usage"
            );
            return json!({});
        }
    };
    usage_status_json(&totals)
}

/// Shape [`UsageTotals`] into the `usage` object octoscode's
/// `SessionUsageStatus` decodes. Split out from [`session_usage_status`] so
/// the field mapping is testable without standing up an `AppState`.
fn usage_status_json(totals: &UsageTotals) -> Value {
    // A session with no recorded runs reports `{}` rather than a row of
    // zeroes: octoscode renders each field only when present, and zeroes
    // would claim "0 tokens used" for a session whose usage simply has not
    // been written yet.
    if totals.run_count == 0 {
        return json!({});
    }
    let mut usage = json!({
        "input_tokens": totals.input_tokens,
        "output_tokens": totals.output_tokens,
        "cached_input_tokens": totals.cache_read_tokens,
    });
    // Only emit a cost when the ledger actually priced something. A session
    // whose model has no catalog pricing accumulates tokens but no spend, and
    // reporting a confident `$0.0000` there is worse than reporting nothing.
    if totals.estimated_cost_usd > 0.0 {
        usage["estimated_cost_micros_usd"] =
            json!((totals.estimated_cost_usd * 1_000_000.0).round() as u64);
    }
    usage
}

async fn raw_session_status_result(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    features: ConnectionUiFeatures,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileParams = parse_raw_params(request)?;
    let Some(session_id) = params.session_id.clone() else {
        return Err(RpcError::invalid_params("session_id is required"));
    };
    let profile_id = raw_profile_id(&params, connection_profile_id);
    let profile = state
        .profile_store
        .as_ref()
        .and_then(|store| store.get(&profile_id).ok().flatten());
    if profile.is_none() && !profile_is_known(state, &profile_id) {
        return Err(profile_unresolved_error(&profile_id));
    }
    let policy =
        runtime_policy_stamp_for_profile(state, &profile_id, Some(&session_id), profile.as_ref());
    let (context, context_state) = if features.context_lifecycle_available() {
        appui_context_status_snapshot_for_state(
            state,
            connection_profile_id,
            Some(&profile_id),
            &session_id,
        )
        .await
    } else {
        (None, None)
    };
    let (context, context_state) = context_snapshot_for_features(context, context_state, features);
    // Emit the `model` object only when the policy actually resolved a
    // model AND provider. Clients (octoscode) decode it into a struct whose
    // `model`/`provider` are non-optional strings, so
    // `{"model": null, "provider": null, "selected": true}` fails the whole
    // session/status/read decode and the composer footer degrades to a
    // placeholder. A missing key is handled fine by their
    // `Option<ModelStatus>` + `#[serde(default)]` — including in shipped
    // octoscode 0.1.5 binaries.
    let resolved_model = policy.get("model").filter(|value| !value.is_null());
    let resolved_provider = policy.get("provider").filter(|value| !value.is_null());
    let model = match (resolved_model, resolved_provider) {
        (Some(model), Some(provider)) => Some(json!({
            "model": model,
            "provider": provider,
            "selected": true
        })),
        _ => None,
    };
    let mut result = json!({
        "session_id": session_id,
        "profile_id": profile_id,
        "runtime_policy_stamp": policy,
        "context": context,
        "context_state": context_state,
        "permission_profile": policy.get("permission_profile").cloned().unwrap_or(Value::Null),
        "sandbox": policy.get("sandbox_mode").cloned().unwrap_or(Value::Null),
        "health": { "status": "ok" },
        "mcp_summary": { "connected": 0, "connecting": 0, "failed": 0, "disabled": 0 },
        "tool_summary": { "visible": 0, "enabled": 0, "denied": 0, "policy_id": "profile" },
        // The ledger keys sessions by the `SessionKey`'s string form (see
        // `SessionActor::record_usage_event`); match it exactly or every
        // lookup silently returns zero totals.
        "usage": session_usage_status(state, &profile_id, &session_id.to_string()).await,
        "cursor": { "healthy": true, "replay_supported": true },
        "capabilities": features.advertised_capabilities(state),
    });
    if let Some(model) = model {
        result["model"] = model;
    }
    Ok(result)
}

fn raw_profile_skill_profile_id(
    profile_id: Option<String>,
    connection_profile_id: Option<&str>,
) -> Result<String, RpcError> {
    let requested = nonempty(profile_id);
    if let Some(connection_profile_id) = connection_profile_id {
        if requested
            .as_deref()
            .is_some_and(|requested| requested != connection_profile_id)
        {
            return Err(RpcError::permission_denied(
                "profile_id is outside the authenticated profile",
            )
            .with_data(json!({
                "kind": "auth_scope_violation",
                "connection_profile_id": connection_profile_id,
                "requested_profile_id": requested,
            })));
        }
        return Ok(connection_profile_id.to_owned());
    }
    Ok(requested.unwrap_or_else(|| MAIN_PROFILE_ID.to_owned()))
}

fn raw_profile_skills_dir(
    state: &AppState,
    profile_id: &str,
) -> Result<std::path::PathBuf, RpcError> {
    let store = profile_store(state)?;
    crate::commands::skills::resolve_profile_skills_dir(store.as_ref(), profile_id).map_err(|err| {
        RpcError::invalid_params(format!("profile skills unavailable: {err}")).with_data(json!({
            "kind": "profile_skills_unavailable",
            "profile_id": profile_id,
        }))
    })
}

fn skill_entry_with_status(skill: crate::commands::skills::SkillEntry) -> Result<Value, RpcError> {
    let mut value = serde_json::to_value(skill)
        .map_err(|err| RpcError::internal_error(format!("failed to encode skill: {err}")))?;
    if let Some(object) = value.as_object_mut() {
        object.insert("installed".into(), Value::Bool(true));
        object.insert("status".into(), Value::String("installed".into()));
    }
    Ok(value)
}

fn raw_profile_skills_list(
    state: &AppState,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileSkillsListParams = parse_raw_params(request)?;
    let profile_id = raw_profile_skill_profile_id(params.profile_id, connection_profile_id)?;
    let skills_dir = raw_profile_skills_dir(state, &profile_id)?;
    let skills = crate::commands::skills::list_skills(&skills_dir)
        .map_err(|err| RpcError::internal_error(format!("failed to list skills: {err}")))?
        .into_iter()
        .map(skill_entry_with_status)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "profile_id": profile_id,
        "count": skills.len(),
        "skills": skills,
    }))
}

async fn raw_profile_skills_registry_search(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileSkillsRegistrySearchParams = parse_raw_params(request)?;
    let profile_id = raw_profile_skill_profile_id(params.profile_id, connection_profile_id)?;
    let skills_dir = raw_profile_skills_dir(state, &profile_id)?;
    let installed = crate::commands::skills::list_skills(&skills_dir)
        .map_err(|err| RpcError::internal_error(format!("failed to list skills: {err}")))?;
    let installed_names: HashSet<String> = installed.into_iter().map(|skill| skill.name).collect();
    let q = params.q;
    let packages = tokio::task::spawn_blocking(move || {
        crate::commands::skills::search_registry(q.as_deref(), None)
    })
    .await
    .map_err(|err| RpcError::internal_error(format!("registry search join error: {err}")))?
    .map_err(|err| RpcError::internal_error(format!("failed to search skill registry: {err}")))?;

    let packages = packages
        .into_iter()
        .map(|package| {
            let mut installed_skills = package
                .skills
                .iter()
                .filter(|skill| installed_names.contains(*skill))
                .cloned()
                .collect::<Vec<_>>();
            if installed_skills.is_empty() && installed_names.contains(&package.name) {
                installed_skills.push(package.name.clone());
            }
            let mut value = serde_json::to_value(package).map_err(|err| {
                RpcError::internal_error(format!("failed to encode registry package: {err}"))
            })?;
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "installed".into(),
                    Value::Bool(!installed_skills.is_empty()),
                );
                object.insert("installed_skills".into(), json!(installed_skills));
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>, RpcError>>()?;

    Ok(json!({
        "profile_id": profile_id,
        "packages": packages,
    }))
}

async fn raw_profile_skills_install(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileSkillsInstallParams = parse_raw_params(request)?;
    let profile_id = raw_profile_skill_profile_id(params.profile_id, connection_profile_id)?;
    // Keep disk mutation, plugin rebuild/publication, and session eviction in
    // one per-profile critical section so an older rebuild cannot publish last.
    let _mutation_guard = state.profile_skill_mutation_locks.lock(&profile_id).await;
    let skills_dir = raw_profile_skills_dir(state, &profile_id)?;
    let repo =
        nonempty(Some(params.repo)).ok_or_else(|| RpcError::invalid_params("repo is required"))?;
    let branch = nonempty(params.branch).unwrap_or_else(|| "main".into());
    let force = params.force;
    let result = tokio::task::spawn_blocking(move || {
        crate::commands::skills::install_skill(&skills_dir, &repo, force, &branch)
    })
    .await
    .map_err(|err| RpcError::internal_error(format!("skill install join error: {err}")))?
    .map_err(|err| RpcError::invalid_params(format!("failed to install skill: {err}")))?;
    if !result.installed.is_empty() {
        rebuild_profile_runtime_after_skill_mutation(state, &profile_id).await?;
    }
    Ok(json!({
        "profile_id": profile_id,
        "ok": true,
        "installed": result.installed,
        "skipped": result.skipped,
        "deps_installed": result.deps_installed,
    }))
}

async fn raw_profile_skills_remove(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileSkillsRemoveParams = parse_raw_params(request)?;
    let profile_id = raw_profile_skill_profile_id(params.profile_id, connection_profile_id)?;
    // See install: the guard spans the full mutation-to-publication sequence.
    let _mutation_guard = state.profile_skill_mutation_locks.lock(&profile_id).await;
    let skills_dir = raw_profile_skills_dir(state, &profile_id)?;
    let name =
        nonempty(Some(params.name)).ok_or_else(|| RpcError::invalid_params("name is required"))?;
    let removed = name.clone();
    tokio::task::spawn_blocking(move || crate::commands::skills::remove_skill(&skills_dir, &name))
        .await
        .map_err(|err| RpcError::internal_error(format!("skill remove join error: {err}")))?
        .map_err(|err| RpcError::invalid_params(format!("failed to remove skill: {err}")))?;
    rebuild_profile_runtime_after_skill_mutation(state, &profile_id).await?;
    Ok(json!({
        "profile_id": profile_id,
        "ok": true,
        "removed": removed,
        "message": format!("Removed skill: {removed}"),
    }))
}

/// `profile/llm/select` — promote one CONFIGURED model (the primary or any
/// fallback) to be the ACTIVE primary. The demoted primary becomes a fallback,
/// so switching back and forth never loses a configuration. Persisted to the
/// profile store, and the profile's cached runtimes are evicted so the very
/// next turn runs on the newly selected model — no restart.
async fn raw_profile_llm_select(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileLlmSelectParams = parse_raw_params(request)?;
    let profile_id = raw_scoped_llm_profile_id(
        params.profile_id.clone(),
        params.session_id.as_ref(),
        connection_profile_id,
    )?;
    let model_id = nonempty(params.model_id)
        .ok_or_else(|| RpcError::invalid_params("model_id is required"))?;
    let family_id = nonempty(params.family_id);
    let route_id = nonempty(params.route_id);

    let store = profile_store(state)?;
    let mut profile = store
        .get(&profile_id)
        .map_err(|err| RpcError::internal_error(format!("failed to read profile: {err}")))?
        .ok_or_else(|| RpcError::not_found("profile", profile_id.clone()))?;

    let mut llm = profile.config.llm.take().unwrap_or_default();
    let matches_family_model = |selection: &crate::profiles::LlmModelSelectionConfig| {
        selection.model_id.as_deref() == Some(model_id.as_str())
            && family_id
                .as_deref()
                .is_none_or(|family| selection.family_id.as_deref() == Some(family))
    };
    let matches_route = |selection: &crate::profiles::LlmModelSelectionConfig| {
        route_id.as_deref().is_none_or(|route| {
            selection
                .route
                .as_ref()
                .and_then(|selection_route| selection_route.route_id.as_deref())
                == Some(route)
        })
    };
    let matches_exact = |selection: &crate::profiles::LlmModelSelectionConfig| {
        matches_family_model(selection) && matches_route(selection)
    };
    // The route is a REAL discriminator: the same family/model can be
    // configured on two routes with different endpoints and credentials.
    // An exact route match always wins; the TUI's synthetic default
    // ("official" when a configured entry carries no route) may fall back
    // to a family/model match ONLY when that match is unambiguous.
    let route_is_synthetic_default = route_id.as_deref() == Some("official");
    let exact_primary = llm.primary.as_ref().is_some_and(matches_exact);
    let exact_fallback = llm.fallbacks.iter().position(matches_exact);
    let target = if exact_primary {
        None
    } else if let Some(index) = exact_fallback {
        Some(index)
    } else if route_is_synthetic_default {
        let primary_matches = llm.primary.as_ref().is_some_and(matches_family_model);
        let fallback_matches = llm
            .fallbacks
            .iter()
            .enumerate()
            .filter(|(_, selection)| matches_family_model(selection))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match (primary_matches, fallback_matches.as_slice()) {
            (true, []) => None,
            (false, [index]) => Some(*index),
            (false, []) => {
                profile.config.llm = Some(llm);
                return Err(RpcError::not_found(
                    "configured model",
                    format!(
                        "{model_id} (not configured on profile '{profile_id}' — save it \
                         first via profile/llm/upsert / the onboarding provider step)"
                    ),
                ));
            }
            _ => {
                profile.config.llm = Some(llm);
                return Err(RpcError::invalid_params(format!(
                    "model '{model_id}' is configured on multiple routes — pass the \
                     route_id to disambiguate"
                )));
            }
        }
    } else {
        profile.config.llm = Some(llm);
        return Err(RpcError::not_found(
            "configured model",
            format!(
                "{model_id} (not configured on profile '{profile_id}' — save it first \
                 via profile/llm/upsert / the onboarding provider step)"
            ),
        ));
    };
    let already_primary = target.is_none();
    // Never persist a selection the runtime cannot activate: the eviction
    // below would silently keep the CURRENT provider chain for live sessions
    // (ensure fails with a warn) while the next `session/open` fails
    // bootstrap outright — a keyless primary bricks the profile until the
    // file is hand-edited. Reject up front, naming the missing key. The
    // idempotent re-select of a keyless primary rejects too: that state
    // needs the key (or a different selection), not an "applied" echo.
    {
        let target_selection = match target {
            Some(index) => &llm.fallbacks[index],
            None => llm
                .primary
                .as_ref()
                .expect("already_primary implies a matched primary"),
        };
        if let Err(error) = llm_selection_activation_error(&profile, target_selection) {
            profile.config.llm = Some(llm);
            return Err(error);
        }
    }
    let mut applied = already_primary;
    if !already_primary {
        let Some(index) = target else {
            unreachable!("already_primary guarantees a fallback target index");
        };
        let promoted = llm.fallbacks.remove(index);
        if let Some(demoted) = llm.primary.take() {
            upsert_llm_fallback(&mut llm.fallbacks, demoted);
        }
        llm.primary = Some(promoted);
        applied = true;
    }
    profile.config.llm = Some(llm);

    // The switch must take effect on the NEXT turn, not the next restart:
    // cached SessionRuntimes embed the old provider chain and the dynamic
    // ProfileRuntime caches forever, so run the SAME post-commit transition
    // as upsert/delete (#2164) — evict both caches (generation-guarded) and
    // rebuild. `applied` stays persistence-only; the runtime disposition is
    // stamped onto the result separately.
    let transition = if !already_primary {
        profile.updated_at = Utc::now();
        store
            .save_with_merge(&mut profile)
            .map_err(|err| RpcError::internal_error(format!("failed to save profile: {err}")))?;
        Some(
            commit_profile_llm_runtime_transition(
                state,
                &profile_id,
                Some(profile.updated_at.to_rfc3339()),
            )
            .await,
        )
    } else {
        None
    };

    let session_id = params
        .session_id
        .unwrap_or_else(|| SessionKey::with_profile_topic(&profile_id, "local", "tui", "coding"));
    let refreshed = store.get(&profile_id).ok().flatten().unwrap_or(profile);
    let models = model_list_result(state, session_id.clone(), &profile_id, Some(&refreshed));
    let selected = models
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models
                .iter()
                .find(|model| model.get("selected") == Some(&Value::Bool(true)))
                .cloned()
        })
        .unwrap_or_else(|| json!({ "model": model_id, "selected": true }));
    let mut result = json!({
        "session_id": session_id,
        "selected": selected,
        "applied": applied,
        "runtime_policy_stamp": runtime_policy_stamp_for_profile(
            state,
            &profile_id,
            Some(&session_id),
            Some(&refreshed),
        ),
    });
    stamp_profile_llm_runtime_transition(
        &mut result,
        transition
            .as_ref()
            .unwrap_or(&ProfileLlmRuntimeTransition::unchanged()),
    );
    if state.profiles.contains_key(&profile_id) {
        // Startup-pinned runtime: the selection is saved but turns keep the
        // boot snapshot until restart (the stamp above says so too). Tell
        // the caller instead of letting the switch silently not take —
        // including for an idempotent re-select of the already-active
        // primary, which performs no transition of its own.
        result["restart_required"] = json!(true);
    }
    Ok(result)
}

/// Resolve the target profile for the profile-LLM raw methods, rejecting
/// cross-profile targets on authenticated (profile-scoped) connections —
/// profile A's credential must never rewire, probe, or spend against
/// profile B's configured providers.
fn raw_scoped_llm_profile_id(
    requested_profile_id: Option<String>,
    requested_session_id: Option<&SessionKey>,
    connection_profile_id: Option<&str>,
) -> Result<String, RpcError> {
    let requested = nonempty(requested_profile_id).or_else(|| {
        requested_session_id.and_then(|session_id| session_id.profile_id().map(ToOwned::to_owned))
    });
    if let Some(connection_profile_id) = connection_profile_id {
        if requested
            .as_deref()
            .is_some_and(|requested| requested != connection_profile_id)
        {
            return Err(RpcError::permission_denied(
                "profile_id is outside the authenticated profile",
            )
            .with_data(json!({
                "kind": "auth_scope_violation",
                "connection_profile_id": connection_profile_id,
                "requested_profile_id": requested,
            })));
        }
        return Ok(connection_profile_id.to_owned());
    }
    Ok(requested.unwrap_or_else(|| MAIN_PROFILE_ID.to_string()))
}

async fn raw_profile_llm_upsert(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileLlmUpsertParams = parse_llm_selection_params(request)?;
    let profile_id =
        raw_scoped_llm_profile_id(params.profile_id.clone(), None, connection_profile_id)?;
    let store = profile_store(state)?;
    let mut profile = store
        .get(&profile_id)
        .map_err(|err| RpcError::internal_error(format!("failed to read profile: {err}")))?
        .unwrap_or_else(|| default_profile(&profile_id));

    let family_id = nonempty(params.selection.family_id)
        .ok_or_else(|| RpcError::invalid_params("selection.family_id is required"))?;
    let model_id = nonempty(params.selection.model_id)
        .ok_or_else(|| RpcError::invalid_params("selection.model_id is required"))?;
    let route = crate::profiles::LlmRouteConfig {
        route_id: nonempty(params.selection.route.route_id),
        label: nonempty(params.selection.route.label),
        base_url: nonempty(params.selection.route.base_url),
        api_key_env: nonempty(params.selection.route.api_key_env)
            .or_else(|| dashboard_family_api_key_env(&family_id)),
        api_type: nonempty(params.selection.route.api_type).or_else(|| Some("openai".into())),
    };
    if let (Some(api_key_env), Some(api_key)) = (
        route.api_key_env.as_ref(),
        secret_from_value(params.api_key),
    ) {
        profile.config.env_vars.insert(api_key_env.clone(), api_key);
    }

    // #2166 typed inference schema: the upsert payload is the COMPLETE
    // inference configuration for the addressed selection — every optional
    // field is `absent ≡ null ≡ inherit` (a re-upsert without it CLEARS a
    // prior override), an explicit value is an override. Rejected/unknown
    // fields never reach this point (parse_llm_selection_params).
    let mut selection = crate::profiles::LlmModelSelectionConfig {
        family_id: Some(family_id),
        model_id: Some(model_id),
        route: Some(route),
        model_hints: params.selection.model_hints,
        context_window: params.selection.context_window,
        temperature: params.selection.temperature.map(|value| value as f32),
        top_p: params.selection.top_p.map(|value| value as f32),
        reasoning_effort: params.selection.reasoning_effort,
        ..Default::default()
    };
    // #2166: routing/QoS metadata that is deliberately OUTSIDE the AppUI
    // schema (`cost_per_m`, `strong` — owned by routing research / QoS) must
    // not be silently destroyed by an endpoint edit: carry the prior
    // same-address values forward instead of resetting them.
    let prior_same_address = profile.config.llm.as_ref().and_then(|llm| {
        llm.fallbacks
            .iter()
            .chain(llm.primary.iter())
            .find(|existing| same_llm_selection_address(existing, &selection))
    });
    if let Some(prior) = prior_same_address {
        selection.cost_per_m = prior.cost_per_m;
        selection.strong = prior.strong;
    }

    let mut llm = profile.config.llm.take().unwrap_or_default();
    if params.set_primary || llm.primary.is_none() {
        // set_primary is lossless (mirrors `profile/llm/select`): a replaced
        // primary is demoted into the fallback list, and the promoted
        // selection is de-duplicated out of it. The two decisions use
        // different comparisons (codex P2 ×2):
        // - demote by *address* (family/model/route_id — all the selector can
        //   discriminate): a same-address upsert is an endpoint edit and
        //   replaces outright, or the old endpoint would linger as a fallback
        //   row `profile/llm/select` can never promote;
        // - de-dup by full *identity* (address + base_url): endpoint-distinct
        //   fallbacks are real failover chain entries (`config_from_profile`
        //   threads every fallback's endpoint into the runtime), so only an
        //   exact duplicate of the new primary leaves the list.
        if let Some(old_primary) = llm.primary.take() {
            if !same_llm_selection_address(&old_primary, &selection) {
                upsert_llm_fallback(&mut llm.fallbacks, old_primary);
            }
        }
        llm.fallbacks
            .retain(|fallback| !same_llm_selection_identity(fallback, &selection));
        llm.primary = Some(selection);
    } else {
        upsert_llm_fallback(&mut llm.fallbacks, selection);
    }
    profile.config.llm = Some(llm);
    // Relocate keychain-backed secrets (e.g. a Vertex SA JSON supplied as the
    // route api_key) into the OS keychain before persisting, so this RPC can't
    // write a private key to plaintext profile config.
    crate::api::handlers::relocate_keychain_backed_secrets(
        &mut profile.config.env_vars,
        &profile_id,
    )
    .map_err(|msg| RpcError::invalid_params(msg))?;
    profile.updated_at = Utc::now();
    store
        .save_with_merge(&mut profile)
        .map_err(|err| RpcError::internal_error(format!("failed to save profile: {err}")))?;
    // #2164: a saved-but-still-cached provider chain used to serve the next
    // turn (ensure returned the existing dynamic or startup runtime
    // immediately). Run the shared post-commit transition instead: evict the
    // cached SessionRuntimes + ProfileRuntime (generation-guarded), then
    // rebuild or report restart_required / persisted_but_not_live.
    let transition = commit_profile_llm_runtime_transition(
        state,
        &profile_id,
        Some(profile.updated_at.to_rfc3339()),
    )
    .await;
    let mut result = profile_llm_mutation_result(state, &profile_id, Some(&profile), true);
    stamp_profile_llm_runtime_transition(&mut result, &transition);
    Ok(result)
}

/// `profile/llm/delete`: remove one configured model — primary or fallback —
/// addressed by family + model + route (the same address comparison the
/// upsert/select paths use). Deleting the primary promotes the first fallback
/// so the profile keeps a working model whenever one exists; deleting the
/// last model leaves `llm.primary` empty (recoverable via `/model` → Add).
/// A non-matching address returns the unchanged state with `applied: false`.
/// On a successful commit it runs the SAME post-commit transition as
/// select/upsert (#2164): evict the cached runtimes and rebuild — deleting
/// the active primary used to leave the old chain serving the next turn.
async fn raw_profile_llm_delete(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileLlmDeleteParams = parse_raw_params(request)?;
    let profile_id =
        raw_scoped_llm_profile_id(params.profile_id.clone(), None, connection_profile_id)?;
    let store = profile_store(state)?;
    let Some(mut profile) = store
        .get(&profile_id)
        .map_err(|err| RpcError::internal_error(format!("failed to read profile: {err}")))?
    else {
        let mut result = profile_llm_mutation_result(state, &profile_id, None, false);
        stamp_profile_llm_runtime_transition(
            &mut result,
            &ProfileLlmRuntimeTransition::unchanged(),
        );
        return Ok(result);
    };

    let family_id = nonempty(Some(params.family_id))
        .ok_or_else(|| RpcError::invalid_params("family_id is required"))?;
    let model_id = nonempty(Some(params.model_id))
        .ok_or_else(|| RpcError::invalid_params("model_id is required"))?;
    let route_id = nonempty(Some(params.route_id))
        .ok_or_else(|| RpcError::invalid_params("route_id is required"))?;
    let target = crate::profiles::LlmModelSelectionConfig {
        family_id: Some(family_id),
        model_id: Some(model_id),
        route: Some(crate::profiles::LlmRouteConfig {
            route_id: Some(route_id),
            ..Default::default()
        }),
        ..Default::default()
    };

    let Some(mut llm) = profile.config.llm.take() else {
        let mut result = profile_llm_mutation_result(state, &profile_id, Some(&profile), false);
        stamp_profile_llm_runtime_transition(
            &mut result,
            &ProfileLlmRuntimeTransition::unchanged(),
        );
        return Ok(result);
    };

    let applied = if llm
        .primary
        .as_ref()
        .is_some_and(|primary| same_llm_selection_address(primary, &target))
    {
        llm.primary = None;
        // Keep the profile on a working model whenever one exists: the first
        // fallback is the failover chain's next-in-line, so it inherits the
        // primary slot.
        if !llm.fallbacks.is_empty() {
            llm.primary = Some(llm.fallbacks.remove(0));
        }
        true
    } else {
        let before = llm.fallbacks.len();
        llm.fallbacks
            .retain(|fallback| !same_llm_selection_address(fallback, &target));
        llm.fallbacks.len() != before
    };
    profile.config.llm = Some(llm);

    if !applied {
        // #2164: a miss persists nothing and must NOT evict a healthy
        // runtime — report the unchanged disposition explicitly.
        let mut result = profile_llm_mutation_result(state, &profile_id, Some(&profile), false);
        stamp_profile_llm_runtime_transition(
            &mut result,
            &ProfileLlmRuntimeTransition::unchanged(),
        );
        return Ok(result);
    }

    profile.updated_at = Utc::now();
    store
        .save_with_merge(&mut profile)
        .map_err(|err| RpcError::internal_error(format!("failed to save profile: {err}")))?;
    // #2164: the shared post-commit transition — deletion used to return
    // `applied: true` while every cached runtime kept serving the deleted
    // model (or the pre-promotion primary) until a restart.
    let transition = commit_profile_llm_runtime_transition(
        state,
        &profile_id,
        Some(profile.updated_at.to_rfc3339()),
    )
    .await;
    let mut result = profile_llm_mutation_result(state, &profile_id, Some(&profile), true);
    stamp_profile_llm_runtime_transition(&mut result, &transition);
    Ok(result)
}

fn upsert_llm_fallback(
    fallbacks: &mut Vec<crate::profiles::LlmModelSelectionConfig>,
    selection: crate::profiles::LlmModelSelectionConfig,
) {
    if let Some(existing) = fallbacks
        .iter_mut()
        .find(|fallback| same_llm_selection_identity(fallback, &selection))
    {
        *existing = selection;
    } else {
        fallbacks.push(selection);
    }
}

/// Full selection *identity*: the [`same_llm_selection_address`] (which
/// normalizes a missing route_id to the synthetic `"official"`) plus the
/// endpoint. Two entries that agree on both resolve to the same provider —
/// defined in terms of the address comparison so the two predicates cannot
/// drift on route_id normalization (codex P2 round 3: a raw `Option`
/// comparison here kept a route_id-less fallback alongside an identical
/// `"official"` primary as a redundant retry-chain duplicate).
fn same_llm_selection_identity(
    left: &crate::profiles::LlmModelSelectionConfig,
    right: &crate::profiles::LlmModelSelectionConfig,
) -> bool {
    same_llm_selection_address(left, right)
        && left
            .route
            .as_ref()
            .and_then(|route| route.base_url.as_ref())
            == right
                .route
                .as_ref()
                .and_then(|route| route.base_url.as_ref())
}

/// Selection *address* as `profile/llm/select` can discriminate it: family +
/// model + route_id, with a missing route_id normalized to the synthetic
/// `"official"` the TUI sends for default routes. `base_url` is deliberately
/// excluded — the selector cannot address it, so two selections differing
/// only by endpoint are the same switchable row.
fn same_llm_selection_address(
    left: &crate::profiles::LlmModelSelectionConfig,
    right: &crate::profiles::LlmModelSelectionConfig,
) -> bool {
    fn route_address(selection: &crate::profiles::LlmModelSelectionConfig) -> &str {
        selection
            .route
            .as_ref()
            .and_then(|route| route.route_id.as_deref())
            .unwrap_or("official")
    }
    left.family_id == right.family_id
        && left.model_id == right.model_id
        && route_address(left) == route_address(right)
}

/// `Err` when the selection's provider requires an API key that resolves
/// nowhere (auth store, profile `env_vars`, keychain, process env — the same
/// chain the runtime bootstrap uses via [`crate::config::Config::
/// get_api_key_with_env`]). Families the registry marks keyless (or unknown
/// families) pass. The caller must not persist a selection this rejects.
fn llm_selection_activation_error(
    profile: &crate::profiles::UserProfile,
    selection: &crate::profiles::LlmModelSelectionConfig,
) -> Result<(), RpcError> {
    let model = selection.model_id.as_deref().unwrap_or("model");
    // Effective provider exactly as bootstrap resolves it: the explicit
    // family, else `detect_provider` on the model id (the same fallback
    // `ProfileRuntime::bootstrap` applies to `config_from_profile` output).
    let family = selection
        .family_id
        .as_deref()
        .filter(|family| !family.is_empty())
        .or_else(|| {
            selection
                .model_id
                .as_deref()
                .and_then(crate::config::detect_provider)
        });
    let Some(family) = family else {
        return Err(RpcError::invalid_params(format!(
            "cannot activate {model}: no provider family configured and none detectable \
             from the model id — re-save the selection with an explicit family first"
        ))
        .with_data(json!({ "kind": "llm_provider_unresolved" })));
    };
    // Key requirement mirrors `create_provider_with_api_type`: the
    // `anthropic`/`responses` protocol overrides always need a key (even on
    // registry-keyless families), `custom` always needs one, otherwise the
    // registry entry decides. A family the registry doesn't know cannot be
    // constructed at all — reject rather than persist a guaranteed
    // bootstrap failure (codex P1).
    let api_type = selection
        .route
        .as_ref()
        .and_then(|route| route.api_type.as_deref());
    // Mirror `create_provider_with_api_type`'s ORDER exactly: `custom`
    // short-circuits first, then the registry lookup gates everything —
    // including the protocol overrides, which the factory only reaches for
    // a registered entry. An unknown family with `api_type: anthropic` and
    // a key would otherwise validate here yet still fail the factory's
    // lookup at bootstrap (codex P1 round 2).
    let requires_key = if family == "custom" {
        true
    } else {
        let Some(entry) = octos_llm::registry::lookup(family) else {
            return Err(RpcError::invalid_params(format!(
                "cannot activate {family}/{model}: unknown provider family — valid: \
                 custom, {}",
                octos_llm::registry::all_names().join(", ")
            ))
            .with_data(json!({ "kind": "llm_provider_unresolved", "family": family })));
        };
        match api_type {
            Some("anthropic") | Some("responses") => true,
            _ => entry.requires_api_key,
        }
    };
    if !requires_key {
        return Ok(());
    }
    let env_override = selection
        .route
        .as_ref()
        .and_then(|route| route.api_key_env.as_deref());
    let config = crate::profiles::config_from_profile(profile);
    if config.get_api_key_with_env(family, env_override).is_ok() {
        return Ok(());
    }
    let var = env_override
        .map(String::from)
        .or_else(|| crate::config::Config::provider_default_env_var(family))
        .unwrap_or_else(|| format!("{}_API_KEY", family.to_uppercase()));
    Err(RpcError::invalid_params(format!(
        "cannot activate {family}/{model}: {var} is not set — add the key via the \
         onboarding key step (or `octos auth login`) before selecting it"
    ))
    .with_data(json!({ "kind": "llm_key_missing", "env": var })))
}

async fn raw_profile_llm_test(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileLlmUpsertParams = parse_llm_selection_params(request)?;
    let profile_id =
        raw_scoped_llm_profile_id(params.profile_id.clone(), None, connection_profile_id)?;
    let profile = state
        .profile_store
        .as_ref()
        .and_then(|store| store.get(&profile_id).ok().flatten());

    let family_id = nonempty(params.selection.family_id)
        .ok_or_else(|| RpcError::invalid_params("selection.family_id is required"))?;
    let model_id = nonempty(params.selection.model_id)
        .ok_or_else(|| RpcError::invalid_params("selection.model_id is required"))?;
    let route = crate::profiles::LlmRouteConfig {
        route_id: nonempty(params.selection.route.route_id),
        label: nonempty(params.selection.route.label),
        base_url: nonempty(params.selection.route.base_url),
        api_key_env: nonempty(params.selection.route.api_key_env)
            .or_else(|| dashboard_family_api_key_env(&family_id)),
        api_type: nonempty(params.selection.route.api_type).or_else(|| Some("openai".into())),
    };

    let resolved_key = secret_from_value(params.api_key).or_else(|| {
        route.api_key_env.as_ref().and_then(|env_name| {
            // Resolve a keychain marker to the real secret (e.g. a scoped Vertex
            // SA JSON); plain values pass through unchanged.
            let raw = profile.as_ref()?.config.env_vars.get(env_name)?;
            crate::auth::keychain::resolve_value(env_name, raw)
        })
    });
    let api_key = match resolved_key {
        Some(key) => key,
        // Keyless local families (local/ollama/vllm) construct without a
        // key — dead-ending them on "No API key provided" blocked the
        // keyless onboarding test entirely (red-team pass).
        None if octos_llm::registry::is_keyless(&family_id) => String::new(),
        None => {
            return Ok(profile_llm_test_result(
                state,
                &profile_id,
                profile.as_ref(),
                false,
                "Provider connection failed",
                Some("No API key provided".into()),
            ));
        }
    };

    let provider =
        match build_test_llm_provider(&family_id, &model_id, route.base_url.clone(), &api_key) {
            Ok(provider) => provider,
            Err(error) => {
                return Ok(profile_llm_test_result(
                    state,
                    &profile_id,
                    profile.as_ref(),
                    false,
                    "Provider connection failed",
                    Some(error),
                ));
            }
        };

    let messages = vec![Message {
        role: MessageRole::User,
        content: "Say OK".into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: Utc::now(),
    }];
    let canonical_family = octos_llm::registry::lookup(&family_id)
        .map(|entry| entry.name)
        .unwrap_or(family_id.as_str());
    let max_tokens = if canonical_family == "gemini" || canonical_family == "vertex" {
        128
    } else {
        16
    };
    let config = octos_llm::ChatConfig {
        max_tokens: Some(max_tokens),
        temperature: Some(0.0),
        ..Default::default()
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        provider.chat(&messages, &[], &config),
    )
    .await
    {
        Ok(Ok(_response)) => {
            info!(
                provider = %family_id,
                model = %model_id,
                "AppUI profile/llm/test succeeded"
            );
            Ok(profile_llm_test_result(
                state,
                &profile_id,
                profile.as_ref(),
                true,
                "Provider connection verified",
                None,
            ))
        }
        Ok(Err(error)) => {
            warn!(
                provider = %family_id,
                model = %model_id,
                error = %error,
                "AppUI profile/llm/test failed"
            );
            Ok(profile_llm_test_result(
                state,
                &profile_id,
                profile.as_ref(),
                false,
                "Provider connection failed",
                Some(format!("{error:#}")),
            ))
        }
        Err(_) => {
            warn!(
                provider = %family_id,
                model = %model_id,
                "AppUI profile/llm/test timed out"
            );
            Ok(profile_llm_test_result(
                state,
                &profile_id,
                profile.as_ref(),
                false,
                "Provider connection timed out",
                Some("Request timed out after 30 seconds".into()),
            ))
        }
    }
}

fn build_test_llm_provider(
    family_id: &str,
    model_id: &str,
    base_url: Option<String>,
    api_key: &str,
) -> Result<Arc<dyn octos_llm::LlmProvider>, String> {
    let params = octos_llm::registry::CreateParams {
        // Empty means "keyless family" — let the factory apply its own
        // fallback instead of sending an empty Bearer token.
        api_key: (!api_key.is_empty()).then(|| api_key.to_owned()),
        model: Some(model_id.to_owned()),
        base_url: base_url.clone(),
        model_hints: None,
        llm_timeout_secs: None,
        llm_connect_timeout_secs: None,
    };
    match octos_llm::registry::lookup(family_id) {
        Some(entry) => (entry.create)(params).map_err(|error| format!("provider error: {error:#}")),
        None => {
            let url = base_url.as_deref().unwrap_or("https://api.openai.com/v1");
            Ok(Arc::new(
                octos_llm::openai::OpenAIProvider::new(api_key, model_id)
                    .with_base_url(url)
                    .with_provider_label(family_id),
            ))
        }
    }
}

fn dashboard_family_api_key_env(family_id: &str) -> Option<String> {
    // Key-env now comes from the provider registry (single source of truth),
    // not the retired dashboard providers.json. `lookup` resolves aliases too.
    octos_llm::registry::lookup(family_id)
        .and_then(|entry| entry.api_key_env)
        .map(str::to_owned)
        .and_then(|env| nonempty(Some(env)))
}

fn profile_llm_mutation_result(
    state: &AppState,
    profile_id: &str,
    profile: Option<&crate::profiles::UserProfile>,
    applied: bool,
) -> Value {
    let mut result = profile_llm_list_result(state, profile_id, profile);
    if let Value::Object(ref mut object) = result {
        object.insert("applied".into(), Value::Bool(applied));
    }
    result
}

/// Post-commit runtime disposition of a Profile LLM mutation (#2164):
/// `applied` only ever means PERSISTED — this is the separate live-runtime
/// truth, uniform across `profile/llm/select`, `upsert`, and `delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileRuntimeDisposition {
    /// Dynamic profile: caches evicted and the ProfileRuntime rebuilt from the
    /// committed file — the next turn serves the new provider chain.
    Reloaded,
    /// Dynamic profile: caches evicted, no runtime bootstrapped right now
    /// (disabled profile, or no model left after deleting the last one) — the
    /// next turn deterministically re-derives, reporting typed
    /// runtime-unavailable truth when the selection is gone.
    Deferred,
    /// Startup-pinned profile: the boot snapshot keeps serving until restart.
    RestartRequired,
    /// Dynamic profile: caches evicted but the rebuild FAILED — the next turn
    /// retries the bootstrap (retry/restart recovers). Reported explicitly on
    /// the wire, never collapsed into a warn-only server log.
    PersistedButNotLive,
    /// Nothing was persisted (applied:false): no runtime transition happened,
    /// a healthy runtime stays healthy.
    Unchanged,
}

impl ProfileRuntimeDisposition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reloaded => "reloaded",
            Self::Deferred => "deferred",
            Self::RestartRequired => "restart_required",
            Self::PersistedButNotLive => "persisted_but_not_live",
            Self::Unchanged => "unchanged",
        }
    }
}

/// The runtime transition a committed Profile LLM mutation performed, stamped
/// onto the wire result next to `applied` (#2164).
#[derive(Debug, Clone)]
struct ProfileLlmRuntimeTransition {
    disposition: ProfileRuntimeDisposition,
    /// Persisted profile revision (`updated_at`) the runtime was — or was
    /// demonstrably not — synced to.
    config_revision: Option<String>,
    /// Rebuild failure detail for `persisted_but_not_live`.
    error: Option<String>,
}

impl ProfileLlmRuntimeTransition {
    fn unchanged() -> Self {
        Self {
            disposition: ProfileRuntimeDisposition::Unchanged,
            config_revision: None,
            error: None,
        }
    }
}

/// The ONE post-commit transition shared by `profile/llm/select`, `upsert`,
/// and `delete` (#2164): evict every cached SessionRuntime for the profile,
/// bump the dynamic-runtime generation and drop the cached ProfileRuntime,
/// then either rebuild it (dynamic profile) or report `restart_required`
/// (startup-pinned boot snapshot). A caller whose persistence FAILED must not
/// reach this — a healthy runtime stays healthy.
async fn commit_profile_llm_runtime_transition(
    state: &AppState,
    profile_id: &str,
    config_revision: Option<String>,
) -> ProfileLlmRuntimeTransition {
    let startup_pinned = state.profiles.contains_key(profile_id);
    // Evict FIRST, generation before removal: the session cache bumps its own
    // guard inside `invalidate_profile`, and the dynamic map's guard must be
    // bumped before the drop so an in-flight bootstrap that read the
    // pre-commit file is refused at insert time.
    state.session_cache.invalidate_profile(profile_id).await;
    if let Some(key) = dynamic_profile_runtime_key(state, profile_id) {
        bump_profile_runtime_generation(&key);
        dynamic_profile_runtimes()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
    }

    if startup_pinned {
        // Startup-config profiles live in an immutable map — the saved
        // mutation persists but cannot rebuild without a restart.
        tracing::warn!(
            profile_id = %profile_id,
            "profile LLM mutation saved, but this startup-config profile's runtime \
             rebuilds on restart only"
        );
        return ProfileLlmRuntimeTransition {
            disposition: ProfileRuntimeDisposition::RestartRequired,
            config_revision,
            error: None,
        };
    }

    match ensure_session_profile_runtime(state, Some(profile_id)).await {
        Ok(Some(_runtime)) => ProfileLlmRuntimeTransition {
            disposition: ProfileRuntimeDisposition::Reloaded,
            config_revision,
            error: None,
        },
        Ok(None) => ProfileLlmRuntimeTransition {
            disposition: ProfileRuntimeDisposition::Deferred,
            config_revision,
            error: None,
        },
        Err(error) => {
            tracing::warn!(
                profile_id = %profile_id,
                error = %error.message,
                "profile LLM mutation saved but runtime rebuild failed; the next turn \
                 retries the bootstrap"
            );
            ProfileLlmRuntimeTransition {
                disposition: ProfileRuntimeDisposition::PersistedButNotLive,
                config_revision,
                error: Some(error.message),
            }
        }
    }
}

/// Stamp the uniform post-commit runtime truth (#2164) onto a profile-LLM
/// mutation result. `applied` stays persistence-only; `restart_required` is
/// always present so clients never parse presence as the signal.
fn stamp_profile_llm_runtime_transition(
    result: &mut Value,
    transition: &ProfileLlmRuntimeTransition,
) {
    if let Value::Object(object) = result {
        object.insert(
            "runtime_disposition".into(),
            json!(transition.disposition.as_str()),
        );
        if let Some(config_revision) = &transition.config_revision {
            object.insert("config_revision".into(), json!(config_revision));
        }
        object.insert(
            "restart_required".into(),
            json!(matches!(
                transition.disposition,
                ProfileRuntimeDisposition::RestartRequired
            )),
        );
        if transition.disposition != ProfileRuntimeDisposition::Unchanged {
            object.insert("effective_from".into(), json!("next_turn"));
        }
        if let Some(error) = &transition.error {
            object.insert("runtime_error".into(), json!(error));
        }
    }
}

fn sub_provider_json(sp: &crate::config::SubProviderConfig) -> Value {
    json!({
        "key": sp.key,
        "provider": sp.provider,
        "model": sp.model,
        "api_key_env": sp.api_key_env,
        "base_url": sp.base_url,
        "description": sp.description,
        "default_context_window": sp.default_context_window,
        "max_output_tokens": sp.max_output_tokens,
        "api_type": sp.api_type,
    })
}

fn sub_providers_list_result(
    state: &AppState,
    profile_id: &str,
    profile: Option<&crate::profiles::UserProfile>,
) -> Value {
    let sub_providers: Vec<Value> = profile
        .map(|p| {
            p.config
                .sub_providers
                .iter()
                .map(sub_provider_json)
                .collect()
        })
        .unwrap_or_default();
    json!({
        "profile_id": profile_id,
        "sub_providers": sub_providers,
        "runtime_policy_stamp": runtime_policy_stamp_for_profile(state, profile_id, None, profile),
    })
}

fn sub_providers_mutation_result(
    state: &AppState,
    profile_id: &str,
    profile: Option<&crate::profiles::UserProfile>,
    applied: bool,
) -> Value {
    let mut result = sub_providers_list_result(state, profile_id, profile);
    if let Value::Object(ref mut object) = result {
        // `applied` means PERSISTED, not live: the isolated research router is
        // built at ProfileRuntime bootstrap, so a persisted change only takes
        // effect on the next serve restart. Surface `restart_required` so the
        // client never presents a persisted change as already-live.
        object.insert("applied".into(), Value::Bool(applied));
        object.insert("restart_required".into(), Value::Bool(applied));
    }
    result
}

/// `profile/sub_providers/list`: read the profile's named provider lanes.
fn raw_profile_sub_providers_list(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileParams = parse_raw_params(request)?;
    let profile_id = raw_scoped_llm_profile_id(
        params.profile_id.clone(),
        params.session_id.as_ref(),
        connection_profile_id,
    )?;
    let store = profile_store(state)?;
    let profile = store
        .get(&profile_id)
        .map_err(|err| RpcError::internal_error(format!("failed to read profile: {err}")))?;
    Ok(sub_providers_list_result(
        state,
        &profile_id,
        profile.as_ref(),
    ))
}

#[derive(Debug, Deserialize)]
struct RawSnapshotListParams {
    session_id: SessionKey,
}

#[derive(Debug, Deserialize)]
struct RawSnapshotRestoreParams {
    session_id: SessionKey,
    snapshot_id: String,
}

/// Resolve the snapshot context for a session: the profile's opt-in +
/// keep-last policy (from the bootstrapped [`ProfileRuntime`]), the profile
/// data dir the snapshot git-dirs live under, and the session's workspace
/// root. Listing works even when the opt-in is currently off (snapshots from
/// an earlier enabled run remain restorable).
fn snapshot_context_for_session(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Result<(bool, Option<octos_agent::SnapshotManager>), RpcError> {
    let profile_id = raw_scoped_llm_profile_id(None, Some(session_id), connection_profile_id)?;
    let runtime = state.profiles.get(&profile_id);
    let enabled = runtime
        .and_then(|rt| rt.snapshots.as_ref())
        .is_some_and(|cfg| cfg.enabled);
    let keep_last = runtime
        .and_then(|rt| rt.snapshots.as_ref())
        .map(|cfg| cfg.keep_last)
        .unwrap_or(octos_agent::DEFAULT_SNAPSHOT_KEEP_LAST);
    let Some(workspace_root) = session_workspace_root_for_state(state, session_id) else {
        return Ok((enabled, None));
    };
    let manager = runtime.and_then(|rt| {
        octos_agent::SnapshotManager::new(rt.data_dir.join("snapshots"), workspace_root, keep_last)
    });
    Ok((enabled, manager))
}

/// Render one wire row per retained snapshot, newest first.
fn snapshot_rows(manager: &octos_agent::SnapshotManager) -> Vec<Value> {
    manager
        .list_snapshots()
        .unwrap_or_default()
        .into_iter()
        .map(|info| {
            json!({
                "id": info.id.as_str(),
                "label": info.label,
                "timestamp_unix": info.timestamp_unix,
            })
        })
        .collect()
}

/// `snapshot/list` (#1768): the session workspace's undo points.
fn raw_snapshot_list(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawSnapshotListParams = parse_raw_params(request)?;
    let (enabled, manager) =
        snapshot_context_for_session(state, connection_profile_id, &params.session_id)?;
    let (available, snapshots) = match manager.as_ref() {
        Some(manager) => (true, snapshot_rows(manager)),
        None => (false, Vec::new()),
    };
    Ok(json!({
        "session_id": params.session_id,
        "enabled": enabled,
        "available": available,
        "snapshots": snapshots,
    }))
}

/// `snapshot/restore` (#1768): roll the session workspace back to a
/// snapshot. Refused while the session has an in-flight turn — restoring
/// under a running agent would yank files out from under its tools. The
/// restore itself takes a pre-restore snapshot first, so it is undoable.
async fn raw_snapshot_restore(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawSnapshotRestoreParams = parse_raw_params(request)?;
    let active = active_turn_sessions(&active_turns_registry()).await;
    if active.contains(&params.session_id) {
        return Err(RpcError::invalid_params(
            "cannot restore while a turn is running in this session — interrupt it first",
        ));
    }
    let (_enabled, manager) =
        snapshot_context_for_session(state, connection_profile_id, &params.session_id)?;
    let Some(manager) = manager else {
        return Err(RpcError::invalid_params(
            "snapshots are not available for this session (no workspace, or git is not installed)",
        ));
    };
    let snapshot_id = octos_agent::SnapshotId::new(params.snapshot_id.clone());
    // Git work is blocking; keep it off the reactor.
    let manager = std::sync::Arc::new(manager);
    let restore_manager = std::sync::Arc::clone(&manager);
    tokio::task::spawn_blocking(move || restore_manager.restore(&snapshot_id))
        .await
        .map_err(|err| RpcError::internal_error(format!("restore task failed: {err}")))?
        .map_err(|err| RpcError::invalid_params(format!("restore failed: {err}")))?;
    Ok(json!({
        "session_id": params.session_id,
        "restored": params.snapshot_id,
        "snapshots": snapshot_rows(&manager),
    }))
}

/// Serializes profile `sub_providers` read-modify-write across concurrent
/// upsert/remove RPCs so two clients adding different lanes don't clobber each
/// other (a bare get→mutate→save is last-writer-wins because `save_with_merge`
/// does NOT union `sub_providers`). A single global lock is proportionate —
/// these mutations are rare + interactive — and it does not serialize with the
/// profile/llm path, which mutates disjoint fields.
static SUB_PROVIDERS_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `profile/sub_providers/upsert`: add or replace one lane by `key`.
async fn raw_profile_sub_providers_upsert(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileSubProvidersUpsertParams = parse_raw_params(request)?;
    let profile_id =
        raw_scoped_llm_profile_id(params.profile_id.clone(), None, connection_profile_id)?;

    let key = nonempty(Some(params.sub_provider.key))
        .ok_or_else(|| RpcError::invalid_params("sub_provider.key is required"))?;
    let provider = nonempty(params.sub_provider.provider)
        .ok_or_else(|| RpcError::invalid_params("sub_provider.provider is required"))?;
    let api_key_env = nonempty(params.sub_provider.api_key_env);
    let pasted_secret = secret_from_value(params.api_key);
    // A pasted secret with no target env var used to be silently DROPPED, after
    // which the lane fell back to ambient/primary credentials — potentially a
    // DIFFERENT provider's key. Reject instead of losing the secret / sending the
    // wrong key.
    if pasted_secret.is_some() && api_key_env.is_none() {
        return Err(RpcError::invalid_params(
            "sub_provider.api_key_env is required when an api_key is supplied",
        ));
    }
    let entry = crate::config::SubProviderConfig {
        key: key.clone(),
        provider,
        model: nonempty(params.sub_provider.model),
        api_key_env: api_key_env.clone(),
        base_url: nonempty(params.sub_provider.base_url),
        description: nonempty(params.sub_provider.description),
        default_context_window: params.sub_provider.default_context_window,
        max_output_tokens: params.sub_provider.max_output_tokens,
        api_type: nonempty(params.sub_provider.api_type),
    };

    // Serialize the read-modify-write so a concurrent lane add isn't lost.
    let _guard = SUB_PROVIDERS_MUTATION_LOCK.lock().await;
    let store = profile_store(state)?;
    let mut profile = store
        .get(&profile_id)
        .map_err(|err| RpcError::internal_error(format!("failed to read profile: {err}")))?
        .unwrap_or_else(|| default_profile(&profile_id));

    // The pasted key is stored under its `api_key_env` in the profile's env_vars.
    // NOTE: this is the SAME secret-at-rest model as the profile's primary/
    // fallback provider keys (env_vars in the 0600 profile JSON); JSON credentials
    // (e.g. a Vertex service-account) are relocated to the OS keychain below,
    // ordinary API keys are not. Keychaining every key would be a whole-profile
    // secret-model change (shared with the profile/llm path), not lane-scoped.
    if let (Some(env), Some(secret)) = (api_key_env.as_ref(), pasted_secret) {
        profile.config.env_vars.insert(env.clone(), secret);
    }

    if let Some(existing) = profile
        .config
        .sub_providers
        .iter_mut()
        .find(|sp| sp.key == key)
    {
        *existing = entry;
    } else {
        profile.config.sub_providers.push(entry);
    }

    crate::api::handlers::relocate_keychain_backed_secrets(
        &mut profile.config.env_vars,
        &profile_id,
    )
    .map_err(|msg| RpcError::invalid_params(msg))?;
    profile.updated_at = Utc::now();
    store
        .save_with_merge(&mut profile)
        .map_err(|err| RpcError::internal_error(format!("failed to save profile: {err}")))?;
    // A LIVE runtime is NOT rebuilt here — the isolated research router is built
    // at ProfileRuntime bootstrap, so a pinned solo profile needs a restart to
    // pick up the change. The result carries `restart_required` so the client
    // never claims the change is live. This only bootstraps when none exists yet.
    if let Err(error) = ensure_session_profile_runtime(state, Some(&profile_id)).await {
        tracing::warn!(
            profile_id = %profile_id,
            error = %error.message,
            "profile/sub_providers/upsert saved but runtime bootstrap is not ready yet",
        );
    }
    Ok(sub_providers_mutation_result(
        state,
        &profile_id,
        Some(&profile),
        true,
    ))
}

/// `profile/sub_providers/remove`: drop the lane with this `key`.
async fn raw_profile_sub_providers_remove(
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile_id: Option<&str>,
) -> Result<Value, RpcError> {
    let params: RawProfileSubProvidersRemoveParams = parse_raw_params(request)?;
    let profile_id =
        raw_scoped_llm_profile_id(params.profile_id.clone(), None, connection_profile_id)?;
    let key =
        nonempty(Some(params.key)).ok_or_else(|| RpcError::invalid_params("key is required"))?;
    // Serialize against concurrent lane mutations (see upsert / the lock doc).
    let _guard = SUB_PROVIDERS_MUTATION_LOCK.lock().await;
    let store = profile_store(state)?;
    let Some(mut profile) = store
        .get(&profile_id)
        .map_err(|err| RpcError::internal_error(format!("failed to read profile: {err}")))?
    else {
        return Ok(sub_providers_mutation_result(
            state,
            &profile_id,
            None,
            false,
        ));
    };
    let before = profile.config.sub_providers.len();
    profile.config.sub_providers.retain(|sp| sp.key != key);
    let applied = profile.config.sub_providers.len() != before;
    if !applied {
        return Ok(sub_providers_mutation_result(
            state,
            &profile_id,
            Some(&profile),
            false,
        ));
    }
    profile.updated_at = Utc::now();
    store
        .save_with_merge(&mut profile)
        .map_err(|err| RpcError::internal_error(format!("failed to save profile: {err}")))?;
    if let Err(error) = ensure_session_profile_runtime(state, Some(&profile_id)).await {
        tracing::warn!(
            profile_id = %profile_id,
            error = %error.message,
            "profile/sub_providers/remove saved but runtime bootstrap is not ready yet",
        );
    }
    Ok(sub_providers_mutation_result(
        state,
        &profile_id,
        Some(&profile),
        true,
    ))
}

fn profile_llm_test_result(
    state: &AppState,
    profile_id: &str,
    profile: Option<&crate::profiles::UserProfile>,
    applied: bool,
    message: &str,
    error: Option<String>,
) -> Value {
    let mut result = profile_llm_mutation_result(state, profile_id, profile, applied);
    if let Value::Object(ref mut object) = result {
        object.insert("message".into(), Value::String(message.to_owned()));
        if let Some(error) = error {
            object.insert("error".into(), Value::String(error));
        }
    }
    result
}

async fn handle_raw_appui_rpc(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    _contracts: &Arc<UiProtocolContractStores>,
    _active_turns: &SharedActiveTurns,
    _connection_turns: &SharedConnectionTurns,
    features: ConnectionUiFeatures,
    connection_profile_id: Option<&str>,
    id: String,
    request: &RpcRequest<Value>,
) -> bool {
    // SINGLE SOURCE OF TRUTH: `raw_method_is_dispatched` decides the raw surface.
    // Anything it rejects is NOT dispatched here (it flows to the typed surface),
    // which lets the match default below be `unreachable!` and guarantees the
    // session-ingress deny gate (which consults the same function) can never
    // drift from what this handler actually serves.
    if !raw_method_is_dispatched(request.method.as_str(), features.stdio_transport) {
        return false;
    }
    let result = match request.method.as_str() {
        APPUI_METHOD_CONFIG_CAPABILITIES_LIST => {
            Ok(json!({ "capabilities": features.advertised_capabilities(state) }))
        }
        APPUI_METHOD_SESSION_STATUS_READ => {
            raw_session_status_result(state, request, features, connection_profile_id).await
        }
        APPUI_METHOD_SESSION_COMPACT => {
            handle_session_compact(ws, state, ledger, features, connection_profile_id, request)
                .await
        }
        APPUI_METHOD_SESSION_COMPACT_MODE_SET => handle_session_compact_mode_set(state, request),
        APPUI_METHOD_PROFILE_LLM_CATALOG => raw_catalog_result(state, connection_profile_id),
        APPUI_METHOD_PROFILE_LLM_LIST => {
            let params: RawProfileParams = match parse_raw_params(request) {
                Ok(params) => params,
                Err(error) => {
                    let _ = send_rpc_error(ws, Some(id), error);
                    return true;
                }
            };
            let profile_id = raw_profile_id(&params, connection_profile_id);
            let profile = state
                .profile_store
                .as_ref()
                .and_then(|store| store.get(&profile_id).ok().flatten());
            if let Some(session_id) = params.session_id {
                Ok(model_list_result(
                    state,
                    session_id,
                    &profile_id,
                    profile.as_ref(),
                ))
            } else {
                Ok(profile_llm_list_result(
                    state,
                    &profile_id,
                    profile.as_ref(),
                ))
            }
        }
        APPUI_METHOD_PROFILE_LLM_UPSERT => {
            raw_profile_llm_upsert(state, request, connection_profile_id).await
        }
        APPUI_METHOD_PROFILE_LLM_TEST => {
            raw_profile_llm_test(state, request, connection_profile_id).await
        }
        APPUI_METHOD_PROFILE_LLM_SELECT => {
            raw_profile_llm_select(state, request, connection_profile_id).await
        }
        APPUI_METHOD_PROFILE_LLM_DELETE => {
            raw_profile_llm_delete(state, request, connection_profile_id).await
        }
        APPUI_METHOD_PROFILE_SUB_PROVIDERS_LIST => {
            raw_profile_sub_providers_list(state, request, connection_profile_id)
        }
        APPUI_METHOD_PROFILE_SUB_PROVIDERS_UPSERT => {
            raw_profile_sub_providers_upsert(state, request, connection_profile_id).await
        }
        APPUI_METHOD_PROFILE_SUB_PROVIDERS_REMOVE => {
            raw_profile_sub_providers_remove(state, request, connection_profile_id).await
        }
        APPUI_METHOD_SNAPSHOT_LIST => raw_snapshot_list(state, request, connection_profile_id),
        APPUI_METHOD_SNAPSHOT_RESTORE => {
            raw_snapshot_restore(state, request, connection_profile_id).await
        }
        APPUI_METHOD_PROFILE_SKILLS_LIST => {
            raw_profile_skills_list(state, request, connection_profile_id)
        }
        APPUI_METHOD_PROFILE_SKILLS_REGISTRY_SEARCH => {
            raw_profile_skills_registry_search(state, request, connection_profile_id).await
        }
        APPUI_METHOD_PROFILE_SKILLS_INSTALL => {
            raw_profile_skills_install(state, request, connection_profile_id).await
        }
        APPUI_METHOD_PROFILE_SKILLS_REMOVE => {
            raw_profile_skills_remove(state, request, connection_profile_id).await
        }
        APPUI_METHOD_AUTH_STATUS => Ok(json!({
            "bootstrap_mode": false,
            "email_login_enabled": true,
            "admin_token_login_enabled": false,
            "allow_self_registration": true,
            "authenticated": true,
            "email_otp": true,
            "token_login": false,
            "profile_id": connection_profile_id.unwrap_or(MAIN_PROFILE_ID),
            "scoped_profile": {
                "id": connection_profile_id.unwrap_or(MAIN_PROFILE_ID),
                "name": connection_profile_id.unwrap_or(MAIN_PROFILE_ID),
                "email_login_enabled": true
            }
        })),
        APPUI_METHOD_AUTH_ME if features.stdio_transport => {
            Err(auth_unavailable_error(APPUI_METHOD_AUTH_ME))
        }
        APPUI_METHOD_AUTH_ME => Ok(json!({
            "email": "unknown account",
            "profile_id": connection_profile_id.unwrap_or(MAIN_PROFILE_ID)
        })),
        APPUI_METHOD_AUTH_SEND_CODE => {
            Ok(json!({ "ok": true, "message": "OTP code accepted in local AppUI mode" }))
        }
        APPUI_METHOD_AUTH_VERIFY => Ok(json!({
            "ok": true,
            "token": "local-appui-token",
            "user": { "email": "unknown account" },
            "message": "verified"
        })),
        APPUI_METHOD_AUTH_LOGOUT if features.stdio_transport => {
            Err(auth_unavailable_error(APPUI_METHOD_AUTH_LOGOUT))
        }
        APPUI_METHOD_AUTH_LOGOUT => Ok(json!({ "ok": true })),
        APPUI_METHOD_MCP_STATUS_LIST | APPUI_METHOD_TOOL_STATUS_LIST => {
            let params: RawProfileParams = match parse_raw_params(request) {
                Ok(params) => params,
                Err(error) => {
                    let _ = send_rpc_error(ws, Some(id), error);
                    return true;
                }
            };
            let Some(session_id) = params.session_id else {
                let _ = send_rpc_error(
                    ws,
                    Some(id),
                    RpcError::invalid_params("session_id is required"),
                );
                return true;
            };
            let active_profile_id = match validate_session_scope(
                &session_id,
                params.profile_id.as_deref(),
                connection_profile_id,
            ) {
                Ok(active_profile_id) => active_profile_id,
                Err(error) => {
                    let _ = send_rpc_error(ws, Some(id), error);
                    return true;
                }
            };
            if request.method == APPUI_METHOD_MCP_STATUS_LIST {
                Ok(mcp_status_list_result(
                    &session_id,
                    active_profile_id.as_deref(),
                ))
            } else {
                match tool_status_list_result(state, &session_id, active_profile_id.as_deref())
                    .await
                {
                    Ok(result) => Ok(result),
                    Err(error) => {
                        let _ = send_rpc_error(ws, Some(id), error);
                        return true;
                    }
                }
            }
        }
        APPUI_METHOD_ONBOARDING_WORKSPACE_PROBE => {
            let params: OnboardingWorkspaceProbeParams = match parse_raw_params(request) {
                Ok(params) => params,
                Err(error) => {
                    let _ = send_rpc_error(ws, Some(id), error);
                    return true;
                }
            };
            onboarding_workspace_probe_result(state, &params.path)
        }
        // Unreachable: the `raw_method_is_dispatched` guard at the top of this
        // function admits exactly the methods handled above. A method reaching
        // here means the guard and this match have drifted — a bug, and (for
        // session-ingress creds) a scope-bypass, so fail loudly rather than
        // silently routing it to the typed surface.
        other => unreachable!("raw_method_is_dispatched admitted an unhandled method: {other}"),
    };

    match result {
        Ok(result) => {
            let _ = send_rpc_result(ws, id, result);
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
    true
}

fn handle_client_hello_rpc(
    ws: &WsConnection,
    state: &Arc<AppState>,
    id: String,
    request: &RpcRequest<Value>,
    features: &mut ConnectionUiFeatures,
) -> bool {
    if request.method != APPUI_METHOD_CLIENT_HELLO {
        return false;
    }
    let params: RawClientHelloParams = match parse_optional_raw_params(request) {
        Ok(params) => params,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return true;
        }
    };
    if !params.supported_features.is_empty() {
        *features = ConnectionUiFeatures::from_requested_feature_tokens(
            params.supported_features.iter().map(String::as_str),
            features.stdio_transport,
        );
    }
    // Codex #1336 round-2 BLOCKER 1: keep the per-connection feature
    // snapshot on WsConnection in lockstep with the local `features`
    // copy. The send-helpers (`send_notification_durable` /
    // `send_notification_ephemeral` / `send_notification_lifecycle` /
    // `send_ledger_event_durable`) consult `ws.snapshot_live_features()`
    // to apply the same `live_event_passes_capability_filter` the
    // broadcast forwarder uses — without this sync a connection that
    // negotiated `projection.envelope.v1` mid-session would still
    // receive legacy frames on direct sends.
    ws.update_live_features(*features);
    let transport = if features.stdio_transport {
        "stdio"
    } else {
        "websocket"
    };
    let capabilities = features.advertised_capabilities(state);
    let _ = send_rpc_result(
        ws,
        id,
        json!({
            "type": "server_hello",
            "transport": transport,
            "client_transport": params.transport,
            "client": params.client,
            "capabilities": capabilities,
        }),
    );
    true
}

fn route_rpc_command(
    request: RpcRequest<Value>,
    features: ConnectionUiFeatures,
) -> Result<UiCommand, RpcError> {
    let method_str = request.method.as_str();
    if !ui_protocol_server_supported_methods().contains(&method_str) {
        return Err(RpcError::method_not_supported(method_str));
    }
    // UPCR-2026-009 / -010 / -011: when the method is gated behind a
    // feature flag and the connection did not negotiate that flag, reject
    // with `method_not_supported` BEFORE attempting to deserialize the
    // params (spec contract: "we don't know about this method at all").
    if features.header_present {
        let gated = match method_str {
            octos_core::ui_protocol::methods::SESSION_HYDRATE => Some(features.session_hydrate),
            octos_core::ui_protocol::methods::THREAD_GRAPH_GET => Some(features.thread_graph),
            octos_core::ui_protocol::methods::TURN_STATE_GET => Some(features.turn_state_get),
            // UPCR-2026-023: `user_question/respond` is strict opt-in. A client
            // that did not negotiate `user_question.v1` never received a
            // `user_question/requested`, so it has nothing to answer.
            octos_core::ui_protocol::methods::USER_QUESTION_RESPOND => {
                Some(features.user_question_v1)
            }
            _ => None,
        };
        if let Some(false) = gated {
            return Err(RpcError::method_not_supported(method_str));
        }
    }
    UiCommand::from_rpc_request(request)
}

/// SINGLE SOURCE OF TRUTH for which methods `handle_raw_appui_rpc` dispatches —
/// the raw path that runs BEFORE `route_rpc_command`.
///
/// `handle_raw_appui_rpc` early-returns `false` for any method this rejects and
/// treats its own match default as `unreachable!`, so the dispatcher and this
/// guard CANNOT drift: a new raw arm that isn't also added here is dead (its
/// feature won't dispatch), while adding it here without a matching arm panics
/// the `unreachable!`.
fn raw_method_is_dispatched(method: &str, _stdio_transport: bool) -> bool {
    if method == APPUI_METHOD_REVIEW_START || method == APPUI_METHOD_TURN_STEER {
        return true;
    }
    if matches!(
        method,
        APPUI_METHOD_CONFIG_CAPABILITIES_LIST
            | APPUI_METHOD_SESSION_STATUS_READ
            | APPUI_METHOD_PROFILE_LLM_CATALOG
            | APPUI_METHOD_PROFILE_LLM_LIST
            | APPUI_METHOD_PROFILE_LLM_UPSERT
            | APPUI_METHOD_PROFILE_LLM_TEST
            | APPUI_METHOD_PROFILE_LLM_SELECT
            | APPUI_METHOD_PROFILE_LLM_DELETE
            | APPUI_METHOD_PROFILE_SUB_PROVIDERS_LIST
            | APPUI_METHOD_PROFILE_SUB_PROVIDERS_UPSERT
            | APPUI_METHOD_PROFILE_SUB_PROVIDERS_REMOVE
            | APPUI_METHOD_SNAPSHOT_LIST
            | APPUI_METHOD_SNAPSHOT_RESTORE
            | APPUI_METHOD_PROFILE_SKILLS_LIST
            | APPUI_METHOD_PROFILE_SKILLS_REGISTRY_SEARCH
            | APPUI_METHOD_PROFILE_SKILLS_INSTALL
            | APPUI_METHOD_PROFILE_SKILLS_REMOVE
            | APPUI_METHOD_AUTH_STATUS
            | APPUI_METHOD_AUTH_ME
            | APPUI_METHOD_AUTH_SEND_CODE
            | APPUI_METHOD_AUTH_VERIFY
            | APPUI_METHOD_AUTH_LOGOUT
            | APPUI_METHOD_MCP_STATUS_LIST
            | APPUI_METHOD_TOOL_STATUS_LIST
            | APPUI_METHOD_ONBOARDING_WORKSPACE_PROBE
            | APPUI_METHOD_SESSION_COMPACT
            | APPUI_METHOD_SESSION_COMPACT_MODE_SET
    ) {
        return true;
    }
    false
}

fn ui_protocol_server_supported_methods() -> Vec<&'static str> {
    let mut methods = octos_core::ui_protocol::UI_PROTOCOL_FIRST_SERVER_METHODS.to_vec();
    methods.extend(APPUI_EXTRA_METHODS.iter().copied());
    methods
}

fn validate_session_scope(
    session_id: &SessionKey,
    requested_profile_id: Option<&str>,
    connection_profile_id: Option<&str>,
) -> Result<Option<String>, RpcError> {
    if requested_profile_id.is_some_and(str::is_empty) {
        return Err(RpcError::invalid_params("profile_id cannot be empty"));
    }

    if let Some(connection_profile_id) = connection_profile_id {
        validate_authenticated_session_scope(
            session_id,
            requested_profile_id,
            connection_profile_id,
        )?;
        return Ok(Some(connection_profile_id.to_string()));
    }

    if let (Some(requested_profile_id), Some(session_profile_id)) =
        (requested_profile_id, session_id.profile_id())
    {
        if requested_profile_id != session_profile_id {
            return Err(profile_mismatch_error(
                "profile_id does not match session_id profile",
                session_profile_id,
                Some(requested_profile_id),
            ));
        }
    }

    Ok(requested_profile_id
        .or_else(|| session_id.profile_id())
        .map(ToOwned::to_owned))
}

fn validate_authenticated_session_scope(
    session_id: &SessionKey,
    requested_profile_id: Option<&str>,
    connection_profile_id: &str,
) -> Result<(), RpcError> {
    if requested_profile_id.is_some_and(|profile_id| profile_id != connection_profile_id) {
        return Err(authenticated_scope_mismatch_error(
            "profile_id is outside the authenticated profile",
            connection_profile_id,
            requested_profile_id,
        ));
    }

    match session_id.profile_id() {
        Some(session_profile_id) if session_profile_id == connection_profile_id => Ok(()),
        Some(session_profile_id) => Err(authenticated_scope_mismatch_error(
            "session_id is outside the authenticated profile",
            connection_profile_id,
            Some(session_profile_id),
        )),
        // SPA convention: a fresh session uses a raw `web-N` id with no
        // profile prefix. Accept it under profile auth — the auth layer
        // is the gate, the session_id is just an opaque per-tab handle.
        // PR #857 originally landed this. PR #926 inadvertently added
        // `auth_scope_violation: true` here, which the new 1008
        // close-on-auth-scope-violation path then weaponized — every
        // OTP-authenticated browser session was 1008-closed on
        // `session/open` and the SPA fell into a reconnect storm.
        // Mini5 OTP-flow probe (2026-05-13) reproduced this.
        None => Ok(()),
    }
}

fn profile_mismatch_error(
    message: &'static str,
    expected_profile_id: &str,
    actual_profile_id: Option<&str>,
) -> RpcError {
    RpcError::invalid_params(message).with_data(json!({
        "expected_profile_id": expected_profile_id,
        "actual_profile_id": actual_profile_id,
    }))
}

/// Profile-mismatch variant emitted only when the connection IS authenticated
/// (i.e. carries an `AuthIdentity::User`) and the requested scope falls
/// outside that user's profile. The `auth_scope_violation` data tag drives
/// the WS close-code 1008 emit per the SPA bridge contract (Web PR #114) —
/// see `is_auth_scope_violation` for the consumer side.
fn authenticated_scope_mismatch_error(
    message: &'static str,
    expected_profile_id: &str,
    actual_profile_id: Option<&str>,
) -> RpcError {
    RpcError::invalid_params(message).with_data(json!({
        "expected_profile_id": expected_profile_id,
        "actual_profile_id": actual_profile_id,
        "auth_scope_violation": true,
    }))
}

/// True iff the error was produced by `validate_authenticated_session_scope`,
/// i.e. the request was rejected because the authenticated identity's profile
/// id does not match the requested session scope. Auth-related rejections
/// trigger a WS close-code 1008 emit so the SPA `crew:auth_expired` listener
/// fires; non-auth `invalid_params` errors (e.g. malformed input) do not.
fn is_auth_scope_violation(error: &RpcError) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("auth_scope_violation"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

// The session/open pipeline needs the approval and question stores, live
// forwarders, negotiated features, and the caller's profile scope as separate
// pieces — a flat dependency list, not a missing struct.
#[allow(clippy::too_many_arguments)]
async fn handle_session_open(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    approvals: &PendingApprovalStore,
    questions: &PendingQuestionStore,
    live_forwarders: &SharedLiveForwarders,
    connection_profile_id: Option<&str>,
    // #2067 — the profile this connection is FROZEN to for its whole lifetime,
    // or `None` when the transport has no such pin. Used only to decide whether
    // the session scope may serve as a DELIVERY filter; it never changes what
    // the session resolves to. See `ledger_event_matches_profile_scope`.
    pinned_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    mut params: SessionOpenParams,
) -> bool {
    normalize_session_open_params_topic(&mut params);
    let topic_scope = params.topic.clone();

    // Subscribe to the live ledger broadcast BEFORE the replay query so any
    // event that lands while we're still computing replay/opened sits in the
    // broadcast buffer and gets emitted by the forwarder once we hand it off
    // (filtered to seq > replay snapshot head to avoid duplicating replay).
    // Issue #760: without this, late background-task artifacts (bg_research
    // result, mofa podcast, bg_research output, TTS audio) reach the ledger
    // but never push to the live WS.
    let session_id_for_subscribe = params.session_id.clone();
    let live_rx = ledger.subscribe(&session_id_for_subscribe);

    // #2065 — retire the PREVIOUS forwarder for this session BEFORE
    // `open_session_result` computes the replay baseline (order: subscribe
    // new receiver → retire old lane → baseline → replay → response →
    // pump). With the old order the previous pump stayed live through the
    // new replay window and could deliver a post-baseline event
    // CONCURRENTLY with the replay that also carries it. abort+join fully
    // retires the lane:
    // every await point in the forwarder (recv, the offloaded send's
    // capacity park) is cancellable, and an enqueue is an atomic
    // `try_send` — there is no detached in-flight segment for the join to
    // miss (see `send_durable_offloaded`).
    //
    // Delivery semantics across the handover are AT-LEAST-ONCE, not
    // exactly-once (#2065, tracked): a durable frame the
    // old lane already delivered may be re-delivered by this open's replay
    // (the client's `after` cursor lags what was enqueued), and the client
    // merges durable frames by id/cursor — the normal reconnect-replay
    // overlap every reopen already has. What the retire-before-baseline
    // order guarantees is the absence of CONCURRENT old-pump/new-replay
    // delivery, not global dedupe.
    let previous_forwarder = live_forwarders
        .lock()
        .await
        .remove(&session_id_for_subscribe);
    if let Some(previous) = previous_forwarder {
        previous.abort();
        let _ = previous.await;
    }

    let outcome = match open_session_result(
        state,
        ledger,
        approvals,
        questions,
        ws.connection_id,
        connection_profile_id,
        pinned_profile_id,
        features,
        params,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            // Drop the receiver, then opportunistically reclaim the
            // broadcast sender slot if no other connection is subscribed
            // (codex MUST-FIX-3: failure paths previously leaked one
            // sender per failed open).
            drop(live_rx);
            ledger.prune_subscriber_if_idle(&session_id_for_subscribe);
            send_scope_error(ws, id, error);
            return false;
        }
    };
    // #2067 (H2) — this session's resolved profile, captured for the live
    // forwarder installed at the end of this function. It is deliberately a
    // per-forwarder value and NOT connection-wide state: a connection keeps one
    // forwarder PER SESSION, and an unscoped connection may open a second
    // session under a different profile. Sharing one mutable cell across
    // forwarders let the second open silently retarget the first session's
    // pump, starving it of every profile-carrying frame.
    let live_profile_scope = outcome.profile_scope.clone();

    let result = match serde_json::to_value(outcome.result) {
        Ok(result) => result,
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!(
                    "failed to serialize session/open result: {error}"
                )),
            );
            return false;
        }
    };
    // session/open reply is the lifecycle frame that the client blocks on;
    // if it fails the connection is doomed for this command.
    if send_rpc_result(ws, id, result).is_err() {
        return false;
    }
    // Replay frames are durable: drops surface as `protocol/replay_lossy`
    // and the client can refetch via REST.
    //
    // Capability filtering is shared between live and reconnect replay. Canonical
    // v2 envelopes bypass obsolete feature gates; historical source records still
    // use their relevant compatibility gates.
    //
    // We silently skip filtered events rather than emitting
    // `protocol/replay_lossy`. The client never asked for these events,
    // so dropping them is not lossy from their perspective.
    //
    // Reusing the helper keeps replay and live capability behavior in lockstep.
    //
    // No profile-scope arm here: `open_session_result` already retained this
    // exact vector against this exact scope (`outcome.profile_scope`), so a
    // second filter could never fire — and reading it as an independent gate
    // would overstate the delivery path's defences.
    for event in outcome.replay {
        let projected = features
            .projection_envelope_v2
            .then(|| project_v2_ledger_event(ledger, &event.event, &event.cursor))
            .flatten();
        let event_for_wire = context_event_for_features(projected.unwrap_or(event.event), features);
        if !live_event_passes_capability_filter(&event_for_wire, features) {
            continue;
        }
        let _ = send_ledger_event_durable(ws, ledger, event_for_wire);
    }
    for event in outcome.pending_approvals {
        let _ = send_ledger_event_durable(
            ws,
            ledger,
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(event)),
        );
    }
    // UPCR-2026-023: replay still-pending structured user-questions, gated by
    // `user_question.v1`. A client that did not negotiate the feature never
    // received a `user_question/requested` it could answer, so it must not
    // get one on reconnect either — run each through the same per-connection
    // capability filter the live broadcast uses so replay and live stay in
    // lockstep with the other replay gates above).
    for event in outcome.pending_questions {
        let ledger_event =
            UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionRequested(event));
        if !live_event_passes_capability_filter(&ledger_event, features) {
            continue;
        }
        let _ = send_ledger_event_durable(ws, ledger, ledger_event);
    }
    // Baseline = head_seq captured atomically with replay (codex MUST-FIX-1).
    // Using opened_event.cursor.seq instead would silently filter out any
    // event that happened to land between replay and the session/open
    // append, exactly the gap codex flagged.
    let baseline_seq = outcome.replay_baseline_seq;
    let session_id = match &outcome.opened_event.event {
        UiProtocolLedgerEvent::Notification(UiNotification::SessionOpened(opened)) => {
            opened.session_id.clone()
        }
        _ => session_id_for_subscribe,
    };

    // C8 / GAP A: replay the supervisor's CURRENT task snapshot to this
    // freshly-opened / reconnecting connection as `task/updated` events. A
    // TUI starts with an empty `session.tasks` and only applies incremental
    // updates, so without this it cannot see tasks that were already running
    // when it connected (it would have to wait for the next live transition,
    // and a stable long-running task may never produce one). Each task is
    // routed through the SAME mapping live updates use
    // (`background_task_to_progress_json` -> `map_progress_json`), so the wire
    // shape is identical to a live `task/updated`.
    //
    // These are sent EPHEMERAL (direct to this connection, NOT appended to the
    // ledger): the snapshot is catch-up state for one client, not a new event
    // in session history. Any historical durable `task/updated` events were
    // already shipped by the replay loop above; the client merges by `task_id`
    // (last-write-wins), so a duplicate is harmless and the latest state wins.
    if let Some(store) = state.task_query_store.as_ref() {
        for (task, _data_dir) in store.raw_tasks_for_session(&session_id.0) {
            if let Some(notification) = replay_task_updated_notification(&session_id, &task) {
                let _ = send_notification_ephemeral(ws, ledger, notification);
            }
        }
    }

    let ledger_for_forwarder = ledger.clone();
    let opened_event_for_wire =
        context_event_for_features(outcome.opened_event.event, ws.snapshot_live_features());
    let _ = send_ledger_event_durable(ws, ledger, opened_event_for_wire);

    // Hand the broadcast receiver to the per-session live pump. The
    // previous forwarder was already retired BEFORE the replay baseline
    // was computed (W6.1, top of this function); the in-spawn retire is an
    // idempotent second line of defense for direct callers.
    spawn_live_forwarder(
        ws.clone(),
        ledger_for_forwarder,
        session_id,
        baseline_seq,
        ws.connection_id,
        features,
        topic_scope,
        live_profile_scope,
        live_rx,
        live_forwarders.clone(),
    )
    .await;
    true
}

fn normalize_session_open_params_topic(params: &mut SessionOpenParams) {
    let topic = normalized_topic(params.topic.as_deref())
        .or_else(|| params.session_id.topic().map(ToOwned::to_owned));
    params.topic = topic.clone();
    if let Some(topic) = topic.as_deref() {
        params.session_id = session_key_with_optional_topic(&params.session_id, Some(topic));
    }
}

fn normalized_topic(topic: Option<&str>) -> Option<String> {
    topic
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
        .map(ToOwned::to_owned)
}

fn ledger_event_matches_topic_scope(
    event: &UiProtocolLedgerEvent,
    topic_scope: Option<&str>,
) -> bool {
    // Topic scoping consults `event.topic()`, which now reads the
    // explicit `topic` field carried on every variant whose emit site
    // can drop the topic suffix from `session_id` (TurnStarted,
    // MessageDelta, ToolStarted/Progress/Completed, Approval*, Task*,
    // TurnCompleted/Error/SpawnComplete, projection envelopes,
    // FileAttached). #1329 closes the P0-A class
    // routing drop: emitters populate `topic` from the upstream
    // SessionKey BEFORE any `base_key()` strip, so a topic-scoped
    // subscriber routes the event correctly even when `session_id`
    // was rebuilt from `base_key()` (the gap that previously
    // dropped `file/attached` in slides soak round-13 and was
    // patched with a single-variant exemption — the exemption is
    // no longer needed now that every vulnerable variant carries
    // explicit topic).
    let event_topic = event
        .topic()
        .map(str::trim)
        .filter(|topic| !topic.is_empty());
    match topic_scope.map(str::trim).filter(|topic| !topic.is_empty()) {
        Some(topic) => event_topic == Some(topic),
        None => event_topic.is_none(),
    }
}

/// #2067 — a durable event that names a profile must reach ONLY connections
/// resolved to that profile. This filter runs at both delivery boundaries
/// (the `replay.retain` in `open_session_result` and the live forwarder
/// pump), so a variant it does not recognise
/// leaks across tenants on every shared/unprofiled wire session key — which is
/// exactly what `session/goal/updated` and `session/goal/cleared` did — and
/// what the `loop/*` and `monitor/*` frames beside them did, since most of them
/// are appended DURABLY by the same `record_autonomy_rpc_evidence` ->
/// `send_notification_durable` dispatch and all of them carry tenant text
/// (goal objective, loop prompt, monitor argv/name).
///
/// `MonitorFired` and `BackgroundActivity` are the exceptions to "stamped by
/// `resolve_autonomy_profile_id`": both are emitted off the continuation drain
/// from a stored record, and that record's profile is the TURN's
/// `ProfileRuntime` id whenever the monitor or fleet was created by a model
/// tool. Filtering them is safe only because
/// [`connection_filterable_profile_scope`] refuses to filter on a scope that
/// can disagree with the turn's profile.
///
/// `profile_id` is the connection's RESOLVED scope and is always a concrete
/// string — [`WsConnection`] seeds [`MAIN_PROFILE_ID`] and `session/open`
/// overwrites it with `open_session_result`'s
/// `active_profile_id.unwrap_or(MAIN_PROFILE_ID)`. `_main` is therefore a real
/// scope on both sides of the wire, NOT a wildcard: the emitters normalize the
/// same way (`resolve_autonomy_profile_id` returns `_main` for an unprofiled
/// deployment), and treating an unscoped connection as "authorized for any
/// profile" is deployment-dependent — see the KNOWN LIMITATION on the
/// interactive goal-charge binding.
///
/// NOT filtered, deliberately: `AgentUpdated`, `PeerStaged` and `PeerClosed`.
/// All three are durable and all three leak, but they are stamped from the
/// TURN's `ProfileRuntime` rather than from `validate_session_scope`, and those
/// two resolutions diverge when an unscoped connection carries a routed profile
/// (`connection_profile_id.or(routed_profile_id)` — `validate_session_scope`
/// never consults the routed id). Filtering them before that divergence is
/// closed would starve exactly the reconnect-replay path they exist for. See
/// issue #2081. `LoopCompleted` / `MonitorExpired` have no producer at all
/// (issue #2080).
///
/// Every arm below prefers the event's top-level stamp and falls back to the
/// profile on the record it carries. The fallback is not cosmetic: the
/// `loop/pause`, `loop/resume` and `loop/delete` results omit the top-level
/// `profile_id` entirely (see `AgentOrchestrator::control_loop`) and only the
/// nested `loop` record names the owner. A fallback can only ever NARROW an
/// event's audience, never widen it.
/// #2067 — the profile scope a connection may be FILTERED against, or `None`
/// when it has none and every delivery must pass.
///
/// Filtering is sound only when the scope a forwarder captured at
/// `session/open` provably equals the profile that session's TURNS run under,
/// for the whole life of the forwarder. That matters because a turn's
/// `ProfileRuntime` hard-binds its profile onto every record the model's tools
/// create (`runtime/profile.rs` registers `MonitorCreateTool::new(profile.id)`,
/// and the tool persists that id), and the durable frames those records emit
/// carry it. Filter against a scope the turn does not share and the connection
/// is starved of its OWN data — fatally so for `background/activity`, whose
/// sink appends over a detached connection and therefore has no delivery path
/// except replay and live fan-out.
///
/// The turn resolves
/// `session.profile_id().or(connection.or(routed.or(session_open)))`, and both
/// `routed` and `session_open` are per-CONNECTION mutable state: `session_open`
/// is a single cell that every successful open overwrites, so opening a second
/// bare session under another profile retargets the turns of the FIRST one.
/// A per-session captured scope can never track that.
///
/// The one binding that cannot drift is an authenticated connection profile:
/// it is frozen at WS upgrade, and `validate_authenticated_session_scope`
/// forces every session opened on that connection to it or rejects the open
/// outright. So the whole connection has exactly one profile, every forwarder
/// captures it, and `connection.or(..)` short-circuits onto it for every turn.
/// That is the only case this filter may act on, and it is exactly the case
/// #2067 reports: a tenant is an authenticated user.
///
/// A routed profile cannot break this even when it names a DIFFERENT profile —
/// which it legitimately can, since an admin-role user is authorized for every
/// profile and a parent user for its owned subaccounts, and both still present
/// an authenticated `connection_profile_id`. `connection.or(routed)` never
/// reaches `routed` while the connection profile is set, and a session key
/// belonging to another profile is rejected before any turn starts.
///
/// Callers pass `None` for a transport whose binding is NOT frozen — stdio
/// rebinds `connection_profile_id_owned` after every successful open, so it
/// drifts exactly as `session_open` does and must not be filtered. Declining to
/// filter costs no tenant isolation: an unscoped WS connection is an
/// admin/operator authorized for every profile (or auth is disabled and every
/// route is deliberately open), and stdio is a single local user.
fn ledger_event_matches_profile_scope(
    event: &UiProtocolLedgerEvent,
    profile_id: Option<&str>,
) -> bool {
    // `None` = this connection's scope is not a tenant boundary (see
    // `connection_filterable_profile_scope`); deliver everything.
    let Some(profile_id) = profile_id else {
        return true;
    };
    let UiProtocolLedgerEvent::Notification(notification) = event else {
        return true;
    };
    match notification {
        // `session/open` is appended for BROADCAST — the emit site tags it with
        // the opening connection id specifically so OTHER connections observe
        // it — and it carries `workspace_root`, the context snapshot and pane
        // snapshots, i.e. another tenant's paths and buffers on a shared key.
        // Filtering it cannot starve its own opener: that connection's
        // forwarder skips the broadcast copy via `from_connection`, and its own
        // frame arrives through the UNFILTERED `send_ledger_event_durable`
        // direct send in `handle_session_open`. The scopes also agree by
        // construction — `ledger_profile_id` IS `active_profile_id
        // .unwrap_or(MAIN_PROFILE_ID)`, which is exactly this normalization.
        UiNotification::SessionOpened(opened) => {
            optional_profile_scope_matches(opened.active_profile_id.as_deref(), profile_id)
        }
        // `background/activity` is raw OBSERVED text — a monitor's stdout, a
        // fleet summary — and the ledger is its entire delivery mechanism: the
        // sink appends it over a detached connection whose writer is drained to
        // nowhere, so every real client receives it through replay or its own
        // forwarder.
        //
        // Its profile is NOT resolver-derived. The fleet origin is stamped from
        // `FleetRecord.profile_id`, and the single production fleet-creation
        // site is reached only from the model's `goal_plan` tool, so that id is
        // always the TURN's `ProfileRuntime` id; the monitor origin is likewise
        // `ProfileRuntime`-derived whenever the monitor came from
        // `monitor_create` rather than the `monitor/create` RPC. Together with
        // `MonitorFired` below, these are the only two arms here whose stamp can
        // come from the turn rather than from `resolve_autonomy_profile_id` —
        // which is exactly why filtering is gated on
        // `connection_filterable_profile_scope`, under which the two resolutions
        // provably agree.
        UiNotification::BackgroundActivity(activity) => {
            optional_profile_scope_matches(activity.profile_id.as_deref(), profile_id)
        }
        _ => true,
    }
}

/// #2067 — compare an OPTIONAL wire profile id against a connection's resolved
/// scope.
///
/// `event_profile_id` is the result of the caller's top-level-then-nested
/// fallback, so a `None` here means the frame names no profile ANYWHERE. That
/// is a LEGACY row: the field is `skip_serializing_if = "Option::is_none"`, and
/// every current producer stamps a concrete id somewhere on the frame
/// (`resolve_autonomy_profile_id` never returns an empty or absent id, and the
/// stored `AutonomyGoalRecord` / `AutonomyLoopRecord` / `AutonomyMonitorRecord`
/// hold a non-optional `profile_id`), so a fully unstamped frame can only have
/// been persisted by a backend that predates the stamp. Only an
/// unprofiled/single-tenant deployment could have produced one, and every
/// connection there resolves to `_main` — so normalizing `None` to `_main` is
/// lossless exactly where such rows exist, while still refusing to hand a
/// legacy row to a real tenant. An empty/whitespace id normalizes the same way
/// rather than becoming an accidental wildcard.
fn optional_profile_scope_matches(event_profile_id: Option<&str>, profile_id: &str) -> bool {
    event_profile_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(MAIN_PROFILE_ID)
        == profile_id
}

fn stdio_session_open_candidate_profile(
    params: &SessionOpenParams,
    current_profile_id: Option<&str>,
) -> Option<String> {
    params
        .profile_id
        .clone()
        .or_else(|| params.session_id.profile_id().map(ToOwned::to_owned))
        .or_else(|| current_profile_id.map(ToOwned::to_owned))
}

/// Deliver one live broadcast ledger event to `ws` through the forwarder's
/// full filter pipeline: baseline cursor (events `<= baseline_seq` were
/// already shipped via replay), self-connection dedupe (Codex MUST-FIX-2 —
/// the originating handler already direct-sent the wire frame on this
/// connection; other connections still receive it via fan-out), topic
/// scope, profile scope, v2 projection (before the capability gate so a v2
/// connection evaluates `EnvelopeV2` while legacy/v1 connections keep the
/// original event byte-for-byte), and the per-connection capability filter.
///
/// The per-event body of `spawn_live_forwarder`'s pump loop. `Ok(())`
/// covers "sent", "filtered", AND backpressure
/// (`send_ledger_event_durable` already opportunistically emits
/// `replay_lossy`; the caller keeps pumping so a recovered consumer gets
/// caught up). `Err` is only the #924 BLOCK 2 writer-fatal pair — a closed
/// writer OR a latched failure both mean further pumps produce FatalClosed
/// forever, so the caller must stop spinning.
#[allow(clippy::too_many_arguments)]
async fn forward_live_ledger_event(
    ws: &WsConnection,
    ledger: &Arc<UiProtocolLedger>,
    event: LedgeredUiProtocolEvent,
    baseline_seq: u64,
    self_connection_id: ConnectionId,
    features: ConnectionUiFeatures,
    topic_scope: Option<&str>,
    profile_scope: Option<&str>,
) -> Result<(), SendError> {
    if event.cursor.seq <= baseline_seq {
        return Ok(());
    }
    if event.from_connection == Some(self_connection_id) {
        return Ok(());
    }
    if !ledger_event_matches_topic_scope(&event.event, topic_scope) {
        return Ok(());
    }
    // #2067 (H2) — the profile scope is captured at session/open, never
    // re-read from the connection: a later `session/open` on the same
    // connection can resolve a different profile, and the shared cell would
    // retarget this pump mid-flight.
    if !ledger_event_matches_profile_scope(&event.event, profile_scope) {
        return Ok(());
    }
    let projected = features
        .projection_envelope_v2
        .then(|| project_v2_ledger_event(ledger, &event.event, &event.cursor))
        .flatten();
    let event_for_wire = context_event_for_features(projected.unwrap_or(event.event), features);
    if !live_event_passes_capability_filter(&event_for_wire, features) {
        return Ok(());
    }
    // #2065 — await-safe send: a full stdio
    // queue parks THIS task cooperatively (non-blocking probe + async
    // sleep), never a blocking `SyncSender::send` on any thread — so the
    // park is cancellable and an abort+join retires it with no detached
    // in-flight work.
    match send_ledger_event_durable_offloaded(ws, ledger, event_for_wire).await {
        Err(err @ (SendError::Closed | SendError::FatalClosed)) => Err(err),
        _ => Ok(()),
    }
}

/// Slow consumer fell behind the broadcast ring. The ledger is durable; the
/// client's cursor is the source of truth and a follow-up session/hydrate
/// or reconnect with the last cursor catches them up. This is also the
/// server-side gap detection point for v2: the live projection stream
/// skipped durable records and the client must rehydrate from its cursor.
fn log_live_forwarder_lag(session_id: &SessionKey, skipped: u64, features: ConnectionUiFeatures) {
    if features.projection_envelope_v2 {
        metrics::counter!("octos_ui_protocol_v2_replay_gap_total").increment(1);
    }
    tracing::warn!(
        target: "octos::ui_protocol::ws",
        session_id = %session_id.0,
        skipped_events = skipped,
        "live ledger forwarder lagged; client must rehydrate via cursor"
    );
}

/// Pump live ledger events for `session_id` into the connection's WS write
/// channel. Filters out events with `cursor.seq <= baseline_seq` (which
/// were already shipped via replay) and applies the same capability
/// gating as the live-emit path. The task ends when the WS write channel
/// closes (peer gone), the broadcast sender is dropped (rare), or the
/// connection cleanup aborts the handle.
///
/// #2065 — every await point here (recv, the offloaded send's capacity
/// park) is cancellable and every enqueue is an atomic `try_send`, so
/// abort+join retires the lane with no detached in-flight work: an
/// uncancellable `spawn_blocking(SyncSender::send)` used to be able to
/// outlive the abort and enqueue a stale frame onto the replacement lane.
/// See `send_durable_offloaded`.
// Each parameter is an independent piece of the forwarder's runtime state
// (connection, ledger, replay baseline, negotiated features, broadcast
// receiver); grouping them would only obscure the per-connection wiring.
#[allow(clippy::too_many_arguments)]
async fn spawn_live_forwarder(
    ws: WsConnection,
    ledger: Arc<UiProtocolLedger>,
    session_id: SessionKey,
    baseline_seq: u64,
    self_connection_id: ConnectionId,
    features: ConnectionUiFeatures,
    topic_scope: Option<String>,
    profile_scope: Option<String>,
    mut rx: tokio::sync::broadcast::Receiver<LedgeredUiProtocolEvent>,
    forwarders: SharedLiveForwarders,
) {
    use tokio::sync::broadcast::error::RecvError;

    // Codex #1336 round-2 BLOCKER 1: keep the per-connection feature
    // snapshot on WsConnection in lockstep with what we received. The
    // forwarder is the canonical source for "this connection's
    // negotiated features" — the direct-send helpers
    // (`send_notification_durable` / `send_notification_ephemeral` /
    // `send_notification_lifecycle`) read this snapshot to apply the
    // same `live_event_passes_capability_filter` the forwarder
    // applies on the broadcast path. Without this sync, a test (or a
    // production path that forgot to call `update_live_features`
    // after `client/hello`) could see the forwarder filter the
    // broadcast copy AND the direct-send path filter it again — or
    // worse, the direct send pass through legacy frames a
    // `projection.envelope.v1` client did not negotiate to receive.
    ws.update_live_features(features);

    // #2065 — retire any PREVIOUS forwarder for
    // this session BEFORE the replacement exists, so "one live lane per
    // (connection, session)" holds across re-opens. The production open
    // path already retired it even earlier — before the replay baseline
    // was computed (see `handle_session_open`) — so this is
    // an idempotent second line of defense for direct callers. abort+join
    // is a full retirement: cancellation lands at a recv/park await and
    // enqueues are atomic, so nothing detached survives the join.
    let previous = forwarders.lock().await.remove(&session_id);
    if let Some(previous) = previous {
        previous.abort();
        let _ = previous.await;
    }

    let session_for_log = session_id.clone();
    let task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if matches!(
                        forward_live_ledger_event(
                            &ws,
                            &ledger,
                            event,
                            baseline_seq,
                            self_connection_id,
                            features,
                            topic_scope.as_deref(),
                            profile_scope.as_deref(),
                        )
                        .await,
                        // #924 BLOCK 2: a closed writer OR a latched failure
                        // both mean further pumps produce FatalClosed
                        // forever; stop spinning.
                        Err(SendError::Closed | SendError::FatalClosed)
                    ) {
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    log_live_forwarder_lag(&session_for_log, skipped, features);
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
    // #924 NIT 8: store the full JoinHandle so the connection-cleanup
    // path can `await` the aborted task before pruning idle subscribers.
    // Any previous forwarder was retired ABOVE (and, on the open path,
    // before the replay baseline), so this insert never displaces a live
    // task.
    forwarders.lock().await.insert(session_id, task);
}

/// Build the Stage-1 v2 projection for one already-durable source event.
///
/// The returned notification is a *wire projection*, not a second ledger
/// append. Its cursor is the cursor of `event`, so enabling v2 cannot shift a
/// legacy client's cursor sequence or otherwise alter its bytes. V1 envelope
/// rows remain the durable source for streamed content; legacy terminal,
/// attachment, and background-completion rows fill the v2 gaps that v1 could
/// not represent canonically.
fn project_v2_ledger_event(
    ledger: &UiProtocolLedger,
    event: &UiProtocolLedgerEvent,
    cursor: &UiCursor,
) -> Option<UiProtocolLedgerEvent> {
    let UiProtocolLedgerEvent::Notification(notification) = event else {
        return None;
    };

    let projection = match notification {
        UiNotification::EnvelopeV2(envelope) => envelope.clone(),
        UiNotification::Envelope(envelope) => {
            let source = &envelope.envelope;
            let assistant_segment_id = || {
                format!(
                    "{}:assistant:{}",
                    source.thread_id,
                    ledger.projection_v2_assistant_segment_index(
                        &envelope.session_id,
                        &source.thread_id,
                        cursor.seq,
                    )
                )
            };
            let payload = match &source.payload {
                Payload::UserMessage { text, files } => PayloadV2::UserMessage {
                    text: text.clone(),
                    files: files.clone(),
                },
                Payload::AssistantDelta { text } => PayloadV2::AssistantDelta {
                    text: text.clone(),
                    assistant_segment_id: assistant_segment_id(),
                },
                Payload::ReasoningDelta { text } => {
                    PayloadV2::ReasoningDelta { text: text.clone() }
                }
                Payload::AssistantPersisted { text, meta } => PayloadV2::AssistantPersisted {
                    text: text.clone(),
                    assistant_segment_id: assistant_segment_id(),
                    meta: meta.clone(),
                },
                Payload::ToolStart {
                    tool_call_id,
                    name,
                    arguments_preview,
                } => PayloadV2::ToolStart {
                    tool_call_id: tool_call_id.clone(),
                    name: name.clone(),
                    arguments_preview: arguments_preview.clone(),
                },
                Payload::ToolProgress {
                    tool_call_id,
                    message,
                } => PayloadV2::ToolProgress {
                    tool_call_id: tool_call_id.clone(),
                    message: message.clone(),
                },
                Payload::ToolEnd {
                    tool_call_id,
                    status,
                    error,
                    reason,
                    output_preview,
                    duration_ms,
                } => PayloadV2::ToolEnd {
                    tool_call_id: tool_call_id.clone(),
                    status: *status,
                    error: error.clone(),
                    reason: reason.clone(),
                    output_preview: output_preview.clone(),
                    duration_ms: *duration_ms,
                },
                // File ownership and all terminal outcomes originate from
                // their richer legacy source events below. Mapping these v1
                // payloads too would create duplicates and lose ownership /
                // errored / interrupted information.
                Payload::FileAttached { .. } | Payload::TurnCompleted { .. } => return None,
            };
            EnvelopeV2Notification {
                session_id: envelope.session_id.clone(),
                topic: envelope.topic.clone(),
                envelope: EnvelopeV2 {
                    thread_id: source.thread_id.clone(),
                    seq: source.seq,
                    cursor: Some(cursor.clone()),
                    turn_id: source.thread_id.clone(),
                    client_message_id: source.client_message_id.clone(),
                    payload,
                },
            }
        }
        UiNotification::TurnCompleted(completed) => {
            let thread_id = completed.turn_id.0.to_string();
            EnvelopeV2Notification {
                session_id: completed.session_id.clone(),
                topic: completed.topic.clone(),
                envelope: EnvelopeV2 {
                    seq: ledger.projection_v2_next_envelope_seq(
                        &completed.session_id,
                        &thread_id,
                        cursor.seq,
                    ),
                    thread_id: thread_id.clone(),
                    cursor: Some(cursor.clone()),
                    turn_id: thread_id,
                    client_message_id: None,
                    payload: PayloadV2::TurnTerminal {
                        outcome: TurnTerminalOutcome::Completed,
                        error: None,
                        token_usage: Some(EnvelopeTokenUsage {
                            input_tokens: completed.tokens_in.map(u64::from).unwrap_or(0),
                            output_tokens: completed.tokens_out.map(u64::from).unwrap_or(0),
                            reasoning_tokens: 0,
                            cache_read_tokens: 0,
                            cache_write_tokens: 0,
                        }),
                    },
                },
            }
        }
        UiNotification::TurnError(error) => {
            let thread_id = error.turn_id.0.to_string();
            let outcome = if error.code == "interrupted" {
                TurnTerminalOutcome::Interrupted
            } else {
                TurnTerminalOutcome::Errored
            };
            EnvelopeV2Notification {
                session_id: error.session_id.clone(),
                topic: error.topic.clone(),
                envelope: EnvelopeV2 {
                    seq: ledger.projection_v2_next_envelope_seq(
                        &error.session_id,
                        &thread_id,
                        cursor.seq,
                    ),
                    thread_id: thread_id.clone(),
                    cursor: Some(cursor.clone()),
                    turn_id: thread_id,
                    client_message_id: None,
                    payload: PayloadV2::TurnTerminal {
                        outcome,
                        error: Some(TurnTerminalError {
                            code: error.code.clone(),
                            message: error.message.clone(),
                            data: error
                                .partial_result
                                .as_ref()
                                .map(|partial| json!({"partial_result": partial})),
                        }),
                        token_usage: error.token_usage.clone(),
                    },
                },
            }
        }
        UiNotification::FileAttached(file) => {
            let thread_id = file.turn_id.0.to_string();
            // The existing background sender emits its per-file legacy
            // notifications after `turn/spawn_complete`. That completion is
            // a linked v2 child stream and already carries its media, so
            // never append a file envelope after the parent's terminal.
            if ledger.projection_v2_has_background_child(&file.session_id, &thread_id, cursor.seq) {
                return None;
            }
            let size_bytes = std::fs::metadata(&file.path)
                .map(|meta| meta.len())
                .unwrap_or(0);
            let mime = file
                .mime
                .as_deref()
                .filter(|mime| !mime.trim().is_empty())
                .unwrap_or("application/octet-stream")
                .to_owned();
            EnvelopeV2Notification {
                session_id: file.session_id.clone(),
                topic: file.topic.clone(),
                envelope: EnvelopeV2 {
                    seq: ledger.projection_v2_next_envelope_seq(
                        &file.session_id,
                        &thread_id,
                        cursor.seq,
                    ),
                    thread_id: thread_id.clone(),
                    cursor: Some(cursor.clone()),
                    turn_id: thread_id.clone(),
                    client_message_id: None,
                    payload: PayloadV2::FileAttached {
                        path: file.path.clone(),
                        mime,
                        size_bytes,
                        attachment_owner: file.attachment_owner.clone().unwrap_or_else(|| {
                            AttachmentOwnerV2 {
                                assistant_segment_id: Some(format!(
                                    "{}:assistant:{}",
                                    thread_id,
                                    ledger.projection_v2_current_assistant_segment_index(
                                        &file.session_id,
                                        &thread_id,
                                        cursor.seq,
                                    )
                                )),
                                tool_call_id: file.tool_call_id.clone(),
                            }
                        }),
                    },
                },
            }
        }
        UiNotification::TurnSpawnComplete(spawn) => {
            let parent_turn_id = spawn
                .turn_id
                .as_ref()
                .map(|turn_id| turn_id.0.to_string())
                .or_else(|| spawn.thread_id.clone())?;
            let child_stream_id = format!("{parent_turn_id}:background:{}", spawn.task_id);
            EnvelopeV2Notification {
                session_id: spawn.session_id.clone(),
                topic: spawn.topic.clone(),
                envelope: EnvelopeV2 {
                    thread_id: child_stream_id.clone(),
                    seq: 1,
                    cursor: Some(cursor.clone()),
                    turn_id: child_stream_id,
                    client_message_id: None,
                    payload: PayloadV2::BackgroundChildCompleted {
                        parent_turn_id,
                        response_to_client_message_id: spawn.response_to_client_message_id.clone(),
                        task_id: spawn.task_id.clone(),
                        content: spawn.content.clone(),
                        tool_call_id: spawn.tool_call_id.clone(),
                        message_id: spawn.message_id.clone(),
                        source: spawn.source.clone(),
                        persisted_at: spawn.persisted_at,
                        media: spawn.media.clone(),
                    },
                },
            }
        }
        _ => return None,
    };

    Some(UiProtocolLedgerEvent::Notification(
        UiNotification::EnvelopeV2(projection),
    ))
}

/// Apply per-connection capability gates to durable UI events.
///
/// Canonical v2 envelopes are never capability-filtered: the Stage-5 server
/// contract delivers `projection/envelope` v2 whenever one is present. The
/// remaining gates cover unrelated additive notifications and historical
/// source records that can still be projected for replay compatibility.
fn live_event_passes_capability_filter(
    event: &UiProtocolLedgerEvent,
    features: ConnectionUiFeatures,
) -> bool {
    // Stage 5: a persisted v2 envelope is the sole canonical delivery lane.
    // It must reach every connection independently of an obsolete capability
    // bit, including direct sends and replay.
    if matches!(
        event,
        UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(_))
    ) {
        return true;
    }

    // A v2-capable connection receives only projections for source records.
    // Keep this branch before the historical capability gates below so a
    // replayed source event cannot leak beside its v2 projection.
    if features.projection_envelope_v2 {
        if let UiProtocolLedgerEvent::Notification(
            UiNotification::Envelope(_)
            | UiNotification::MessageDelta(_)
            | UiNotification::ReasoningDelta(_)
            | UiNotification::ToolStarted(_)
            | UiNotification::ToolProgress(_)
            | UiNotification::ToolCompleted(_)
            | UiNotification::FileAttached(_)
            | UiNotification::TurnCompleted(_)
            | UiNotification::TurnError(_)
            | UiNotification::TurnSpawnComplete(_),
        ) = event
        {
            return false;
        }
    }
    if !features.context_lifecycle_available() {
        if let UiProtocolLedgerEvent::Notification(
            UiNotification::ContextCompactionCompleted(_)
            | UiNotification::ContextCompactionStarted(_)
            | UiNotification::ContextNormalizationReported(_),
        ) = event
        {
            return false;
        }
    }
    if !features.spawn_complete {
        // Clients that did not negotiate this historical notification never
        // receive it. New background results are v2 envelopes instead.
        if let UiProtocolLedgerEvent::Notification(UiNotification::TurnSpawnComplete(_)) = event {
            return false;
        }
    }
    // UPCR-2026-014 M9-α-9: `event.file_attached.v1` gate. Old clients
    // never see the new `file/attached` envelope. New
    // clients see `file/attached` as an additional dedicated signal.
    if !features.file_attached {
        if let UiProtocolLedgerEvent::Notification(UiNotification::FileAttached(_)) = event {
            return false;
        }
    }
    // `plan.todos.v1` gate: the model-authored plan checklist only reaches
    // connections that negotiated it (and know how to render `plan/updated`).
    // Applies on both the live broadcast and reconnect replay, since both
    // routes call this filter.
    if !features.plan_todos {
        if let UiProtocolLedgerEvent::Notification(UiNotification::PlanUpdated(_)) = event {
            return false;
        }
    }
    // #2019 `event.background_activity.v1` gate. The human sink is a NEW
    // notification shape; a client that did not negotiate it cannot render it
    // and would report "unknown UI protocol notification" — the exact
    // ui-protocol-v2-migration trap this pairing exists to avoid. Applies on
    // both the live broadcast and reconnect replay (both call this filter).
    if !features.background_activity {
        if let UiProtocolLedgerEvent::Notification(UiNotification::BackgroundActivity(_)) = event {
            return false;
        }
    }
    // UPCR-2026-023 `user_question.v1` gate. A client that did not negotiate
    // the structured-question capability never installed a
    // `SessionUserQuestionRequester` and has no `user_question/respond` path,
    // so it must never receive a `user_question/requested` it cannot answer —
    // neither via the live broadcast NOR via reconnect replay (both routes
    // call this filter). Mirrors the `ApprovalRequested` / typed-approval
    // gating discipline: only deliver an interactive prompt to a connection
    // that can act on it.
    if !features.user_question_v1 {
        if let UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionRequested(_)) = event
        {
            return false;
        }
    }
    // UPCR-2026-014 M9-γ cutover: per-connection mutual exclusion.
    //
    // Connections that NEGOTIATED `projection.envelope.v1` see historical
    // v1 projection envelopes only — the legacy notifications
    // they supersede are filtered out on this side. Connections that did
    // NOT negotiate see legacy notifications ONLY — envelopes are
    // filtered out. This is the cutover gate that makes the M9-γ
    // projection contract enforceable end-to-end without dual-rendering
    // the same logical event in two shapes.
    //
    // Legacy events superseded by envelopes per spec § 14.7:
    //   - message/delta             → assistant_delta envelope
    //   - message/reasoning_delta   → reasoning_delta envelope
    //   - tool/started              → tool_start envelope
    //   - tool/progress             → tool_progress envelope
    //   - tool/completed            → tool_end envelope
    //   - file/attached             → file_attached envelope
    //   - turn/completed            → turn_completed envelope
    //
    // Note: the legacy *emit* sites stay in place — clients that did
    // NOT negotiate the feature still need them. What this gate
    // changes is the per-connection wire delivery.
    if features.projection_envelope {
        if let UiProtocolLedgerEvent::Notification(
            UiNotification::MessageDelta(_)
            | UiNotification::ReasoningDelta(_)
            | UiNotification::ToolStarted(_)
            | UiNotification::ToolProgress(_)
            | UiNotification::ToolCompleted(_)
            | UiNotification::FileAttached(_)
            | UiNotification::TurnCompleted(_),
        ) = event
        {
            return false;
        }
    } else if let UiProtocolLedgerEvent::Notification(UiNotification::Envelope(_)) = event {
        return false;
    }
    true
}

#[derive(Debug)]
struct SessionOpenOutcome {
    result: SessionOpenResult,
    replay: Vec<LedgeredUiProtocolEvent>,
    pending_approvals: Vec<ApprovalRequestedEvent>,
    /// UPCR-2026-023: still-pending structured user-questions to replay on
    /// reconnect, mirroring [`pending_approvals`](Self::pending_approvals).
    /// Gated by the `user_question.v1` capability at the send site.
    pending_questions: Vec<UserQuestionRequestedEvent>,
    opened_event: LedgeredUiProtocolEvent,
    /// Head seq observed atomically with the replay snapshot. The live
    /// forwarder uses this — NOT `opened_event.cursor.seq` — as its
    /// drop-everything-≤-this baseline. Closes the replay/open race
    /// where an event landing between replay and the session/open append
    /// would otherwise be filtered out (codex PR #761 MUST-FIX-1).
    replay_baseline_seq: u64,
    /// #2067 R3 — the profile scope this connection may be FILTERED against,
    /// or `None` when its scope is not a tenant boundary and filtering it
    /// would starve it. See `connection_filterable_profile_scope`.
    profile_scope: Option<String>,
}

// Threading both the approval store and the (UPCR-2026-023) question store
// through the session/open contract pushes this past clippy's 7-arg lint;
// the parameters are a flat dependency list, not a missing struct.
#[allow(clippy::too_many_arguments)]
async fn open_session_result(
    state: &Arc<AppState>,
    ledger: &UiProtocolLedger,
    approvals: &PendingApprovalStore,
    questions: &PendingQuestionStore,
    connection_id: ConnectionId,
    connection_profile_id: Option<&str>,
    pinned_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    mut params: SessionOpenParams,
) -> Result<SessionOpenOutcome, RpcError> {
    normalize_session_open_params_topic(&mut params);
    let topic_scope = params.topic.clone();
    let active_profile_id = validate_session_scope(
        &params.session_id,
        params.profile_id.as_deref(),
        connection_profile_id,
    )?;
    let ledger_profile_id = active_profile_id
        .clone()
        .unwrap_or_else(|| MAIN_PROFILE_ID.to_owned());
    // #2067 — see `ledger_event_matches_profile_scope`. `validate_session_scope`
    // returns the connection profile verbatim when there is one, so this equals
    // `ledger_profile_id` in that case; naming the pin directly keeps the
    // invariant (scope == the turn's profile) visible at the resolution site.
    let profile_scope = pinned_profile_id.map(ToOwned::to_owned);
    if let Some(profile_id) = active_profile_id.as_deref() {
        ensure_known_profile(state, profile_id)?;
    }
    ensure_session_profile_runtime(state, active_profile_id.as_deref()).await?;
    let requested_workspace =
        validate_requested_session_cwd(state, features, active_profile_id.as_deref(), &params)?;
    validate_session_sandbox_feature_gate(features, &params)?;
    // M11-F deliverable D: re-introduce the
    // `appui.default_session_cwd` Tier-2 fallback that M11-E's
    // `clone_session_tools` deletion took out. Pre-resolution order:
    //   Tier 1 — `requested_workspace` (validated client cwd above).
    //   Tier 2 — `AppState::appui_default_session_cwd` (operator default).
    //   Tier 3 — `SessionRuntime::bootstrap`'s
    //            `<profile.data_dir>/users/<encoded base>/workspace`.
    //
    // We resolve Tier 2 here, in the UI Protocol entrypoint, rather
    // than threading it through `SessionRuntime::bootstrap`. Rationale:
    //  - The bootstrap signature stays stable across M11-F.
    //  - Tier 2 is a serve-level operator setting (octos serve reads
    //    `config.appui.default_session_cwd`) — the runtime layer
    //    doesn't otherwise see operator-level config, so leaving the
    //    resolution at the dispatcher keeps `ProfileRuntime` /
    //    `SessionRuntime` free of `AppState`-shaped knowledge.
    //  - The hint is passed verbatim into `SessionRuntimeCache::get_or_init`,
    //    which forwards it to `SessionRuntime::bootstrap`'s
    //    `workspace_hint`. `validate_workspace_hint` runs the same
    //    safety check on it as on a client-supplied cwd (canonicalize,
    //    reject banned system roots).
    let effective_workspace_hint: Option<PathBuf> = requested_workspace
        .clone()
        .or_else(|| state.appui_default_session_cwd.clone())
        // An implicit same-process reopen may reuse its already established
        // explicit binding, but a recovered disk cwd is never inferred here.
        .or_else(|| session_workspaces().runtime_hint(&ledger_profile_id, &params.session_id));
    if effective_workspace_hint.is_none() {
        require_recovered_scoped_session_open(
            state,
            ledger,
            &params.session_id,
            &ledger_profile_id,
        )?;
    }
    // M11-E: when a profile is registered for this session, materialize
    // the `SessionRuntime` against the validated workspace hint NOW so
    // the subsequent `turn/start` (and any cached read of
    // `session_runtime.workspace_root`) observes the supplied cwd.
    //
    // The cache's `get_or_init` is single-flight: a same-key hit returns
    // the EXISTING `Arc<SessionRuntime>` and IGNORES the new
    // `workspace_hint`. That means a client cannot silently change a
    // running session's cwd by re-opening with a different `cwd`
    // parameter; the first cwd wins until the runtime is evicted (LRU,
    // idle TTL, explicit `invalidate`). The `SessionOpened` reply is
    // sourced from the cached runtime's `workspace_root` (not the
    // requested hint) so the wire response truthfully reflects which
    // workspace the next turn will use — closing the cache/wire
    // divergence codex flagged on PR #884 follow-up.
    //
    // The `session_workspaces()` map is kept as a thin read-through view for
    // the legacy WS dispatcher fallback (no profile registered — setup wizard
    // / single-agent serve) and for pane snapshots that need a sync read of
    // the workspace root. It stores both the effective root and the provenance
    // of the runtime hint: a derived Tier-3 root remains available to tools and
    // UI without being mistaken for an explicit cwd on a later cache lookup.
    let mut effective_workspace_root: Option<PathBuf> = None;
    let mut effective_runtime_hint: Option<PathBuf> = None;
    // The provider the open-time context snapshot derives its compaction
    // threshold from — the SAME peer-lane→profile-primary resolution the
    // session's turns use. Stays `None` when no session runtime
    // materializes (profile-less open), which makes the snapshot fail open
    // (publish without compacting) rather than guess a window.
    let mut open_context_provider: Option<Arc<dyn octos_llm::LlmProvider>> = None;
    if let Some(profile_runtime) =
        resolve_session_profile_runtime(state, active_profile_id.as_deref())
    {
        let hint = effective_workspace_hint.clone();
        // Capture the session epoch BEFORE resolving permissions so any
        // concurrent `permission/profile/set` (which bumps the epoch AFTER
        // writing the store) that could have made these permissions stale
        // also invalidates this epoch — the cache insert then rejects the
        // stale bootstrap (codex P1 round 3 on #1639).
        let permissions_epoch = state.session_cache.session_generation(&params.session_id);
        let permissions = effective_permissions_for_session(state, &params.session_id)?;
        let sandbox_override = validate_requested_session_sandbox(
            features,
            &params,
            &permissions.apply_to_sandbox(&profile_runtime.default_sandbox),
        )?;
        match state
            .session_cache
            .get_or_init_with_permissions_and_sandbox(
                &profile_runtime,
                params.session_id.clone(),
                hint,
                permissions,
                sandbox_override,
                permissions_epoch,
            )
            .await
        {
            Ok(runtime) => {
                // Per-project ledger isolation (#1666): registered BEFORE the
                // `replay_after_with_head` below, so this open replays (and
                // this session's turns later append) under the per-cwd
                // storage identity. No-op when the store wasn't relocated.
                register_session_ledger_scope(state, ledger, &runtime);
                open_context_provider = Some(runtime.profile.llm.clone());
                effective_workspace_root = Some(runtime.workspace_root.clone());
                effective_runtime_hint = effective_workspace_hint
                    .as_ref()
                    .map(|_| runtime.workspace_root.clone());
                // Sticky marker: record this profile as the folder's active one
                // so a later bare launch resumes it deterministically (beats the
                // store-mtime recency fallback in `derive_sticky_profile`). Gated
                // on the SAME condition `resolve_sessions_root_from_hint` uses to
                // relocate the store — `sessions_in_cwd && effective hint present`
                // — so the marker lands in the same `<cwd>/.octos` the store did
                // (client cwd OR operator `appui_default_session_cwd`), and a
                // no-hint (web/gateway) session that stays in its data-dir
                // workspace never drops a stray marker there. Written to the
                // runtime's canonical `workspace_root` (the store's actual parent,
                // post-canonicalize), not the raw hint. Best-effort in the helper.
                if state.session_cache.sessions_in_cwd() && effective_workspace_hint.is_some() {
                    crate::runtime::session::write_active_profile_marker(
                        &runtime.workspace_root,
                        &profile_runtime.profile_id,
                    );
                }
            }
            Err(error) => {
                tracing::error!(
                    error = %error,
                    profile_id = %profile_runtime.profile_id,
                    session = %params.session_id,
                    "session/open: SessionRuntime::bootstrap failed",
                );
                // The most common bootstrap failure is a non-writable workspace
                // folder — the session can't create its `.octos-workspace.toml`
                // there. Surface a clear, actionable message naming the folder
                // instead of the opaque "failed to bootstrap session runtime".
                if is_permission_denied_error(&error) {
                    return Err(workspace_not_writable_error(params.cwd.as_deref()));
                }
                return Err(runtime_unavailable_error(format!(
                    "failed to bootstrap session runtime: {error}"
                )));
            }
        }
    } else if params.sandbox.is_some() {
        return Err(RpcError::invalid_params(
            "session/open sandbox requires a configured profile runtime",
        )
        .with_data(json!({
            "kind": "sandbox_runtime_unavailable",
            "active_profile_id": active_profile_id,
        })));
    } else if let Some(workspace_root) = effective_workspace_hint.as_ref() {
        // No profile registered (legacy single-agent serve). Stash the
        // effective hint in the read-through map so the legacy
        // dispatcher's pane-snapshot path can pick it up.
        effective_workspace_root = Some(workspace_root.clone());
        effective_runtime_hint = Some(workspace_root.clone());
    }
    if let Some(root) = effective_workspace_root.as_ref() {
        session_workspaces().set_resolved(
            &ledger_profile_id,
            params.session_id.clone(),
            root.clone(),
            effective_runtime_hint,
        );
    }
    // Open-time context snapshot — deliberately BEFORE the replay head is
    // taken: when the open-time threshold pass compacts an oversized rebuilt
    // ledger, its started/completed lifecycle events are appended to the
    // ledger HERE so this very open's replay delivers them in-band and the
    // client renders the compaction UX (live block + sticky notice). A
    // silent 1.17M→4K rewrite of the session's context is exactly the kind
    // of event the UPCR-2026-026 surface exists for. Feature-gating is the
    // replay filter's job (`ContextCompaction*` events are dropped for
    // connections without `context_lifecycle`).
    let Some(sessions) = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        active_profile_id.as_deref(),
        &params.session_id,
    )
    .await
    else {
        return Err(runtime_unavailable_error("Sessions not available"));
    };
    let (data_dir, history) = {
        let mut sessions = sessions.lock().await;
        let data_dir = sessions.data_dir();
        let session = sessions.get_or_create(&params.session_id).await;
        (data_dir, session.messages.clone())
    };
    let (context, context_state, open_compaction_events) = appui_context_open_snapshot(
        &data_dir,
        &params.session_id,
        &history,
        open_context_provider.as_ref(),
    );
    for notification in open_compaction_events {
        let _ = ledger.append_notification_from(notification, connection_id);
    }
    let (mut replay, replay_baseline_seq) =
        ledger.replay_after_with_head(&params.session_id, params.after.as_ref())?;
    replay.retain(|event| {
        ledger_event_matches_topic_scope(&event.event, topic_scope.as_deref())
            && ledger_event_matches_profile_scope(&event.event, profile_scope.as_deref())
    });
    let replayed_approval_ids = replay
        .iter()
        .filter_map(|event| match &event.event {
            UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(approval)) => {
                Some(approval.approval_id.clone())
            }
            _ => None,
        })
        .collect::<HashSet<_>>();
    let pending_approvals = approvals
        .pending_for_session(&params.session_id)
        .into_iter()
        .filter(|approval| {
            let event = UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(
                approval.clone(),
            ));
            ledger_event_matches_topic_scope(&event, topic_scope.as_deref())
        })
        .filter(|approval| !replayed_approval_ids.contains(&approval.approval_id))
        .collect::<Vec<_>>();

    // UPCR-2026-023: replay still-pending structured user-questions on
    // reconnect, mirroring the pending-approval replay above EXACTLY —
    // topic-scope filtered, and de-duplicated against any question already
    // carried in the cursor replay window. The `user_question.v1` capability
    // gate is applied at the send site (`live_event_passes_capability_filter`
    // / the dedicated send loop) so a non-negotiated client never receives a
    // question it cannot answer.
    let replayed_question_ids = replay
        .iter()
        .filter_map(|event| match &event.event {
            UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionRequested(
                question,
            )) => Some(question.question_id.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let pending_questions = questions
        .pending_for_session(&params.session_id)
        .into_iter()
        .filter(|question| {
            let event = UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionRequested(
                question.clone(),
            ));
            ledger_event_matches_topic_scope(&event, topic_scope.as_deref())
        })
        .filter(|question| !replayed_question_ids.contains(&question.question_id))
        .collect::<Vec<_>>();

    let (context, context_state) = if features.context_lifecycle_available() {
        (Some(context), Some(context_state))
    } else {
        (None, None)
    };

    // The cached SessionRuntime's `workspace_root` is the source of truth
    // for the wire response when present. Fall back to the legacy lookup
    // when no SessionRuntime was materialized (no profile registered).
    let workspace_root = effective_workspace_root.or_else(|| {
        session_workspace_root_for_profile(active_profile_id.as_deref(), &params.session_id)
    });
    let panes = features
        .pane_snapshots
        .then(|| build_pane_snapshot(&data_dir, &params.session_id, workspace_root.as_deref()));
    // UPCR-2026-007: advertise the negotiated capability set in-band so
    // clients don't have to rely on out-of-band knowledge of which feature
    // tokens the server honours.
    let capabilities = features.advertised_capabilities(state);
    // Surface the server-persisted per-session reasoning/thinking effort so a
    // restarting/reconnecting TUI can restore its local `/thinking` state and
    // mark its menu. Read from the same disk-backed store the turn path writes
    // (`<data_dir>/users/.../<topic>.reasoning_effort.json`); `None` when the
    // session has never set an effort.
    let reasoning_effort = crate::api::ui_protocol_reasoning_effort::read_reasoning_effort_async(
        &data_dir,
        &params.session_id,
    )
    .await;
    // Tag the broadcast with our connection id so the live forwarder
    // installed below skips this event (we direct-send it inline at the
    // call site). Other connections still observe the broadcast.
    let opened_event = ledger.append_notification_from(
        UiNotification::SessionOpened(SessionOpened {
            session_id: params.session_id,
            active_profile_id,
            workspace_root: workspace_root.map(|path| path.to_string_lossy().to_string()),
            context,
            context_state,
            cursor: None,
            panes,
            capabilities,
            reasoning_effort,
        }),
        connection_id,
    );
    let UiProtocolLedgerEvent::Notification(UiNotification::SessionOpened(mut opened)) =
        opened_event.event.clone()
    else {
        unreachable!("session/open ledger append returns session/open notification");
    };
    let (context, context_state) =
        context_snapshot_for_features(opened.context, opened.context_state, features);
    opened.context = context;
    opened.context_state = context_state;
    Ok(SessionOpenOutcome {
        result: SessionOpenResult::new(opened),
        replay,
        pending_approvals,
        pending_questions,
        opened_event,
        replay_baseline_seq,
        profile_scope,
    })
}

fn validate_requested_session_cwd(
    state: &AppState,
    features: ConnectionUiFeatures,
    active_profile_id: Option<&str>,
    params: &SessionOpenParams,
) -> Result<Option<PathBuf>, RpcError> {
    let Some(cwd) = params
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
    else {
        return Ok(None);
    };

    if !features.session_workspace_cwd {
        return Err(RpcError::invalid_params(
            "session/open cwd requires feature session.workspace_cwd.v1",
        )
        .with_data(json!({
            "kind": "feature_required",
            "feature": UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
        })));
    }

    let workspace_root = canonical_existing_dir(cwd)?;
    validate_session_workspace_allowed(state, active_profile_id, &workspace_root)?;
    Ok(Some(workspace_root))
}

fn validate_session_sandbox_feature_gate(
    features: ConnectionUiFeatures,
    params: &SessionOpenParams,
) -> Result<(), RpcError> {
    if params.sandbox.is_some() && !features.session_sandbox {
        return Err(RpcError::invalid_params(
            "session/open sandbox requires feature session.sandbox.v1",
        )
        .with_data(json!({
            "kind": "feature_required",
            "feature": UI_PROTOCOL_FEATURE_SESSION_SANDBOX_V1,
        })));
    }
    Ok(())
}

fn validate_requested_session_sandbox(
    features: ConnectionUiFeatures,
    params: &SessionOpenParams,
    inherited: &octos_agent::SandboxConfig,
) -> Result<Option<octos_agent::SandboxConfig>, RpcError> {
    validate_session_sandbox_feature_gate(features, params)?;
    let Some(requested) = params.sandbox.as_ref() else {
        return Ok(None);
    };

    let mut narrowed = inherited.clone();
    if let Some(enabled) = requested.enabled {
        if !enabled && inherited.enabled {
            return Err(session_sandbox_widening_error(
                "enabled",
                json!(enabled),
                json!(inherited.enabled),
                "session sandbox cannot disable profile-level sandbox isolation",
            ));
        }
        narrowed.enabled = enabled;
    }

    if let Some(network_access) = requested.network_access {
        if network_access && !inherited.allow_network {
            return Err(session_sandbox_widening_error(
                "network_access",
                json!(network_access),
                json!(inherited.allow_network),
                "session sandbox cannot enable network access denied by the profile",
            ));
        }
        narrowed.allow_network = network_access;
    }

    if !requested.read_allow_paths.is_empty() {
        let requested_paths =
            canonical_session_sandbox_paths("read_allow_paths", &requested.read_allow_paths)?;
        if !inherited.read_allow_paths.is_empty() {
            let inherited_paths = canonical_session_sandbox_paths(
                "inherited_read_allow_paths",
                &inherited.read_allow_paths,
            )?;
            for requested_path in &requested_paths {
                if !inherited_paths
                    .iter()
                    .any(|allowed| requested_path == allowed || requested_path.starts_with(allowed))
                {
                    return Err(session_sandbox_widening_error(
                        "read_allow_paths",
                        json!(requested_path.to_string_lossy()),
                        json!(
                            inherited_paths
                                .iter()
                                .map(|path| path.to_string_lossy().to_string())
                                .collect::<Vec<_>>()
                        ),
                        "session sandbox read paths must stay within the profile allowlist",
                    ));
                }
            }
        }
        narrowed.read_allow_paths = requested_paths
            .into_iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect();
    }

    Ok(Some(narrowed))
}

fn canonical_session_sandbox_paths(
    field: &str,
    paths: &[String],
) -> Result<Vec<PathBuf>, RpcError> {
    paths
        .iter()
        .map(|path| {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                return Err(RpcError::invalid_params(format!(
                    "session/open sandbox {field} path must not be empty"
                ))
                .with_data(json!({
                    "kind": "session_sandbox_path_empty",
                    "field": field,
                })));
            }
            let expanded = expand_home_path(trimmed);
            std::fs::canonicalize(&expanded).map_err(|error| {
                RpcError::invalid_params(format!(
                    "session/open sandbox {field} path is not accessible: {path}"
                ))
                .with_data(json!({
                    "kind": "session_sandbox_path_not_accessible",
                    "field": field,
                    "path": path,
                    "error": error.to_string(),
                }))
            })
        })
        .collect()
}

fn session_sandbox_widening_error(
    field: &'static str,
    requested: Value,
    inherited: Value,
    message: &'static str,
) -> RpcError {
    RpcError::permission_denied(message).with_data(json!({
        "kind": "session_sandbox_would_widen_profile",
        "field": field,
        "requested": requested,
        "inherited": inherited,
    }))
}

fn canonical_existing_dir(path: &str) -> Result<PathBuf, RpcError> {
    let expanded = expand_home_path(path);
    let canonical = std::fs::canonicalize(&expanded).map_err(|error| {
        RpcError::invalid_params(format!("session/open cwd is not accessible: {path}")).with_data(
            json!({
                "kind": "cwd_not_accessible",
                "cwd": path,
                "error": error.to_string(),
            }),
        )
    })?;
    if !canonical.is_dir() {
        return Err(RpcError::invalid_params(format!(
            "session/open cwd is not a directory: {path}"
        ))
        .with_data(json!({
            "kind": "cwd_not_directory",
            "cwd": path,
        })));
    }
    Ok(canonical)
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path));
    }
    PathBuf::from(path)
}

fn validate_session_workspace_allowed(
    state: &AppState,
    active_profile_id: Option<&str>,
    workspace_root: &Path,
) -> Result<(), RpcError> {
    // M11-F: per-session cwd is only honored on the profile-aware
    // dispatch path (`SessionRuntime` materialized via
    // `SessionRuntimeCache`). The legacy single-agent fallback was
    // deleted in M11-F — `octos serve` bootstraps every profile in
    // `ProfileStore::list()` at startup, so an unregistered profile
    // here is a configuration bug. We still surface the
    // `cwd_runtime_unavailable` typed error so the client sees a
    // distinct shape from a path-safety rejection.
    //
    // We check the SPECIFIC routed profile, not just "any profile is
    // registered" — a multi-profile deployment may have profiles A, B,
    // and a request that routes to profile C should still get the
    // `cwd_runtime_unavailable` rejection (codex round-3 fix). Reject
    // early so the client sees a typed error instead of a silent
    // wire/turn mismatch.
    //
    // Path safety mirrors `SessionRuntime::bootstrap`'s
    // `validate_workspace_hint`: the cwd must canonicalize and must
    // not be rooted under a banned system path (`/etc`, `/usr`,
    // `/sbin`, …). Cross-session containment is intentionally NOT
    // checked here — coding-agent UIs point sessions at arbitrary
    // repos. Session-scope access control belongs in the auth /
    // connection-profile gate (`validate_session_scope`), not the cwd
    // validator.
    if resolve_session_profile_runtime(state, active_profile_id).is_none() {
        // Distinguish "profile exists but no LLM configured" from "no
        // profile registered at all" so the client can route the user
        // to the right setup flow. #952: solo `session/open` previously
        // returned the generic `cwd_runtime_unavailable` even when the
        // profile was simply missing its LLM selection — replace with
        // typed `profile_unconfigured`.
        let candidate = active_profile_id.unwrap_or(MAIN_PROFILE_ID);
        if let Some(store) = state.profile_store.as_ref() {
            if let Ok(Some(profile)) = store.get(candidate) {
                if profile.enabled
                    && profile.parent_id.is_none()
                    && !profile.config.has_llm_selection()
                {
                    return Err(RpcError::invalid_params(
                        "session/open requires the routed profile to have an LLM selection",
                    )
                    .with_data(json!({
                        "kind": "profile_unconfigured",
                        "cwd": workspace_root.to_string_lossy(),
                        "active_profile_id": candidate,
                        "missing": "llm",
                    })));
                }
            }
        }

        return Err(RpcError::invalid_params(
            "session/open cwd requires a configured profile runtime",
        )
        .with_data(json!({
            "kind": "cwd_runtime_unavailable",
            "cwd": workspace_root.to_string_lossy(),
            "active_profile_id": active_profile_id,
        })));
    }

    validate_session_workspace_path_safety(workspace_root)
}

/// Path-safety gate for multi-profile session cwds.
///
/// Mirrors the banned-system-path list in
/// `crate::runtime::session::validate_workspace_hint`. The two paths
/// must stay in lockstep; the duplicate exists because
/// `SessionRuntime::bootstrap` does not see `AppState` and cannot call
/// back into this module. TODO(post-M11): collapse to a shared helper.
fn validate_session_workspace_path_safety(workspace_root: &Path) -> Result<(), RpcError> {
    // `validate_requested_session_cwd` already canonicalized the path
    // and verified it is a directory, so we only need to guard against
    // banned system roots here.
    let mut components = workspace_root.components();
    let _root = components.next();
    if let Some(first) = components.next() {
        let first = first.as_os_str();
        const BANNED: &[&str] = &[
            "etc", "sbin", "bin", "boot", "dev", "proc", "sys", "usr", "var", "root",
        ];
        for entry in BANNED {
            if first == std::ffi::OsStr::new(entry) {
                return Err(RpcError::invalid_params(format!(
                    "session/open cwd is rooted under a system path /{entry}"
                ))
                .with_data(json!({
                    "kind": "cwd_system_path_banned",
                    "cwd": workspace_root.to_string_lossy(),
                    "banned_root": entry,
                })));
            }
        }
    }
    Ok(())
}

/// #1057: shared root-escape detector for `onboarding/workspace_probe`.
///
/// Mirrors `validate_session_workspace_path_safety` but returns the banned
/// system-root component (if any) instead of an `RpcError`. The probe
/// surfaces `root_escape: true` plus the banned component so the TUI can
/// render a typed recovery hint without first eating an `invalid_params`
/// error on `session/open`.
///
/// "Root escape" here means "the resolved canonical workspace would land
/// the agent under a banned OS path (`/etc`, `/usr`, `/proc`, ...)", i.e.
/// the same safety gate `session/open` enforces. We don't add the
/// `bin`/`sbin`/`var` set used by some probes because the probe is the
/// onboarding gate and we want the same answer the runtime would give.
fn workspace_root_escape_under_system_path(path: &Path) -> Option<&'static str> {
    let mut components = path.components();
    let _root = components.next();
    let first = components.next()?;
    let first = first.as_os_str();
    const BANNED: &[&str] = &[
        "etc", "sbin", "bin", "boot", "dev", "proc", "sys", "usr", "var", "root",
    ];
    for entry in BANNED {
        if first == std::ffi::OsStr::new(entry) {
            return Some(*entry);
        }
    }
    None
}

/// #1057: parameters for `onboarding/workspace_probe`.
#[derive(Debug, Deserialize)]
struct OnboardingWorkspaceProbeParams {
    /// User-supplied path string (may contain `~/`, may not exist yet, may
    /// be a regular file rather than a directory). The probe canonicalizes
    /// when possible and reports the answer truthfully — it does not error
    /// on missing paths because the TUI uses this RPC to walk users through
    /// recovery.
    path: String,
}

/// #1057: backend-owned workspace status for TUI onboarding.
///
/// Resolves `params.path` against the local filesystem and returns a
/// summary the TUI can show in its onboarding/setup wizard:
///
/// - `requested_path`: the unmodified input (with `~` expansion noted via
///   `expanded_path`).
/// - `expanded_path`: the `~/`-expanded path before canonicalization. Equal
///   to `requested_path` when no expansion happened.
/// - `canonical_path`: `std::fs::canonicalize` result (string) or `null`
///   when the path doesn't exist.
/// - `exists`, `is_directory`, `writable`: filesystem state.
/// - `workspace_policy.present`: whether `workspace_policy.toml` exists in
///   the resolved root.
/// - `workspace_policy.parse_error`: `null` when the file parses, otherwise
///   the toml parser's error string.
/// - `workspace_policy.kind`: parsed `workspace.kind` (e.g. `"coding"`)
///   when the file parses, else `null`.
/// - `root_escape`: `true` when the canonical (or expanded) path roots
///   under a banned system path (`/etc`, `/usr`, …). The TUI uses this to
///   show a "pick another directory" prompt without running `session/open`
///   first.
/// - `banned_root`: the banned system component (`"etc"`, …) when
///   `root_escape == true`, else `null`.
fn onboarding_workspace_probe_result(state: &AppState, path: &str) -> Result<Value, RpcError> {
    if !supports_local_solo_profile_create(state) {
        // #1057 bullet 4: tenant / cloud rejection is typed so TUI clients
        // get the same shape as `profile/local/create`.
        return Err(local_profile_permission_error(
            "profile_local_unsupported",
            "onboarding/workspace_probe is available only in local solo mode",
            state,
        ));
    }
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(
            RpcError::invalid_params("path is required").with_data(json!({
                "kind": "workspace_probe_invalid_path",
                "reason": "empty",
            })),
        );
    }
    let expanded = expand_home_path(trimmed);
    let canonical = std::fs::canonicalize(&expanded).ok();

    // Determine what path to evaluate "root escape" against: prefer the
    // canonical answer (truthful for symlinks) and fall back to the
    // expanded literal so non-existent paths still get a typed answer.
    let evaluated_path: &Path = canonical.as_deref().unwrap_or(expanded.as_path());
    let banned_root = workspace_root_escape_under_system_path(evaluated_path);
    let root_escape = banned_root.is_some();

    let metadata = canonical.as_deref().and_then(|p| std::fs::metadata(p).ok());
    let exists = metadata.is_some();
    let is_directory = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let writable = canonical
        .as_deref()
        .filter(|_| is_directory)
        .map(directory_is_writable)
        .unwrap_or(false);

    let policy = workspace_policy_probe(canonical.as_deref());

    Ok(json!({
        "requested_path": trimmed,
        "expanded_path": expanded.to_string_lossy(),
        "canonical_path": canonical.as_ref().map(|p| p.to_string_lossy().to_string()),
        "exists": exists,
        "is_directory": is_directory,
        "writable": writable,
        "workspace_policy": policy,
        "root_escape": root_escape,
        "banned_root": banned_root,
        "runtime_mode": runtime_mode_for_state(state),
    }))
}

/// #1057: probe writability by attempting to create + delete a temp file in
/// the resolved workspace root. We do NOT fall back to filesystem-permission
/// bit inspection because on macOS / Linux the effective writability
/// depends on ACLs, mount-time `ro` flags, and capability sets that
/// `Permissions.mode()` cannot answer. The probe must reflect the answer
/// the agent would get when it next tries to write.
fn directory_is_writable(path: &Path) -> bool {
    // #1147 codex P2: use a unique per-call probe filename so a stale
    // file from a killed prior probe (or a user-created `.octos-workspace-probe`)
    // doesn't cause `create_new` to fail with `AlreadyExists` and
    // falsely report `writable=false`. PID + nanos + process-local
    // atomic counter make collision practically impossible.
    use std::sync::atomic::{AtomicU64, Ordering};
    static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let probe = path.join(format!(".octos-workspace-probe-{pid}-{nanos}-{counter}"));
    let attempt = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe);
    match attempt {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn session_permission_profiles() -> Arc<SessionPermissionProfileStore> {
    static SESSION_PERMISSION_PROFILES: OnceLock<Arc<SessionPermissionProfileStore>> =
        OnceLock::new();
    SESSION_PERMISSION_PROFILES
        .get_or_init(|| Arc::new(SessionPermissionProfileStore::default()))
        .clone()
}

/// #1057: report `workspace_policy.toml` presence + parse status. We do not
/// load the policy into the runtime here — that responsibility lives with
/// `SessionRuntime::bootstrap`. The probe only answers "would a future
/// session/open against this directory hit a policy parse failure?".
fn workspace_policy_probe(root: Option<&Path>) -> Value {
    let Some(root) = root else {
        return json!({
            "present": false,
            "parse_error": null,
            "kind": null,
        });
    };
    // #1147 codex P2: probe the canonical runtime policy filename
    // (`octos_agent::workspace_policy::WORKSPACE_POLICY_FILE`,
    // currently `.octos-workspace.toml`) — the previous `workspace_policy.toml`
    // hardcoded name didn't match what `SessionRuntime::bootstrap`
    // actually reads, so the probe reported `present=false` for
    // every real workspace.
    let policy_path = root.join(octos_agent::workspace_policy::WORKSPACE_POLICY_FILE);
    if !policy_path.exists() {
        return json!({
            "present": false,
            "parse_error": null,
            "kind": null,
        });
    }
    let raw = match std::fs::read_to_string(&policy_path) {
        Ok(raw) => raw,
        Err(error) => {
            return json!({
                "present": true,
                "parse_error": format!("read error: {error}"),
                "kind": null,
            });
        }
    };
    match toml::from_str::<octos_agent::workspace_policy::WorkspacePolicy>(&raw) {
        Ok(policy) => json!({
            "present": true,
            "parse_error": null,
            "kind": policy.workspace.kind.as_str(),
        }),
        Err(error) => json!({
            "present": true,
            "parse_error": error.to_string(),
            "kind": null,
        }),
    }
}

fn workspace_profile_scope(profile_id: Option<&str>, session_id: &SessionKey) -> String {
    profile_id
        .or_else(|| session_id.profile_id())
        .unwrap_or(MAIN_PROFILE_ID)
        .to_owned()
}

/// Resolve the `ProfileRuntime` for the routed session, mirroring
/// `chat_sync`'s `state.profiles.get(profile_id)` lookup.
pub(crate) fn resolve_session_profile_runtime(
    state: &AppState,
    active_profile_id: Option<&str>,
) -> Option<Arc<crate::runtime::ProfileRuntime>> {
    let candidate = active_profile_id.unwrap_or(MAIN_PROFILE_ID);
    let dynamic = dynamic_profile_runtime_key(state, candidate).and_then(|key| {
        dynamic_profile_runtimes()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
    });
    dynamic.or_else(|| state.profiles.get(candidate).cloned())
}

fn dynamic_profile_runtimes() -> &'static DynamicProfileRuntimeMap {
    static RUNTIMES: OnceLock<DynamicProfileRuntimeMap> = OnceLock::new();
    RUNTIMES.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

fn dynamic_profile_runtime_key(state: &AppState, profile_id: &str) -> Option<String> {
    let store = state.profile_store.as_ref()?;
    Some(format!(
        "{}::{profile_id}",
        store.octos_home_dir().to_string_lossy()
    ))
}

/// Generation guard for the dynamic ProfileRuntime cache (#2164): the
/// post-commit Profile LLM transition bumps the generation BEFORE dropping
/// the cached runtime, so an in-flight bootstrap that read the PRE-commit
/// profile file is refused at insert time and cannot repopulate the cache
/// with a stale provider chain (mirrors `SessionRuntimeCache::generations`).
fn profile_runtime_generations() -> &'static std::sync::RwLock<HashMap<String, u64>> {
    static GENERATIONS: OnceLock<std::sync::RwLock<HashMap<String, u64>>> = OnceLock::new();
    GENERATIONS.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

fn current_profile_runtime_generation(key: &str) -> u64 {
    profile_runtime_generations()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
        .copied()
        .unwrap_or(0)
}

fn bump_profile_runtime_generation(key: &str) -> u64 {
    let mut generations = profile_runtime_generations()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = generations.entry(key.to_owned()).or_insert(0);
    *generation += 1;
    *generation
}

/// Insert `runtime` under `key` only while `generation` is still current.
/// Returns `false` — leaving the cache untouched — when a post-commit
/// invalidation bumped the generation while this bootstrap was in flight.
fn insert_profile_runtime_if_current(
    key: &str,
    generation: u64,
    runtime: Arc<crate::runtime::ProfileRuntime>,
) -> bool {
    let mut runtimes = dynamic_profile_runtimes()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if current_profile_runtime_generation(key) != generation {
        return false;
    }
    runtimes.entry(key.to_owned()).or_insert(runtime);
    true
}

pub(crate) async fn ensure_session_profile_runtime(
    state: &AppState,
    active_profile_id: Option<&str>,
) -> Result<Option<Arc<crate::runtime::ProfileRuntime>>, RpcError> {
    let profile_id = active_profile_id.unwrap_or(MAIN_PROFILE_ID);
    let Some(store) = state.profile_store.as_ref() else {
        return Ok(state.profiles.get(profile_id).cloned());
    };
    let Some(key) = dynamic_profile_runtime_key(state, profile_id) else {
        return Ok(None);
    };

    if let Some(runtime) = dynamic_profile_runtimes()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
        .cloned()
    {
        return Ok(Some(runtime));
    }
    if let Some(runtime) = state.profiles.get(profile_id) {
        return Ok(Some(runtime.clone()));
    }

    // #2164: a profile/llm select/upsert/delete that commits while this
    // bootstrap is in flight bumps the generation and must not be undone by
    // this insert — a runtime built from the PRE-commit file would silently
    // serve the old provider chain for the next turn. Capture the generation
    // up front, verify it at insert time, and on a lost race retry once from
    // the freshly committed file.
    for attempt in 0..2 {
        let generation = current_profile_runtime_generation(&key);
        let profile = store.get(profile_id).map_err(|error| {
            runtime_unavailable_error(format!("failed to read profile: {error}"))
        })?;
        let Some(profile) = profile else {
            return Ok(None);
        };
        let profile_data_dir = store.resolve_data_dir(&profile);
        // Restart recovery belongs exclusively to `octos serve` startup, which
        // scans every persisted profile before runtimes are bootstrapped. This
        // helper also runs for live cache replacement (for example
        // `profile/llm/select`), where marking active jobs abandoned would lie
        // about work still executing in this process.
        // `enabled` controls whether a profile's standalone gateway process is
        // auto-started; it must not disable authenticated AppUI/skill sessions.
        // Public BYOK profiles are intentionally created with `enabled: false`
        // so a VPS does not eagerly start one gateway per account. They still
        // need an on-demand ProfileRuntime after selecting an LLM.
        if profile.parent_id.is_some() || !profile.config.has_llm_selection() {
            return Ok(None);
        }

        // Lazily-created profiles must honour host-level policy too — without
        // host_memory, a host opt-out of (default-on) memory refresh would not
        // bind profiles created after startup.
        let runtime = crate::runtime::ProfileRuntime::bootstrap_with_host_memory(
            &profile,
            &profile_data_dir,
            Some(store.octos_home_dir()),
            crate::runtime::BootstrapRole::Serve,
            state.host_memory.as_ref(),
        )
        .await
        .map_err(|error| {
            // Lock contention is a config mistake with a concrete fix, so it gets
            // its own typed kind and a sentence the operator can act on. Anything
            // else stays `runtime_unavailable` — but formatted with `{error:#}`
            // so the eyre chain survives to the client. Plain `{error}` prints
            // only the outermost context, which is how "failed to open episode
            // store for profile 'x'" used to reach the TUI with its actual cause
            // (and its remedy) silently dropped.
            if octos_memory::is_episode_store_locked(&error) {
                data_dir_locked_error(profile_id, &error)
            } else {
                runtime_unavailable_error(format!(
                    "failed to bootstrap ProfileRuntime for profile '{profile_id}': {error:#}"
                ))
            }
        })?;
        if insert_profile_runtime_if_current(&key, generation, runtime.clone()) {
            return Ok(Some(runtime));
        }
        tracing::debug!(
            profile_id = %profile_id,
            attempt,
            "profile runtime bootstrap raced a profile/llm commit; retrying from the committed file"
        );
    }
    Err(runtime_unavailable_error(format!(
        "profile '{profile_id}' configuration changed while its runtime was bootstrapping; \
         retry the turn"
    )))
}

async fn rebuild_profile_runtime_after_skill_mutation(
    state: &Arc<AppState>,
    profile_id: &str,
) -> Result<(), RpcError> {
    let Some(current) = ensure_session_profile_runtime(state, Some(profile_id)).await? else {
        return Ok(());
    };
    // Binary plugin retirement: skill mutations only affect SKILL.md prompt
    // content, so drop the cached runtime (and bump its generation so an
    // in-flight bootstrap cannot republish the stale one) and let the next
    // `ensure_session_profile_runtime` re-bootstrap from disk.
    let Some(key) = dynamic_profile_runtime_key(state, profile_id) else {
        return Ok(());
    };
    let _ = current;
    dynamic_profile_runtimes()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&key);
    bump_profile_runtime_generation(&key);
    state.session_cache.invalidate_profile(profile_id).await;
    Ok(())
}

/// Explain why `ensure_session_profile_runtime`/`resolve_session_profile_runtime`
/// returned `None` for `profile_id`, re-deriving the same checks in the same
/// order. The 4 call sites used to collapse every cause (no profile store, no
/// such profile, sub-account, or no LLM selection) into one "Set up the profile
/// with an API key" message.
fn profile_runtime_unavailable_message(state: &AppState, profile_id: &str) -> String {
    let Some(store) = state.profile_store.as_ref() else {
        return format!(
            "No profile store is configured, so profile '{profile_id}' cannot be resolved."
        );
    };
    let profile = match store.get(profile_id) {
        Ok(Some(profile)) => profile,
        Ok(None) => return format!("Profile '{profile_id}' does not exist."),
        Err(error) => return format!("Failed to read profile '{profile_id}': {error}"),
    };
    if profile.parent_id.is_some() {
        return format!(
            "Profile '{profile_id}' is a sub-account and does not have its own runtime."
        );
    }
    if !profile.config.has_llm_selection() {
        return format!(
            "No ProfileRuntime registered for profile '{profile_id}'. \
             Set up the profile with an API key in the dashboard."
        );
    }
    format!("Profile '{profile_id}' runtime is unavailable.")
}

/// Resolve the canonical `SessionManager` handle for read operations
/// (hydrate, state, etc.). Closes #919.1: turn persistence writes to
/// the profile's `SessionRuntime.sessions`, so reads under profile
/// auth MUST hit the same handle — otherwise `state.sessions` (the
/// top-level data-dir store) reports `unknown_session` on reconnect.
///
/// Returns the per-profile session manager if a `ProfileRuntime` is
/// registered for the resolved profile and the cache can bootstrap
/// a `SessionRuntime` for `session_id`. Falls back to
/// `state.sessions` so the legacy no-profile flow continues to work.
///
/// #924 BLOCK 4: the active-profile precedence MUST mirror
/// `handle_turn_start` — `session_id.profile_id()` first (the key
/// itself encodes the owner profile), then `connection_profile_id`
/// (token-auth scope), then `routed_profile_id` (host/header
/// routing). Without the `session_id` precedence + the routed
/// fallback, a host-routed admin session on a hosted subdomain
/// hydrated from `_main`/`state.sessions` instead of the right
/// profile runtime — and turns whose `SessionKey` already carried
/// `<profile>:api:...` resolved to a different store than the one
/// that persisted them.
pub(crate) async fn resolve_sessions_for_lookup(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Option<Arc<tokio::sync::Mutex<octos_bus::SessionManager>>> {
    let active_profile_id = session_id
        .profile_id()
        .or(connection_profile_id)
        .or(routed_profile_id);
    if let Some(profile_runtime) = resolve_session_profile_runtime(state, active_profile_id) {
        let workspace_profile_id = workspace_profile_scope(active_profile_id, session_id);
        let hint = session_workspaces().runtime_hint(&workspace_profile_id, session_id);
        let permissions_epoch = state.session_cache.session_generation(session_id);
        let permissions = effective_permissions_for_session(state, session_id).ok()?;
        if let Ok(runtime) = state
            .session_cache
            .get_or_init_with_permissions(
                &profile_runtime,
                session_id.clone(),
                hint,
                permissions,
                permissions_epoch,
            )
            .await
        {
            return Some(runtime.sessions.clone());
        }
    }
    state.sessions.clone()
}

pub(crate) fn session_workspace_root_for_profile(
    profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Option<PathBuf> {
    // The cached SessionRuntime is authoritative after bootstrap. This map is
    // only the synchronous read-through view used before an async cache lookup
    // can complete, so its key must include the resolved profile as well as the
    // raw session id.
    let workspace_profile_id = workspace_profile_scope(profile_id, session_id);
    session_workspaces().get(&workspace_profile_id, session_id)
}

pub(crate) fn session_workspace_root_for_state(
    state: &AppState,
    session_id: &SessionKey,
) -> Option<PathBuf> {
    let _ = state;
    session_workspace_root_for_profile(None, session_id)
}

/// Append the per-session workspace-root hint to the system prompt.
///
/// M11-F: the base prompt comes from the SessionRuntime's agent only
/// (legacy `state.agent` was deleted), so this helper takes the
/// resolved `String` rather than an `Agent` reference. The text
/// appended is identical to the pre-M11-E `session_system_prompt`
/// wording — the SPA's reducer matches on it heuristically and must
/// not change.
fn append_workspace_root_hint(mut prompt: String, workspace_root: Option<&Path>) -> String {
    if let Some(workspace_root) = workspace_root {
        prompt.push_str("\n\nAppUi session workspace root: ");
        prompt.push_str(&workspace_root.to_string_lossy());
        prompt.push_str(
            "\nThe server approved this cwd for the current session. Resolve relative shell and file-tool paths against this workspace.",
        );
    }
    prompt
}

const MAX_PANE_WORKSPACE_ENTRIES: usize = 200;
const MAX_PANE_ARTIFACT_ITEMS: usize = 80;
const MAX_PANE_GIT_HISTORY: usize = 12;

fn build_pane_snapshot(
    data_dir: &Path,
    session_id: &SessionKey,
    workspace_root: Option<&Path>,
) -> UiPaneSnapshot {
    let workspace_dirs = ui_protocol_session_workspace_dirs(data_dir, session_id, workspace_root);
    let mut limitations = Vec::new();
    let workspace = build_workspace_pane_snapshot(&workspace_dirs, &mut limitations);
    let artifacts = build_artifact_pane_snapshot(&workspace_dirs);
    let git = build_git_pane_snapshot(&workspace_dirs);

    UiPaneSnapshot {
        session_id: session_id.clone(),
        generated_at: Some(Utc::now()),
        workspace: Some(workspace),
        artifacts: Some(artifacts),
        git: Some(git),
        limitations,
    }
}

fn build_workspace_pane_snapshot(
    workspace_dirs: &[PathBuf],
    limitations: &mut Vec<UiPaneSnapshotLimitation>,
) -> UiWorkspacePaneSnapshot {
    let root = workspace_dirs
        .iter()
        .find(|path| path.exists())
        .or_else(|| workspace_dirs.first())
        .cloned()
        .unwrap_or_default();

    let mut entries = Vec::new();
    let mut truncated = false;
    if root.exists() {
        collect_workspace_entries(&root, &root, &mut entries, &mut truncated);
    } else {
        limitations.push(UiPaneSnapshotLimitation {
            code: "workspace_missing".into(),
            message: format!("workspace root does not exist: {}", root.display()),
        });
    }

    let mut workspace_limitations = Vec::new();
    if truncated {
        workspace_limitations.push(UiPaneSnapshotLimitation {
            code: "workspace_truncated".into(),
            message: format!("workspace tree limited to {MAX_PANE_WORKSPACE_ENTRIES} entries"),
        });
    }

    let root = root.to_string_lossy().to_string();
    UiWorkspacePaneSnapshot {
        root: root.clone(),
        readable_roots: vec![root.clone()],
        writable_roots: vec![root],
        contract: vec![
            "api octos-app-ui/v1alpha1".into(),
            "source session/open panes".into(),
            "feature pane.snapshots.v1".into(),
        ],
        entries,
        limitations: workspace_limitations,
    }
}

fn collect_workspace_entries(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<UiWorkspacePaneEntry>,
    truncated: &mut bool,
) {
    if entries.len() >= MAX_PANE_WORKSPACE_ENTRIES {
        *truncated = true;
        return;
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children = read_dir.flatten().collect::<Vec<_>>();
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        if entries.len() >= MAX_PANE_WORKSPACE_ENTRIES {
            *truncated = true;
            return;
        }

        let path = child.path();
        let file_name = child.file_name();
        let label = file_name.to_string_lossy().to_string();
        if should_skip_pane_dir(&label) {
            continue;
        }

        let Ok(metadata) = child.metadata() else {
            continue;
        };
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let relative_path = relative.to_string_lossy().to_string();
        let depth = relative.components().count().saturating_sub(1);
        let (kind, detail) = if metadata.is_dir() {
            ("directory", Some("dir".into()))
        } else if metadata.is_file() {
            ("file", Some(format_size(metadata.len())))
        } else if metadata.file_type().is_symlink() {
            ("symlink", None)
        } else {
            ("other", None)
        };

        entries.push(UiWorkspacePaneEntry {
            path: relative_path,
            label,
            depth,
            kind: kind.into(),
            detail,
        });

        if metadata.is_dir() {
            collect_workspace_entries(root, &path, entries, truncated);
        }
    }
}

fn build_artifact_pane_snapshot(workspace_dirs: &[PathBuf]) -> UiArtifactPaneSnapshot {
    let mut artifacts = Vec::new();
    for root in workspace_dirs.iter().filter(|path| path.exists()) {
        collect_artifact_items(root, root, &mut artifacts);
        if artifacts.len() >= MAX_PANE_ARTIFACT_ITEMS {
            break;
        }
    }

    artifacts.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.title.cmp(&right.1.title))
    });
    artifacts.truncate(MAX_PANE_ARTIFACT_ITEMS);

    let items = artifacts.into_iter().map(|(_, item)| item).collect();
    UiArtifactPaneSnapshot {
        items,
        limitations: Vec::new(),
    }
}

fn collect_artifact_items(
    root: &Path,
    dir: &Path,
    artifacts: &mut Vec<(std::time::SystemTime, UiArtifactPaneItem)>,
) {
    if artifacts.len() >= MAX_PANE_ARTIFACT_ITEMS {
        return;
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for child in read_dir.flatten() {
        if artifacts.len() >= MAX_PANE_ARTIFACT_ITEMS {
            return;
        }

        let path = child.path();
        let label = child.file_name().to_string_lossy().to_string();
        if should_skip_pane_dir(&label) {
            continue;
        }

        let Ok(metadata) = child.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_artifact_items(root, &path, artifacts);
            continue;
        }
        if !metadata.is_file() {
            continue;
        }

        let modified = metadata
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let updated_at = Some(chrono::DateTime::<Utc>::from(modified));
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let relative_path = relative.to_string_lossy().to_string();
        artifacts.push((
            modified,
            UiArtifactPaneItem {
                title: label,
                kind: "file".into(),
                path: Some(relative_path.clone()),
                uri: Some(relative_path),
                source: Some("workspace".into()),
                status: format_size(metadata.len()),
                source_task_id: None,
                preview_id: None,
                size_bytes: Some(metadata.len()),
                updated_at,
            },
        ));
    }
}

fn build_git_pane_snapshot(workspace_dirs: &[PathBuf]) -> UiGitPaneSnapshot {
    let Some(repo_root) = workspace_dirs
        .iter()
        .filter(|path| path.exists())
        .find_map(|path| git_repo_root(path))
    else {
        return UiGitPaneSnapshot {
            repo_root: None,
            branch: None,
            head: None,
            clean: true,
            status: Vec::new(),
            history: Vec::new(),
            limitations: vec![UiPaneSnapshotLimitation {
                code: "git_unavailable".into(),
                message: "no git repository found for session workspace".into(),
            }],
        };
    };

    let branch = git_output(&repo_root, ["branch", "--show-current"]);
    let head = git_output(&repo_root, ["rev-parse", "--short", "HEAD"]);
    let status_output = git_output(&repo_root, ["status", "--porcelain=v1"]).unwrap_or_default();
    let status = status_output
        .lines()
        .filter_map(parse_git_status_line)
        .collect::<Vec<_>>();
    let history_limit = MAX_PANE_GIT_HISTORY.to_string();
    let history_output = git_output(
        &repo_root,
        ["log", "--oneline", "-n", history_limit.as_str()],
    )
    .unwrap_or_default();
    let history = history_output
        .lines()
        .filter_map(parse_git_history_line)
        .collect::<Vec<_>>();

    UiGitPaneSnapshot {
        repo_root: Some(repo_root.to_string_lossy().to_string()),
        branch,
        head,
        clean: status.is_empty(),
        status,
        history,
        limitations: Vec::new(),
    }
}

fn git_repo_root(path: &Path) -> Option<PathBuf> {
    git_output(path, ["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

fn git_output<const N: usize>(repo_root: &Path, args: [&str; N]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn parse_git_status_line(line: &str) -> Option<UiGitStatusItem> {
    let code = line.get(0..2)?.trim().to_string();
    let path = line.get(3..)?.trim().to_string();
    if path.is_empty() {
        return None;
    }

    Some(UiGitStatusItem {
        detail: git_status_detail(&code).into(),
        code: if code.is_empty() { "?".into() } else { code },
        path,
    })
}

fn git_status_detail(code: &str) -> &'static str {
    match code {
        "M" | "MM" | "AM" | "A M" | " M" | "M " => "modified",
        "A" | "A " => "added",
        "D" | " D" | "D " => "deleted",
        "R" | "R " => "renamed",
        "??" => "untracked",
        _ => "changed",
    }
}

fn parse_git_history_line(line: &str) -> Option<UiGitHistoryItem> {
    let (commit, summary) = line.split_once(' ')?;
    Some(UiGitHistoryItem {
        commit: commit.into(),
        summary: summary.into(),
    })
}

fn should_skip_pane_dir(label: &str) -> bool {
    matches!(label, ".git" | "target" | "node_modules")
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn ui_protocol_session_workspace_dirs(
    data_dir: &Path,
    session_id: &SessionKey,
    workspace_root: Option<&Path>,
) -> Vec<PathBuf> {
    let profile_id = infer_profile_id_from_data_dir(data_dir);
    let mut dirs = Vec::with_capacity(4);
    let mut seen = HashSet::new();

    if let Some(workspace_root) = workspace_root {
        let path = workspace_root.to_path_buf();
        if seen.insert(path.clone()) {
            dirs.push(path);
        }
    }

    for key in [
        session_id.clone(),
        SessionKey::with_profile(&profile_id, session_id.channel(), session_id.chat_id()),
        SessionKey::with_profile(MAIN_PROFILE_ID, session_id.channel(), session_id.chat_id()),
        SessionKey::new(session_id.channel(), session_id.chat_id()),
    ] {
        let encoded_base = octos_bus::session::encode_path_component(key.base_key());
        let path = data_dir.join("users").join(encoded_base).join("workspace");
        if seen.insert(path.clone()) {
            dirs.push(path);
        }
    }

    dirs
}

fn infer_profile_id_from_data_dir(data_dir: &Path) -> String {
    data_dir
        .file_name()
        .and_then(|name| (name == "data").then_some(data_dir))
        .and_then(|_| data_dir.parent())
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(MAIN_PROFILE_ID)
        .to_string()
}

// Turn-start threads the full dispatch context (connection, stores, turn
// registries, caller and routed profile scope, negotiated features) straight
// through to `handle_turn_start_with_accept`; the wide signature is the
// adapter boundary, not a missing struct.
#[allow(clippy::too_many_arguments)]
async fn handle_turn_start(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    active_turns: &SharedActiveTurns,
    connection_turns: &SharedConnectionTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: TurnStartParams,
) {
    let _ = handle_turn_start_with_accept(
        ws,
        state,
        ledger,
        contracts,
        active_turns,
        connection_turns,
        connection_profile_id,
        routed_profile_id,
        features,
        id,
        params,
        json!({ "accepted": true }),
    )
    .await;
}

/// `handle_turn_start` body with a caller-chosen accept payload.
///
/// The lifecycle (admission → registry insert → accept reply → `start_tx`)
/// is shared verbatim between `turn/start` (accept = `{"accepted": true}`)
/// and the `turn/steer` no-active-turn fallback (accept =
/// `{"turn_id": ..., "steered": false}`) — codex parity: `Op::UserInput`
/// falls back from `steer_input` to `spawn_task(RegularTask)` through the
/// SAME submission path (`handlers.rs:220-266`); only the RPC result shape
/// differs on the app-server surface.
#[allow(clippy::too_many_arguments)]
async fn handle_turn_start_with_accept(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    active_turns: &SharedActiveTurns,
    connection_turns: &SharedConnectionTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    mut params: TurnStartParams,
    accept_result: Value,
) -> bool {
    // UPCR-2026-015 (M9-β-1): if the client carried a `topic` field
    // alongside the session_id, fold it into the resolved SessionKey
    // BEFORE scope validation. The rest of the turn pipeline keys
    // exclusively off `params.session_id`, so adopting the topic-
    // suffixed form here means history lookup, ledger appends, and
    // `task/list` filtering all see the per-topic bucket
    // automatically. Empty / whitespace-only topics fall through to
    // the bare session shape (matching `SessionKey::with_topic`'s
    // own empty-string short-circuit).
    if let Some(topic) = params
        .topic
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        // Replace the SessionKey with the topic-suffixed form. Splice
        // any existing topic suffix away first so a client that sends
        // both `session_id: "x:y#old"` and `topic: "new"` lands in a
        // single, unambiguous bucket (`x:y#new`) rather than the
        // double-suffixed garbage `x:y#old#new`. The base parser
        // already handles `#`-stripped lookups, but we want the
        // canonical form on the wire-trip back to clients.
        let base = params.session_id.base_key().to_owned();
        params.session_id = SessionKey(format!("{base}#{topic}"));
    }

    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return false;
    }

    let prompt = match prompt_text(&params.input) {
        Some(p) => p,
        None => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::invalid_params("turn/start requires at least one text input item"),
            );
            return false;
        }
    };

    let fixture = m9_protocol_fixture_for_prompt(&prompt);
    if fixture.is_none() {
        // M11-F: validate that a `ProfileRuntime` is registered for the
        // routed profile BEFORE spawning the turn task. The legacy
        // `validate_runtime` (which checked `state.agent` /
        // `state.sessions`) was deleted; the equivalent gate now is
        // "the SessionRuntimeCache can resolve a ProfileRuntime for
        // this session's profile id". Fail fast with the same
        // `runtime_unavailable` shape so existing clients see no wire
        // change.
        let active_profile_id = params
            .session_id
            .profile_id()
            .map(ToOwned::to_owned)
            .or_else(|| {
                connection_profile_id
                    .or(routed_profile_id)
                    .map(ToOwned::to_owned)
            });
        if let Some(profile_id) = active_profile_id.as_deref() {
            if let Err(error) = ensure_known_profile(state, profile_id) {
                let _ = send_rpc_error(ws, Some(id), error);
                return false;
            }
        }
        let Some(profile_runtime) =
            resolve_session_profile_runtime(state, active_profile_id.as_deref())
        else {
            let _ = send_rpc_error(
                ws,
                Some(id),
                runtime_unavailable_error(profile_runtime_unavailable_message(
                    state,
                    active_profile_id.as_deref().unwrap_or("<unset>"),
                )),
            );
            return false;
        };
        if let Err(error) = require_recovered_scoped_session_open(
            state,
            ledger,
            &params.session_id,
            &profile_runtime.profile_id,
        ) {
            let _ = send_rpc_error(ws, Some(id), error);
            return false;
        }
    }

    let ws_for_turn = ws.clone();
    let state_for_turn = state.clone();
    let ledger_for_turn = ledger.clone();
    let contracts_for_turn = contracts.clone();
    let session_id = params.session_id.clone();
    let turn_id = params.turn_id.clone();
    let turn_state = Arc::new(TokioMutex::new(TurnState::Active));
    let (interrupt_tx, interrupt_rx) = mpsc::channel::<()>(1);
    let interrupt_tx = Arc::new(TokioMutex::new(Some(interrupt_tx)));
    let turn_state_for_task = turn_state.clone();
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    // `turn/steer` pending-input buffer (codex `TurnState.pending_input`).
    // Regular standalone turns are steerable; M9 protocol fixture turns are
    // not (they never run the agent loop, so a pushed input would silently
    // vanish — advertise that by registering no buffer).
    let steer_buffer: Option<octos_agent::SharedSteerBuffer> = fixture
        .is_none()
        .then(|| Arc::new(octos_agent::SteerBuffer::default()));
    let steer_buffer_for_turn = steer_buffer.clone();
    let resolved_profile_id = connection_profile_id
        .or(routed_profile_id)
        .map(ToOwned::to_owned);
    // Stamp for the ActiveTurn registry: mirror `session/btw`'s resolution
    // (session key first) so the aside's profile check compares like with
    // like; canonical `_main` when nothing resolves.
    let profile_for_stamp = session_id
        .profile_id()
        .map(ToOwned::to_owned)
        .or_else(|| resolved_profile_id.clone())
        .unwrap_or_else(|| MAIN_PROFILE_ID.to_owned());
    let handle = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        {
            run_standalone_turn(
                ws_for_turn,
                state_for_turn,
                ledger_for_turn,
                contracts_for_turn,
                features,
                params,
                prompt,
                resolved_profile_id,
                turn_state_for_task,
                interrupt_rx,
                steer_buffer_for_turn,
                None,
                // OLP-CTRL 回合 4 — an interactive turn is never a steer
                // continuation turn; it must not consume reviewer-notes.
                false,
                None,
            )
            .await;
        }
    });

    let inserted = {
        let mut active = active_turns.lock().await;
        // Allow replacing a `Terminal(_)` entry — the prior turn is finished;
        // we keep the entry only so a follow-up `turn/interrupt` can return
        // `terminal_state` instead of `unknown_turn`. Any non-terminal entry
        // means there is still a turn running for this session.
        let occupied = match active.get(&session_id) {
            Some(existing) => {
                let existing_state = existing.state.lock().await;
                !matches!(*existing_state, TurnState::Terminal(_))
            }
            None => false,
        };
        if occupied {
            false
        } else {
            // Client-supplied turn ids carry no uniqueness guarantee — a
            // reused id must not inherit a prior turn's `session/btw` draft.
            btw_live_draft_clear(&session_id, &turn_id);
            active.insert(
                session_id.clone(),
                ActiveTurn {
                    turn_id: turn_id.clone(),
                    profile_id: profile_for_stamp.clone(),
                    state: turn_state.clone(),
                    interrupt_tx,
                    steer: steer_buffer,
                    abort: handle.abort_handle(),
                },
            );
            true
        }
    };
    if !inserted {
        handle.abort();
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_request("a turn is already running for this session"),
        );
        return false;
    }

    connection_turns.lock().await.insert(
        session_id.clone(),
        ConnectionTurn {
            turn_id: turn_id.clone(),
            state: turn_state.clone(),
        },
    );
    // Lifecycle reply: if the client cannot receive the accept, abort the
    // freshly-inserted turn — running an unaccepted turn would be a leak.
    if send_rpc_result(ws, id, accept_result).is_err() {
        handle.abort();
        let mut active = active_turns.lock().await;
        if active
            .get(&session_id)
            .is_some_and(|entry| entry.turn_id == turn_id && Arc::ptr_eq(&entry.state, &turn_state))
        {
            active.remove(&session_id);
        }
        drop(active);
        let mut connection = connection_turns.lock().await;
        if connection.get(&session_id).is_some_and(|registered| {
            registered.turn_id == turn_id && Arc::ptr_eq(&registered.state, &turn_state)
        }) {
            connection.remove(&session_id);
        }
        return false;
    }
    let _ = start_tx.send(());
    true
}

/// Snapshot of sessions that currently have an in-flight (non-terminal) turn in
/// the process-global active-turns registry. One lock acquisition; used to feed
/// the whole-job orchestration status without re-locking per session.
async fn active_turn_sessions(
    active_turns: &SharedActiveTurns,
) -> std::collections::HashSet<SessionKey> {
    let mut sessions = std::collections::HashSet::new();
    let active = active_turns.lock().await;
    for (session_id, turn) in active.iter() {
        if !matches!(&*turn.state.lock().await, TurnState::Terminal(_)) {
            sessions.insert(session_id.clone());
        }
    }
    sessions
}

/// #2019 — bound on the human-sink queue. Producers (`try_send`) never block:
/// a full queue drops the event, bumps a metric, and logs. Generous enough
/// that only a genuinely pathological burst reaches it, and the per-origin cap
/// in `autonomy::human_events` already emits a VISIBLE marker long before.
const BACKGROUND_ACTIVITY_QUEUE_CAPACITY: usize = 512;

/// #2019 — install the process-global HUMAN sink for background events that
/// today only wake the model, and spawn its drain task.
///
/// Monitor event lines (#1977) and claimed fleet outbox events already exist,
/// are already durable, and already have producers — with exactly ONE
/// consumer, the model. This adds the second consumer, the user, WITHOUT
/// touching how or when the model is woken.
///
/// Shape:
/// - Producers call the sync, non-blocking
///   [`crate::autonomy::human_events::emit_background_activity`], which caps
///   per origin and hands the event to the installed sink.
/// - The sink is a bounded-channel `try_send`. A full queue DROPS and logs; it
///   must never stall a watcher task or the outbox consumer.
/// - The drain task appends each event to the durable per-session ledger via
///   [`send_notification_durable`] over a DETACHED connection, mirroring
///   `PeerStaged`. The append (ring + disk + `publish_live`) is
///   connection-independent, so a client disconnected mid-loop replays the
///   stream by cursor on reconnect instead of losing its middle; connected
///   clients receive it on their session's live forwarder, which applies their
///   own `event.background_activity.v1` capability filter.
///
/// Nothing here is routed back into model context.
pub(crate) fn spawn_background_activity_sink(state: Arc<AppState>) {
    let (_tx, mut rx) = mpsc::channel::<octos_core::ui_protocol::BackgroundActivityEvent>(
        BACKGROUND_ACTIVITY_QUEUE_CAPACITY,
    );
    tokio::spawn(async move {
        // Detached connection: there is no live peer. Outbound frames are
        // discarded by a drain task (the durable record is the ledger); keep
        // the receiver alive so sends never backpressure-fail.
        let (writer_tx, mut writer_rx) = mpsc::channel::<WsMessage>(WS_WRITER_CHANNEL_CAPACITY);
        tokio::spawn(async move { while writer_rx.recv().await.is_some() {} });
        let ws = WsConnection::new(writer_tx);
        let ledger = event_ledger(&state).await;
        info!("background activity human sink started (#2019)");
        while let Some(event) = rx.recv().await {
            let _ =
                send_notification_durable(&ws, &ledger, UiNotification::BackgroundActivity(event));
        }
    });
}

async fn handle_turn_interrupt(
    ws: &WsConnection,
    _ledger: &Arc<UiProtocolLedger>,
    active_turns: &SharedActiveTurns,
    // FIX-06 + FIX-08: kept on the signature so callers don't need to know
    // whether this handler currently evicts scopes / drains approvals itself.
    // The actual eviction + pending-approval cancel happens when
    // `run_standalone_turn` observes the interrupt: it calls
    // `cancel_pending_for_turn` (FIX-08) before `try_emit_terminal`
    // (FIX-03) and `evict_turn` (FIX-06) on exit. Centralising both there
    // guarantees a single happens-before edge: agent abort → cancel
    // notifications → terminal `turn/error code=interrupted`, all on the
    // same task that owned the turn.
    _contracts: &Arc<UiProtocolContractStores>,
    id: String,
    params: TurnInterruptParams,
) {
    // task-turn-interrupt-steer-correlation-logs: make the interrupt's
    // receipt, decision and ack reconstructible from the log alone.
    crate::turn_trace::log_interrupt_received(&params.session_id, &params.turn_id);
    let outcome = decide_interrupt(active_turns, &params).await;
    let outcome_label: String = match &outcome {
        InterruptOutcome::Unknown => "unknown".into(),
        InterruptOutcome::Mismatch => "mismatch".into(),
        InterruptOutcome::AlreadyTerminal(reason) => {
            format!("already_terminal:{}", reason.as_str())
        }
        InterruptOutcome::AlreadyInterrupting => "already_interrupting".into(),
        InterruptOutcome::Captured { .. } => "captured".into(),
    };
    crate::turn_trace::log_interrupt_outcome(&params.session_id, &params.turn_id, &outcome_label);
    match outcome {
        InterruptOutcome::Unknown => {
            let _ = send_rpc_error(ws, Some(id), unknown_turn_error(&params.turn_id));
        }
        InterruptOutcome::Mismatch => {
            // Codified by accepted UPCR-2026-008: typed `reason` field on
            // `TurnInterruptResult`. String registry value `turn_id_mismatch`.
            let _ = send_typed_interrupt_result(
                ws,
                id,
                TurnInterruptResult::declined("turn_id_mismatch"),
            );
        }
        InterruptOutcome::AlreadyTerminal(reason) => {
            let interrupted = matches!(reason, TerminalReason::Interrupted);
            // Codified by accepted UPCR-2026-008: typed `terminal_state` field
            // on `TurnInterruptResult`. Values come from `TerminalReason`.
            let _ = send_typed_interrupt_result(
                ws,
                id,
                TurnInterruptResult::already_terminal(reason.as_str(), interrupted),
            );
        }
        InterruptOutcome::AlreadyInterrupting => {
            // A prior caller transitioned the turn to `Interrupting` and is
            // awaiting ack. The terminal event is already guaranteed to be
            // emitted exactly once. Idempotent: report the same response shape
            // as the original caller will.
            let _ = send_typed_interrupt_result(ws, id, TurnInterruptResult::interrupted_ok());
        }
        InterruptOutcome::Captured { ack_rx } => {
            // State is now `Interrupting { ack }`; the turn task is wired to
            // observe `interrupt_rx`, abort its agent, emit exactly one
            // `TurnError(interrupted)`, and signal `ack`. We do NOT abort the
            // outer turn future here — that would race with the terminal
            // emission and could lose the wire-side event.
            let result = tokio::time::timeout(INTERRUPT_ACK_TIMEOUT, ack_rx).await;
            crate::turn_trace::log_interrupt_ack(
                &params.session_id,
                &params.turn_id,
                if matches!(result, Ok(Ok(()))) {
                    "interrupted"
                } else {
                    "ack_timed_out"
                },
            );
            let payload = match result {
                Ok(Ok(())) => TurnInterruptResult::interrupted_ok(),
                Ok(Err(_)) => {
                    // Sender dropped without ack — the task panicked or was
                    // cancelled before reaching the terminal arm. The state
                    // remains `Interrupting`; report timeout-style result so
                    // the caller knows the wire-side terminal is uncertain.
                    // Codified by accepted UPCR-2026-008.
                    TurnInterruptResult::ack_timed_out()
                }
                Err(_) => TurnInterruptResult::ack_timed_out(),
            };
            let _ = send_typed_interrupt_result(ws, id, payload);
        }
    }
}

/// Serialize a typed `TurnInterruptResult` and dispatch via `send_rpc_result`.
///
/// Falls back to a hand-built minimal result if serialization fails. The
/// fallback path should be unreachable in practice — `TurnInterruptResult`
/// has no field that can fail to serialize — but keeping the call infallible
/// on the wire avoids leaving the caller without a response on a defensive
/// path.
fn send_typed_interrupt_result(
    ws: &WsConnection,
    id: String,
    result: TurnInterruptResult,
) -> Result<(), SendError> {
    let value = serde_json::to_value(&result)
        .unwrap_or_else(|_| json!({ "interrupted": result.interrupted }));
    send_rpc_result(ws, id, value)
}

#[derive(Debug)]
enum InterruptOutcome {
    Unknown,
    Mismatch,
    AlreadyTerminal(TerminalReason),
    AlreadyInterrupting,
    Captured { ack_rx: oneshot::Receiver<()> },
}

async fn decide_interrupt(
    active_turns: &SharedActiveTurns,
    params: &TurnInterruptParams,
) -> InterruptOutcome {
    let registry = active_turns.lock().await;
    let Some(active) = registry.get(&params.session_id) else {
        return InterruptOutcome::Unknown;
    };
    if active.turn_id != params.turn_id {
        return InterruptOutcome::Mismatch;
    }

    let state_arc = active.state.clone();
    let interrupt_tx_arc = active.interrupt_tx.clone();
    drop(registry);
    capture_turn_interrupt(state_arc, interrupt_tx_arc, InterruptOrigin::Client).await
}

/// The interrupt CAPTURE routine shared by `turn/interrupt` and the #1842(a)
/// peer-close abort.
///
/// The lock boundary: hold the per-turn state mutex across the read and the
/// write. This is what closes the original TOCTOU window — natural completion
/// inside `run_standalone_turn` is gated on this same mutex via
/// `try_emit_terminal`, so the two paths can't both transition `Active` → a
/// terminal state.
/// The origin captured when this turn was interrupted.
///
/// Read BEFORE `try_emit_terminal`, which takes the same state lock. Defaults
/// to `Client` for any non-`Interrupting` state: the only callers are on the
/// interrupt path, so a race that already moved the state is reported as the
/// common case rather than mislabelled `PeerClose`.
async fn captured_interrupt_origin(state: &Arc<TokioMutex<TurnState>>) -> InterruptOrigin {
    match &*state.lock().await {
        TurnState::Interrupting { origin, .. } => *origin,
        _ => InterruptOrigin::Client,
    }
}

async fn capture_turn_interrupt(
    state_arc: Arc<TokioMutex<TurnState>>,
    interrupt_tx_arc: Arc<TokioMutex<Option<mpsc::Sender<()>>>>,
    origin: InterruptOrigin,
) -> InterruptOutcome {
    let mut state = state_arc.lock().await;
    match &*state {
        TurnState::Terminal(reason) => InterruptOutcome::AlreadyTerminal(*reason),
        TurnState::Interrupting { .. } => InterruptOutcome::AlreadyInterrupting,
        TurnState::Active => {
            let (ack_tx, ack_rx) = oneshot::channel();
            *state = TurnState::Interrupting {
                ack: ack_tx,
                origin,
            };
            drop(state);
            // Best-effort signal — capacity-1 channel; sending fails only if
            // the receiver has already been dropped (turn task is gone). Even
            // if the signal is lost, the state is already `Interrupting`, and
            // the next progress event in the task loop checks the state.
            let interrupt_tx = interrupt_tx_arc.lock().await.take();
            if let Some(tx) = interrupt_tx {
                let _ = tx.try_send(());
            }
            InterruptOutcome::Captured { ack_rx }
        }
    }
}

fn unknown_turn_error(turn_id: &TurnId) -> RpcError {
    let turn_id_str = turn_id.0.to_string();
    RpcError::new(UNKNOWN_TURN_CODE, format!("unknown turn: {turn_id_str}"))
        .with_data(json!({ "turn_id": turn_id_str, "kind": "unknown_turn" }))
}

/// Publish the canonical `approval/decided` durable notification + decision
/// trace. SHARED by the `approval/respond` RPC handler and `peer_respond` (#P1-5)
/// so a master-driven decision is recorded on the wire identically to a
/// client-driven one. Sync (no await) so the RPC path can call it BEFORE the
/// data_dir lock — the decided event must append to the ledger before the woken
/// turn can publish `turn/completed`.
fn emit_approval_decided(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    event: &ApprovalDecidedEvent,
    tool_name: Option<&str>,
) {
    log_decision_tracing(event, tool_name);
    let _ = send_notification_durable(ws, ledger, UiNotification::ApprovalDecided(event.clone()));
}

/// Append the approval decision to the durable audit log. SHARED by the
/// `approval/respond` RPC handler and `peer_respond` (#P1-5). Sync given a
/// pre-resolved `data_dir` (the RPC path resolves it via the sessions lock; the
/// peer path captures it at wiring time).
fn audit_approval_decided(
    contracts: &UiProtocolContractStores,
    data_dir: &Path,
    event: &ApprovalDecidedEvent,
    tool_name: Option<&str>,
) {
    let audit = contracts.audit_log(data_dir);
    if let Err(error) = audit.record(event, tool_name) {
        tracing::warn!(
            target: "octos.approvals.decision",
            approval_id = %event.approval_id.0,
            error = %error,
            "failed to append approval audit log entry"
        );
    }
}

async fn handle_approval_respond(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    contracts: &Arc<UiProtocolContractStores>,
    connection_profile_id: Option<&str>,
    id: String,
    params: octos_core::ui_protocol::ApprovalRespondParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }

    let session_id = params.session_id.clone();
    let scope_string = params.approval_scope.clone();
    // FIX-01: `ApprovalDecision` is non-Copy because of the `Unknown(String)`
    // variant; clone to keep the value alive across `respond_with_context`
    // (consumes `params` via clone), the scope-recording call below, and the
    // FIX-07 audit/notification emission.
    let decision = params.decision.clone();

    let outcome = match contracts.approvals.respond_with_context(params.clone()) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };

    // FIX-07: publish the canonical durable decision immediately after the
    // store accepts the response. `respond_with_context` wakes the waiting
    // turn; any await before this append can let that turn publish
    // `turn/completed` first.
    let tool_name = outcome.context.as_ref().map(|ctx| ctx.tool_name.clone());
    let event = super::ui_protocol_approvals::build_decided_event(
        &params,
        &outcome,
        connection_profile_id.unwrap_or(""),
        Utc::now(),
    );
    emit_approval_decided(ws, ledger, &event, tool_name.as_deref());

    // FIX-06: if the user picked a recordable scope and we have the original
    // request context, register the policy entry. Open-registry rule:
    // unknown scope strings collapse to `approve_once` and are not recorded
    // — preserving backward compat with clients that send future scope
    // tokens we don't yet recognise.
    if let (Some(scope_string), Some(context)) = (scope_string.as_deref(), outcome.context.as_ref())
    {
        let scope_kind = ApprovalScopeKind::from_scope_str(scope_string);
        if scope_kind.is_recordable() {
            let match_key = match_key_for(scope_kind, &context.tool_name, &context.turn_id);
            contracts
                .scopes
                .record(&session_id, scope_kind, match_key, decision);
        }
    }

    let result = match serde_json::to_value(&outcome.result) {
        Ok(value) => value,
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!(
                    "failed to serialize approval/respond result: {error}"
                )),
            );
            return;
        }
    };
    let _ = send_rpc_result(ws, id, result);

    if let Some(sessions) = state.sessions.as_ref() {
        let data_dir = sessions.lock().await.data_dir();
        audit_approval_decided(contracts, &data_dir, &event, tool_name.as_deref());
    }
}

/// UPCR-2026-023 `user_question/respond` handler. Mirrors
/// [`handle_approval_respond`]: validate session scope, resolve the pending
/// question's oneshot (typed `user_question_unknown` / `user_question_stale`
/// on miss), and return the ack result.
async fn handle_user_question_respond(
    ws: &WsConnection,
    contracts: &Arc<UiProtocolContractStores>,
    connection_profile_id: Option<&str>,
    id: String,
    params: UserQuestionRespondParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }

    let outcome = match contracts.user_questions.respond_with_context(&params) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };

    let result = match serde_json::to_value(&outcome.result) {
        Ok(value) => value,
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!(
                    "failed to serialize user_question/respond result: {error}"
                )),
            );
            return;
        }
    };
    let _ = send_rpc_result(ws, id, result);
}

async fn handle_approval_scopes_list(
    ws: &WsConnection,
    scopes: &ScopePolicy,
    connection_profile_id: Option<&str>,
    id: String,
    params: octos_core::ui_protocol::ApprovalScopesListParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }

    let result = octos_core::ui_protocol::ApprovalScopesListResult {
        scopes: scopes.list_for_session(&params.session_id),
    };
    match serde_json::to_value(result) {
        Ok(result) => {
            let _ = send_rpc_result(ws, id, result);
        }
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!(
                    "failed to serialize approval/scopes/list result: {error}"
                )),
            );
        }
    }
}

// ----- UPCR-2026-009 / -010 / -011 handlers -----

/// Recover the identity of a committed row after the legacy flat/per-user
/// merge changes its display index. The ledger's persisted timestamp and
/// typed owner are provenance; content/media only verify that provenance and
/// are never used to search for an equal-looking message. Both directions of
/// the mapping must be unique. Missing or conflicting evidence stays unmapped.
///
/// An exact existing ID remains compatible with historical producers whose
/// background row uses the explicitly recorded response-to client message as
/// its thread. Even a matching position-derived ID cannot override a typed
/// owner contradiction; rebinding requires the exact persisted parent/thread.
fn hydrated_canonical_message_identities(
    session_id: &SessionKey,
    messages: &[Message],
    envelopes: &[octos_core::ui_protocol::EnvelopeV2Notification],
) -> HashMap<usize, (String, bool)> {
    if envelopes.is_empty() {
        return HashMap::new();
    }
    struct Identity<'a> {
        message_id: &'a str,
        persisted_at: chrono::DateTime<Utc>,
        owner: &'a str,
        content: &'a str,
        media: &'a [String],
        background: bool,
    }
    fn identity(envelope: &EnvelopeV2) -> Option<Identity<'_>> {
        match &envelope.payload {
            PayloadV2::AssistantPersisted { text, meta, .. } => Some(Identity {
                message_id: &meta.message_id,
                persisted_at: meta.persisted_at,
                owner: &envelope.thread_id,
                content: text,
                media: &meta.media,
                background: false,
            }),
            PayloadV2::BackgroundChildCompleted {
                message_id,
                persisted_at,
                parent_turn_id,
                content,
                media,
                ..
            } => Some(Identity {
                message_id,
                persisted_at: *persisted_at,
                owner: parent_turn_id,
                content,
                media,
                background: true,
            }),
            _ => None,
        }
    }

    let mut current_ids = HashMap::new();
    let mut owners = HashMap::new();
    for (seq, message) in messages.iter().enumerate() {
        current_ids.insert(
            format!(
                "{}:{seq}:{}",
                session_id.0,
                message.timestamp.timestamp_nanos_opt().unwrap_or(0),
            ),
            seq,
        );
        if let Some(owner) = message.thread_id.as_deref() {
            owners
                .entry((message.timestamp, owner))
                .and_modify(|row| *row = None)
                .or_insert(Some(seq));
        }
    }

    // Replayed copies of the same typed record are harmless. A reused ID with
    // different ownership/payload is not authority for either candidate row.
    let mut references: HashMap<&str, Option<&EnvelopeV2>> = HashMap::new();
    for notification in envelopes {
        if notification.session_id != *session_id {
            continue;
        }
        let envelope = &notification.envelope;
        let Some(Identity { message_id, .. }) = identity(envelope) else {
            continue;
        };
        if message_id.is_empty() {
            continue;
        }
        references
            .entry(message_id)
            .and_modify(|previous| {
                if previous.is_some_and(|previous| {
                    previous.thread_id != envelope.thread_id || previous.payload != envelope.payload
                }) {
                    *previous = None;
                }
            })
            .or_insert(Some(envelope));
    }

    let mut resolved = HashMap::new();
    for envelope in references.into_values().flatten() {
        let Some(Identity {
            message_id,
            persisted_at,
            owner,
            content,
            media,
            background,
        }) = identity(envelope)
        else {
            continue;
        };
        let row = current_ids.get(message_id).copied().or_else(|| {
            (!owner.is_empty())
                .then(|| owners.get(&(persisted_at, owner)).copied().flatten())
                .flatten()
        });
        let Some(row) = row else {
            continue;
        };
        let message = &messages[row];
        let owner_matches = message.thread_id.as_deref().is_some_and(|thread| {
            !thread.is_empty()
                && (thread == owner
                    || matches!(
                        &envelope.payload,
                        PayloadV2::BackgroundChildCompleted {
                            response_to_client_message_id: Some(response_to),
                            ..
                        } if response_to == thread
                    ))
        });
        if owner.is_empty()
            || !owner_matches
            || message.role != MessageRole::Assistant
            || message.timestamp != persisted_at
            || message.content != content
            || message.media != media
        {
            continue;
        }
        resolved
            .entry(row)
            .and_modify(|identity| *identity = None)
            .or_insert(Some((message_id.to_owned(), background)));
    }
    resolved
        .into_iter()
        .filter_map(|(row, identity)| identity.map(|identity| (row, identity)))
        .collect()
}

/// Per UPCR-2026-009: bundle the chat-state projection into one RPC.
///
/// Atomicity invariant (codex's review ask): the ledger snapshot and the
/// returned `cursor` are read in one critical section via
/// [`UiProtocolLedger::snapshot_with_cursor`]. A concurrent appender cannot
/// land an event with cursor ≤ result.cursor that the client did not also
/// observe — so a follow-up `session/hydrate { after: result.cursor }`
/// returns only events strictly after the snapshot, with no gap.
// The handler threads the ledger, approval/question stores, turn registry,
// and the caller's profile scope plus negotiated features as one flat
// per-request dependency list.
#[allow(clippy::too_many_arguments)]
async fn handle_session_hydrate(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    approvals: &PendingApprovalStore,
    questions: &PendingQuestionStore,
    active_turns: &SharedActiveTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: SessionHydrateParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }
    if params.include.len() > SESSION_HYDRATE_INCLUDE_MAX {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!(
                "session/hydrate include too large: {} > {}",
                params.include.len(),
                SESSION_HYDRATE_INCLUDE_MAX
            ))
            .with_data(json!({
                "kind": "include_too_large",
                "limit": SESSION_HYDRATE_INCLUDE_MAX,
            })),
        );
        return;
    }

    // Atomic snapshot of (events ≥ after, head cursor) — closes the
    // codex-flagged gap where reading events and head separately could
    // miss any event committed in between.
    let (replayed, head_cursor) =
        match ledger.snapshot_with_cursor(&params.session_id, params.after.as_ref()) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = send_rpc_error(ws, Some(id), error);
                return;
            }
        };

    let include_set = HydrateIncludeSet::from_request(&params.include);
    // #919.1: route to the profile's session manager when the connection
    // has a profile scope. Turn persistence writes to
    // `SessionRuntime.sessions`; reads must hit the same handle or we'd
    // report `unknown_session` on reconnect-hydrate.
    let Some(sessions) = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        routed_profile_id,
        &params.session_id,
    )
    .await
    else {
        let _ = send_rpc_error(
            ws,
            Some(id),
            runtime_unavailable_error("Sessions not available"),
        );
        return;
    };
    // Reject unknown sessions per UPCR-2026-009 error model. The session
    // must already exist (typically via a prior `session/open` call); we
    // do NOT auto-create on hydrate.
    {
        let mut sessions_guard = sessions.lock().await;
        if !sessions_guard.session_known(&params.session_id) {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(params.session_id.0.clone()),
            );
            return;
        }
    }
    // Hydrate carries the same canonical v2 records as live delivery. Route
    // historical retained source records through the projector too, so an old
    // on-disk turn/spawn_complete row remains readable without restoring a
    // legacy wire lane.
    let replayed_envelopes = if features.projection_envelope_v2 && include_set.messages {
        Some(
            replayed
                .iter()
                .filter_map(|event| {
                    let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) =
                        project_v2_ledger_event(ledger, &event.event, &event.cursor)?
                    else {
                        return None;
                    };
                    matches!(
                        &envelope.envelope.payload,
                        PayloadV2::BackgroundChildCompleted { .. }
                    )
                    .then_some(envelope.envelope)
                })
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };
    let replayed_tool_envelopes = if features.projection_envelope_v2 && include_set.messages {
        Some(
            replayed
                .iter()
                .filter_map(|event| {
                    let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) =
                        project_v2_ledger_event(ledger, &event.event, &event.cursor)?
                    else {
                        return None;
                    };
                    hydrate_replays_tool_payload(&envelope.envelope).then_some(envelope.envelope)
                })
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };

    let expose_message_id = features.projection_envelope_v2 && include_set.messages;
    // Identity provenance is independent of the caller's replay window AND
    // the hot-ring cap. Read only eligible canonical references through the
    // original scoped head, including evidence still in rotated retained logs.
    let identity_history = if expose_message_id {
        ledger
            .retained_message_identity_references(&params.session_id, &head_cursor)
            .ok()
    } else {
        None
    };
    let canonical_envelopes = if expose_message_id {
        identity_history
            .as_deref()
            .unwrap_or(&replayed)
            .iter()
            .filter(|event| event.cursor.seq <= head_cursor.seq)
            .filter_map(|event| {
                let UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) =
                    project_v2_ledger_event(ledger, &event.event, &event.cursor)?
                else {
                    return None;
                };
                matches!(
                    envelope.envelope.payload,
                    PayloadV2::AssistantPersisted { .. }
                        | PayloadV2::BackgroundChildCompleted { .. }
                )
                .then_some(envelope)
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let claimed_message_ids = canonical_envelopes
        .iter()
        .filter(|notification| notification.session_id == params.session_id)
        .filter_map(|notification| match &notification.envelope.payload {
            PayloadV2::AssistantPersisted { meta, .. } => Some(meta.message_id.as_str()),
            PayloadV2::BackgroundChildCompleted { message_id, .. } => Some(message_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();

    // Lock once; gather all the in-memory chat state we need so the
    // result reflects a single sessions-side snapshot.
    let (messages, threads_projection, context, context_state) = {
        let mut sessions_guard = sessions.lock().await;
        let data_dir = sessions_guard.data_dir();
        let session = sessions_guard.get_or_create(&params.session_id).await;
        let (context, context_state) = if features.context_lifecycle_available() {
            let (context, context_state) =
                appui_context_inspection_snapshot(&data_dir, &params.session_id, &session.messages);
            context_snapshot_for_features(Some(context), Some(context_state), features)
        } else {
            (None, None)
        };
        let canonical_identities = hydrated_canonical_message_identities(
            &params.session_id,
            &session.messages,
            &canonical_envelopes,
        );
        let messages = if include_set.messages {
            Some(
                session
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(seq, _)| match params.after.as_ref() {
                        Some(after) => *seq as u64 > after.seq,
                        None => true,
                    })
                    .map(|(seq, msg)| {
                        let canonical_identity = canonical_identities.get(&seq);
                        let seq = seq as u64;
                        // V2 clients get a transcript identity that matches
                        // assistant_persisted/background_child_completed.
                        let message_id = if expose_message_id {
                            canonical_identity.map(|(id, _)| id.clone()).or_else(|| {
                                let candidate = format!(
                                    "{}:{seq}:{}",
                                    params.session_id.0,
                                    msg.timestamp.timestamp_nanos_opt().unwrap_or(0),
                                );
                                // A rejected claim must not re-enter as the
                                // fallback ID: the client correlates by ID even
                                // when source is absent. Leave it unbound.
                                (!claimed_message_ids.contains(candidate.as_str()))
                                    .then_some(candidate)
                            })
                        } else {
                            None
                        };
                        let source = canonical_identity
                            .filter(|(_, background)| *background)
                            .map(|_| "background".to_owned());
                        HydratedMessage {
                            seq,
                            role: msg.role.as_str().to_owned(),
                            content: msg.content.clone(),
                            turn_id: None, // Message struct does not carry typed turn_id today
                            thread_id: msg.thread_id.clone(),
                            client_message_id: msg.client_message_id.clone(),
                            persisted_at: msg.timestamp,
                            message_id,
                            source,
                            // The store persists reasoning; without this the
                            // "· reasoning" block vanishes on every client
                            // restart. Same v2 gate as message_id/source.
                            reasoning_content: if expose_message_id {
                                msg.reasoning_content.clone()
                            } else {
                                None
                            },
                            // P1.3 fix: surface canonical-ledger media so a
                            // client reconnecting after a disconnect can
                            // re-render the same `.md` / `.mp3` / `.pptx`
                            // attachment carried by the v2 projection.
                            media: msg.media.clone(),
                        }
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let threads_projection = if include_set.threads || include_set.turns {
            Some(build_thread_graph_entries(session))
        } else {
            None
        };
        (messages, threads_projection, context, context_state)
    };

    let threads = if include_set.threads {
        threads_projection
            .clone()
            .map(|(threads, _orphans)| threads)
    } else {
        None
    };

    let turns = if include_set.turns {
        let projected_threads = threads_projection
            .as_ref()
            .map(|(t, _)| t.clone())
            .unwrap_or_default();
        Some(
            collect_session_turns(
                &params.session_id,
                active_turns,
                &replayed,
                &projected_threads,
            )
            .await,
        )
    } else {
        None
    };

    let pending_approvals = if include_set.pending_approvals {
        Some(approvals.pending_for_session(&params.session_id))
    } else {
        None
    };

    // UPCR-2026-023: hydrate still-pending structured user-questions alongside
    // pending approvals (same `pending_approvals` include section), but only
    // for a connection that negotiated `user_question.v1` — a client without
    // the capability has no `user_question/respond` path, so the section is
    // omitted (not `null`) exactly like a non-negotiated wire event is
    // filtered out. Mirrors the `session/open` pending-question replay gate.
    let pending_questions = if include_set.pending_approvals && features.user_question_v1 {
        Some(questions.pending_for_session(&params.session_id))
    } else {
        None
    };

    let result = SessionHydrateResult {
        session_id: params.session_id,
        cursor: head_cursor,
        context,
        context_state,
        messages,
        threads,
        turns,
        pending_approvals,
        pending_questions,
        replayed_envelopes,
        replayed_tool_envelopes,
    };
    send_serialized_rpc_result(
        ws,
        id,
        octos_core::ui_protocol::methods::SESSION_HYDRATE,
        result,
    );
}

fn hydrate_replays_tool_payload(envelope: &EnvelopeV2) -> bool {
    matches!(
        envelope.payload,
        PayloadV2::ToolStart { .. } | PayloadV2::ToolProgress { .. } | PayloadV2::ToolEnd { .. }
    )
}

/// `session/rollback` — conversation-only rewind. Drops the last `num_turns`
/// user turns from the session (persisted + in-memory), persists an idempotent
/// append-only marker, and returns the trimmed thread projected exactly like
/// `session/hydrate` (messages + threads + turns + cursor).
///
/// Scoped / resolved identically to [`handle_session_hydrate`]. Rejected when a
/// turn is in progress for the session (rewinding under an active turn would
/// race the writer). NOTHING outside the conversation transcript is touched —
/// no git, worktree, or workspace file state.
#[allow(clippy::too_many_arguments)]
async fn handle_session_rollback(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    active_turns: &SharedActiveTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    id: String,
    params: SessionRollbackParams,
) {
    let method = octos_core::ui_protocol::methods::SESSION_ROLLBACK;
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }
    // `num_turns` must be >= 1 — a zero-turn rollback is a no-op the client
    // should never send.
    if params.num_turns < 1 {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!("{method}: num_turns must be >= 1"))
                .with_data(json!({ "kind": "invalid_num_turns" })),
        );
        return;
    }

    // Snapshot the ledger once for the trimmed-thread projection's cursor +
    // turn overlay (mirrors the hydrate handler).
    let (replayed, head_cursor) = match ledger.snapshot_with_cursor(&params.session_id, None) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };

    let Some(sessions) = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        routed_profile_id,
        &params.session_id,
    )
    .await
    else {
        let _ = send_rpc_error(
            ws,
            Some(id),
            runtime_unavailable_error("Sessions not available"),
        );
        return;
    };

    // Guard: refuse to rewind while a turn is in flight for this session.
    // Detected via the same process-global active-turn registry the
    // turn/hydrate handlers consult. Checked before taking the sessions lock so
    // we never hold two locks at once.
    if active_turn_sessions(active_turns)
        .await
        .contains(&params.session_id)
    {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!(
                "{method}: a turn is in progress; interrupt it before rolling back"
            ))
            .with_data(json!({ "kind": "turn_in_progress" })),
        );
        return;
    }

    // Apply the rollback (marker + in-memory trim) and project the trimmed
    // thread while holding the sessions lock, so the projection reflects the
    // post-trim snapshot.
    let (dropped_turns, messages, threads, orphans) = {
        let mut sessions_guard = sessions.lock().await;
        // Reject unknown sessions per the hydrate error model — we do NOT
        // auto-create on rollback.
        if !sessions_guard.session_known(&params.session_id) {
            drop(sessions_guard);
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(params.session_id.0.clone()),
            );
            return;
        }
        let dropped_turns = match sessions_guard
            .rollback_last_n_user_turns(&params.session_id, params.num_turns)
            .await
        {
            Ok(dropped) => dropped,
            Err(error) => {
                drop(sessions_guard);
                let _ = send_rpc_error(
                    ws,
                    Some(id),
                    RpcError::internal_error(format!("{method}: {error}")),
                );
                return;
            }
        };
        let data_dir = sessions_guard.data_dir();
        let session = sessions_guard.get_or_create(&params.session_id).await;
        // Rebuild + persist the context ledger from the trimmed history.
        // The ledger coverage check (`context_ledger_covers_history`) is a
        // high-watermark `>=` — deliberately tolerant of bounded history
        // slices — so a pre-rollback snapshot still "covers" the shrunken
        // history and would be Loaded verbatim on the next turn, feeding the
        // rolled-back turns straight back into the model prompt. Mirrors
        // `reset_context_manager_from_history` on the session-actor path.
        let mut rebuilt_context = crate::context_manager::ContextManager::from_session_history(
            params.session_id.to_string(),
            None,
            &session.messages,
        );
        rebuilt_context.set_recovery_state(crate::context_manager::ContextRecoveryState::Rebuilt);
        if let Err(error) =
            persist_appui_context_snapshot(&data_dir, &params.session_id, &rebuilt_context)
        {
            warn!(
                session = %params.session_id,
                %error,
                "session/rollback: failed to persist rebuilt context ledger"
            );
        }
        publish_appui_context_status(&params.session_id, &rebuilt_context);
        // Same message projection as `handle_session_hydrate` for a
        // non-negotiated client: seqs are the trimmed transcript's indices.
        let messages = session
            .messages
            .iter()
            .enumerate()
            .map(|(seq, msg)| HydratedMessage {
                seq: seq as u64,
                role: msg.role.as_str().to_owned(),
                content: msg.content.clone(),
                turn_id: None,
                thread_id: msg.thread_id.clone(),
                client_message_id: msg.client_message_id.clone(),
                persisted_at: msg.timestamp,
                message_id: None,
                source: None,
                reasoning_content: None,
                media: msg.media.clone(),
            })
            .collect::<Vec<_>>();
        let (threads, orphans) = build_thread_graph_entries(session);
        (dropped_turns, messages, threads, orphans)
    };
    let _ = orphans;

    // Turn projection reuses the exact hydrate helper over the trimmed threads.
    // The `replayed` snapshot was taken PRE-trim, so it still carries lifecycle
    // + canonical projection events for the just-rolled-back turns. Scope the
    // projected turns to the SURVIVING threads (codex P2): a turn belongs to the
    // trimmed thread iff its `thread_id` — surfaced from the ledger's
    // projection envelope rows — is still present among the trimmed threads.
    // Without this, `thread.turns` would leak lifecycle state for dropped turns
    // even though `messages`/`threads` are trimmed.
    let surviving_thread_ids: std::collections::HashSet<&str> = threads
        .iter()
        .map(|entry| entry.thread_id.as_str())
        .collect();
    let turns = collect_session_turns(&params.session_id, active_turns, &replayed, &threads)
        .await
        .into_iter()
        .filter(|turn| {
            turn.thread_id
                .as_deref()
                .is_some_and(|thread_id| surviving_thread_ids.contains(thread_id))
        })
        .collect::<Vec<_>>();

    let thread = SessionHydrateResult {
        session_id: params.session_id.clone(),
        cursor: head_cursor,
        context: None,
        context_state: None,
        messages: Some(messages),
        threads: Some(threads),
        turns: Some(turns),
        pending_approvals: None,
        pending_questions: None,
        replayed_envelopes: None,
        replayed_tool_envelopes: None,
    };
    let result = SessionRollbackResult {
        dropped_turns,
        thread,
    };
    send_serialized_rpc_result(ws, id, method, result);
}

/// Process-global in-flight fork child-key reservations. Two forks
/// from DIFFERENT parents resolve different per-session
/// `SessionManager`s, so a manager-local existence check cannot see
/// the other fork mid-write — the later rewrite would silently
/// overwrite the earlier child (codex #1613 P2). Keyed by
/// `(sessions_dir, child key)`: the sessions DIR is the on-disk
/// collision domain — same-profile managers share it and must
/// mutually exclude, while different profiles' identical child keys
/// name different files and must NOT reject each other (codex #1613
/// r2). One serve process owns a profile's sessions dir, so a
/// process-global set suffices.
fn fork_reservations() -> &'static std::sync::Mutex<std::collections::HashSet<(PathBuf, String)>> {
    static RESERVATIONS: OnceLock<std::sync::Mutex<std::collections::HashSet<(PathBuf, String)>>> =
        OnceLock::new();
    RESERVATIONS.get_or_init(Default::default)
}

/// RAII reservation on a fork child key — released on every exit path.
struct ForkReservation((PathBuf, String));

impl ForkReservation {
    /// `None` when another fork to the same child key in the same
    /// sessions dir is in flight.
    fn try_acquire(sessions_dir: &Path, child_key: &SessionKey) -> Option<Self> {
        // Canonicalized: two managers can reach one on-disk dir through
        // different spellings (`..` segments, symlinked data_dir
        // overrides) — distinct PathBufs would defeat the reservation
        // (codex #1613 r3). The dir exists (the manager created it), so
        // canonicalize only fails on races — fall back to the raw path.
        let dir =
            std::fs::canonicalize(sessions_dir).unwrap_or_else(|_| sessions_dir.to_path_buf());
        let key = (dir, child_key.0.clone());
        let mut set = fork_reservations()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set.insert(key.clone()).then(|| Self(key))
    }
}

impl Drop for ForkReservation {
    fn drop(&mut self) {
        let mut set = fork_reservations()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set.remove(&self.0);
    }
}

/// `session/fork` — branch a NEW session off an existing one, copying
/// the tail of its history (web parity P3: the book's documented fork
/// affordance had no wire surface for the SPA; `SessionManager::fork`
/// existed but had no production caller). MUTATING: writes the child
/// session (parent tracked via `parent_key`).
async fn handle_session_fork(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    id: String,
    params: octos_core::ui_protocol::SessionForkParams,
) {
    let method = octos_core::ui_protocol::methods::SESSION_FORK;
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }
    // The child chat-id becomes a filesystem path component and a wire
    // session key half — hold it to the same charset as topic names.
    if let Err(reason) = octos_bus::validate_topic_name(&params.new_chat_id) {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!("{method}: invalid new_chat_id: {reason}"))
                .with_data(json!({ "kind": "invalid_new_chat_id" })),
        );
        return;
    }

    let Some(sessions) = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        routed_profile_id,
        &params.session_id,
    )
    .await
    else {
        let _ = send_rpc_error(
            ws,
            Some(id),
            runtime_unavailable_error("Sessions not available"),
        );
        return;
    };

    let (new_key, copied) = {
        let mut sessions_guard = sessions.lock().await;
        // Reserve the child key across the whole check-then-write: the
        // durable `session_known` check below cannot see a concurrent
        // fork's child (a DIFFERENT parent's manager) until its write
        // lands. Scoped to this manager's sessions dir — the on-disk
        // collision domain (RAII — released on return).
        let candidate_key = params.session_id.fork_child(&params.new_chat_id);
        let Some(_reservation) =
            ForkReservation::try_acquire(sessions_guard.sessions_dir(), &candidate_key)
        else {
            drop(sessions_guard);
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::invalid_params(format!(
                    "{method}: a fork to '{candidate_key}' is already in flight"
                ))
                .with_data(json!({ "kind": "child_exists" })),
            );
            return;
        };
        // Mirror rollback's error model: fork of an unknown session is
        // an error, not an auto-create.
        if !sessions_guard.session_known(&params.session_id) {
            drop(sessions_guard);
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(params.session_id.0.clone()),
            );
            return;
        }
        // Refuse to clobber an existing session under the child key.
        // `SessionKey::fork_child` is the SAME rule `SessionManager::fork`
        // applies, so the pre-check guards exactly the key fork writes.
        let candidate = params.session_id.fork_child(&params.new_chat_id);
        if sessions_guard.session_known(&candidate) {
            drop(sessions_guard);
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::invalid_params(format!(
                    "{method}: a session already exists for '{candidate}'"
                ))
                .with_data(json!({ "kind": "child_exists" })),
            );
            return;
        }
        // Absent copy_messages → the FULL parent history.
        let copy = params
            .copy_messages
            .map(|n| n as usize)
            .unwrap_or(usize::MAX);
        let copied_upper = {
            let parent = sessions_guard.get_or_create(&params.session_id).await;
            parent.messages.len().min(copy)
        };
        match sessions_guard
            .fork(&params.session_id, &params.new_chat_id, copy)
            .await
        {
            Ok(new_key) => (new_key, copied_upper as u32),
            Err(error) => {
                drop(sessions_guard);
                let _ = send_rpc_error(
                    ws,
                    Some(id),
                    RpcError::internal_error(format!("{method}: {error}")),
                );
                return;
            }
        }
    };

    let result = octos_core::ui_protocol::SessionForkResult {
        new_session_id: new_key,
        parent_session_id: params.session_id,
        copied_messages: copied,
    };
    send_serialized_rpc_result(ws, id, method, result);
}

/// Per UPCR-2026-010: lift the in-memory thread partition onto the wire.
// Connection state, ledger, turn registry, and the caller's profile scope
// arrive as separate per-request pieces — a flat dependency list.
#[allow(clippy::too_many_arguments)]
async fn handle_thread_graph_get(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    _active_turns: &SharedActiveTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    id: String,
    params: ThreadGraphGetParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }

    // Atomic snapshot for `at`/`cursor` consistency. We don't actually
    // need the events here, but the cursor read piggybacks off the same
    // helper so the wire result echoes the head-of-snapshot moment.
    let (_events, head_cursor) = match ledger.snapshot_with_cursor(&params.session_id, None) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };

    // #919.1: route to the profile's session manager when under profile auth.
    let Some(sessions) = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        routed_profile_id,
        &params.session_id,
    )
    .await
    else {
        let _ = send_rpc_error(
            ws,
            Some(id),
            runtime_unavailable_error("Sessions not available"),
        );
        return;
    };
    // Reject unknown sessions per UPCR-2026-010 error model.
    {
        let mut sessions_guard = sessions.lock().await;
        if !sessions_guard.session_known(&params.session_id) {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(params.session_id.0.clone()),
            );
            return;
        }
    }
    let (threads, orphans) = {
        let mut sessions_guard = sessions.lock().await;
        let session = sessions_guard.get_or_create(&params.session_id).await;
        build_thread_graph_entries(session)
    };

    // When the caller pinned `at`, echo that cursor; otherwise return the
    // current head. Note: `at` as a true point-in-time projection of the
    // grouping is bounded by what `Session::messages` exposes today;
    // honouring `at` rigorously requires per-seq message snapshots in the
    // session store, which is out of scope for PR G. The wire shape
    // unconditionally reflects the current grouping; future UPCR can add
    // strict point-in-time snapshots if pinning becomes a hard requirement.
    let cursor = params.at.unwrap_or(head_cursor);

    let result = ThreadGraphGetResult {
        session_id: params.session_id,
        cursor,
        threads,
        orphans,
    };
    send_serialized_rpc_result(
        ws,
        id,
        octos_core::ui_protocol::methods::THREAD_GRAPH_GET,
        result,
    );
}

/// Per UPCR-2026-011: turn lifecycle introspection backed by the in-memory
/// active-turn registry AND a durable projection from the ledger
/// (`turn/started` + terminal `turn/completed` / `turn/error`). Codex's
/// review asked for the durable backing so a turn the registry has already
/// evicted (e.g. daemon restart, idle eviction) can still surface a
/// non-`unknown` state.
// The handler consults the ledger AND the active-turn registry under the
// caller's profile scope and negotiated features — a flat per-request
// dependency list, not a missing struct.
#[allow(clippy::too_many_arguments)]
async fn handle_turn_state_get(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    active_turns: &SharedActiveTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: TurnStateGetParams,
) {
    if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return;
    }

    // UPCR-2026-011: reject `unknown_session` so the client distinguishes
    // "wrong session id" from "session id known but turn missing"
    // (which returns `state: unknown`). When the sessions manager is
    // unavailable we fall through to the default "unknown" path so the
    // RPC remains callable in headless tests.
    //
    // #919.1: route to the profile's session manager when under profile
    // auth so reads see the same store the turn writes used.
    let sessions = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        routed_profile_id,
        &params.session_id,
    )
    .await;
    if let Some(sessions) = sessions.as_ref() {
        let mut sessions_guard = sessions.lock().await;
        if !sessions_guard.session_known(&params.session_id) {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(params.session_id.0.clone()),
            );
            return;
        }
    }

    // Look up in the active-turn registry first.
    let registry_state = {
        let registry = active_turns.lock().await;
        if let Some(entry) = registry.get(&params.session_id) {
            if entry.turn_id == params.turn_id {
                let state = entry.state.lock().await;
                Some(turn_state_to_lifecycle(&state))
            } else {
                None
            }
        } else {
            None
        }
    };

    // Pull the ledger projection so we can backfill thread_id /
    // started_at / completed_at / committed_seqs even when the registry
    // entry is absent or carries less metadata.
    let projection = match ledger.snapshot_with_cursor(&params.session_id, None) {
        Ok((events, _)) => Some(project_turn_from_ledger(&params.turn_id, &events)),
        Err(_) => None,
    };

    // Cross-reference Session::messages for committed_seqs that match the
    // turn_id via thread_id grouping (today the type system does not yet
    // carry typed turn_id on Message; we approximate via the projection's
    // thread_id and the message's stored thread_id).
    let committed_seqs = if let Some(sessions) = sessions.as_ref() {
        let mut sessions_guard = sessions.lock().await;
        let session = sessions_guard.get_or_create(&params.session_id).await;
        let target_thread_id = projection.as_ref().and_then(|p| p.thread_id.clone());
        target_thread_id
            .map(|target| {
                session
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, msg)| msg.thread_id.as_deref() == Some(target.as_str()))
                    .map(|(seq, _)| seq as u64)
                    .collect::<Vec<u64>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let (context, context_state) = if features.context_lifecycle_available() {
        appui_context_status_snapshot_for_state(
            state,
            connection_profile_id,
            routed_profile_id,
            &params.session_id,
        )
        .await
    } else {
        (None, None)
    };
    let (context, context_state) = context_snapshot_for_features(context, context_state, features);

    // Combine: registry beats projection for `state` (live truth) but
    // projection backfills metadata. When neither knows the turn, return
    // `unknown` per UPCR-2026-011 (NOT an error).
    let (state_value, started_at, completed_at, thread_id) =
        match (registry_state, projection.as_ref()) {
            (Some(state), Some(proj)) => (
                state,
                proj.started_at,
                proj.completed_at,
                proj.thread_id.clone(),
            ),
            (Some(state), None) => (state, None, None, None),
            (None, Some(proj)) => (
                proj.state.unwrap_or(TurnLifecycleState::Unknown),
                proj.started_at,
                proj.completed_at,
                proj.thread_id.clone(),
            ),
            (None, None) => (TurnLifecycleState::Unknown, None, None, None),
        };

    let result = TurnStateGetResult {
        session_id: params.session_id,
        turn_id: params.turn_id,
        state: state_value,
        context,
        context_state,
        started_at,
        completed_at,
        thread_id,
        committed_seqs,
    };
    send_serialized_rpc_result(
        ws,
        id,
        octos_core::ui_protocol::methods::TURN_STATE_GET,
        result,
    );
}

/// In-flight `session/btw` asides, keyed by (profile, session) — session
/// runtimes are isolated per profile and bare session ids can repeat across
/// profiles, so a bare-session key would let one profile's aside starve
/// another's. One at a time per key: each aside is a paid provider request. `std::sync::Mutex` on purpose: the critical
/// sections never await, and the release must run in a `Drop` guard (which
/// cannot lock a tokio mutex).
fn btw_in_flight_sessions() -> &'static StdMutex<HashSet<(String, SessionKey)>> {
    static BTW_IN_FLIGHT: OnceLock<StdMutex<HashSet<(String, SessionKey)>>> = OnceLock::new();
    BTW_IN_FLIGHT.get_or_init(|| StdMutex::new(HashSet::new()))
}

/// Releases the per-(profile, session) `session/btw` slot on every exit path
/// (including panics/cancellation) so an error can never wedge the aside.
struct BtwInFlightGuard((String, SessionKey));

impl Drop for BtwInFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut in_flight) = btw_in_flight_sessions().lock() {
            in_flight.remove(&self.0);
        }
    }
}

/// Test-only provider override so handler tests can exercise the full
/// `session/btw` flow without constructing a `ProfileRuntime`.
#[cfg(test)]
fn btw_test_provider_slot() -> &'static StdMutex<Option<Arc<dyn octos_llm::LlmProvider>>> {
    static SLOT: OnceLock<StdMutex<Option<Arc<dyn octos_llm::LlmProvider>>>> = OnceLock::new();
    SLOT.get_or_init(|| StdMutex::new(None))
}

fn btw_test_provider() -> Option<Arc<dyn octos_llm::LlmProvider>> {
    #[cfg(test)]
    if let Ok(slot) = btw_test_provider_slot().lock() {
        if let Some(llm) = slot.as_ref() {
            return Some(llm.clone());
        }
    }
    None
}

/// Live in-flight assistant draft per TURN, fed at the ephemeral send choke
/// point (`message/delta` is non-durable per spec § 9 — the ledger can never
/// replay it) so `session/btw` can describe the answer being written RIGHT
/// NOW. Keyed by [`TurnId`] — globally unique per turn — so no cross-profile
/// or cross-session stream can ever mix and no lifecycle resets are needed:
/// a new turn is a new key, and stale keys are unreadable (the aside resolves
/// the CURRENT non-terminal turn id through the active-turns registry before
/// reading). Bounded by arbitrary eviction past the cap.
fn btw_live_draft_store() -> &'static StdMutex<HashMap<(SessionKey, TurnId), String>> {
    static DRAFTS: OnceLock<StdMutex<HashMap<(SessionKey, TurnId), String>>> = OnceLock::new();
    DRAFTS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Clear a turn's draft slot at ADMISSION (synchronously, before the turn task
/// can stream): `turn_id` is client-supplied with no global uniqueness check,
/// so a reused id must start from a clean slate instead of inheriting the
/// prior turn's tail.
fn btw_live_draft_clear(session_id: &SessionKey, turn_id: &TurnId) {
    if let Ok(mut drafts) = btw_live_draft_store().lock() {
        drafts.remove(&(session_id.clone(), turn_id.clone()));
    }
}

const BTW_LIVE_DRAFT_STORE_MAX_TURNS: usize = 64;

fn btw_live_draft_append(session_id: &SessionKey, turn_id: &TurnId, text: &str) {
    let Ok(mut drafts) = btw_live_draft_store().lock() else {
        return;
    };
    let key = (session_id.clone(), turn_id.clone());
    if !drafts.contains_key(&key) && drafts.len() >= BTW_LIVE_DRAFT_STORE_MAX_TURNS {
        // Arbitrary eviction is fine — a lost draft only degrades one aside's
        // context, and terminal turns' entries are dead weight anyway
        // (unreadable through the non-terminal gate).
        let evict = drafts.keys().next().cloned();
        if let Some(evict) = evict {
            drafts.remove(&evict);
        }
    }
    let draft = drafts.entry(key).or_default();
    draft.push_str(text);
    if draft.len() > BTW_LIVE_DRAFT_TAIL_CHARS {
        let cut = draft.len() - BTW_LIVE_DRAFT_TAIL_CHARS;
        let cut = draft
            .char_indices()
            .map(|(index, _)| index)
            .find(|index| *index >= cut)
            .unwrap_or(0);
        *draft = draft.split_off(cut);
    }
}

fn btw_live_draft_tail(session_id: &SessionKey, turn_id: &TurnId) -> String {
    btw_live_draft_store()
        .lock()
        .ok()
        .and_then(|drafts| drafts.get(&(session_id.clone(), turn_id.clone())).cloned())
        .unwrap_or_default()
}

const BTW_TRANSCRIPT_TAIL_MESSAGES: usize = 20;
const BTW_TRANSCRIPT_MESSAGE_CHARS: usize = 1_200;
const BTW_TRANSCRIPT_CHAR_BUDGET: usize = 16_000;
const BTW_ACTIVITY_TAIL_EVENTS: usize = 12;
const BTW_LIVE_DRAFT_TAIL_CHARS: usize = 1_200;
const BTW_ANSWER_MAX_TOKENS: u32 = 700;
const BTW_TIMEOUT_SECS: u64 = 30;

/// Build the two-message prompt for a `session/btw` aside. Pure so the
/// context shape is unit-testable: transcript tail (already limited by the
/// caller) + a short live-activity digest + the question. The system prompt
/// carries the restrictions: no tools, brief answer, ephemeral exchange.
/// The `ChatConfig` for a `session/btw` aside — split out so its cache
/// economics are pinnable in isolation.
fn btw_chat_config() -> octos_llm::ChatConfig {
    octos_llm::ChatConfig {
        max_tokens: Some(BTW_ANSWER_MAX_TOKENS),
        temperature: Some(0.2),
        tool_choice: octos_llm::ToolChoice::None,
        // #2194 review: ONE restricted LLM call per aside — the prompt
        // (transcript tail + activity tail + question) is never replayed, so
        // a cache write is pure premium.
        cache_retention: octos_llm::CacheRetention::None,
        ..Default::default()
    }
}

fn build_btw_messages(
    transcript_tail: &[Message],
    activity_lines: &[String],
    live_draft_tail: &str,
    question: &str,
) -> Vec<Message> {
    let mut transcript = String::new();
    let mut used = 0usize;
    for message in transcript_tail {
        let content = message.content.trim();
        if content.is_empty() {
            continue;
        }
        let rendered = format!(
            "{}: {}\n",
            message.role.as_str(),
            truncate_for_display(content, BTW_TRANSCRIPT_MESSAGE_CHARS)
        );
        if used + rendered.len() > BTW_TRANSCRIPT_CHAR_BUDGET {
            break;
        }
        used += rendered.len();
        transcript.push_str(&rendered);
    }
    if transcript.is_empty() {
        transcript.push_str("(no messages yet)\n");
    }
    let activity = if activity_lines.is_empty() {
        "- (no live activity)".to_owned()
    } else {
        activity_lines
            .iter()
            .map(|line| format!("- {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let draft = if live_draft_tail.trim().is_empty() {
        String::new()
    } else {
        format!("\n## In-flight answer you are writing right now (tail)\n…{live_draft_tail}\n")
    };
    let prompt = format!(
        "## Recent conversation\n{transcript}\n## Current activity\n{activity}\n{draft}\n## Aside question\n{question}\n"
    );
    vec![
        Message {
            role: MessageRole::System,
            content: "You are the assistant working inside this octos session. The user \
                      asked a QUICK ASIDE question (\"btw, ...\") while your main work \
                      continues in the background. Answer briefly — a few sentences — \
                      from the context provided. You have NO tools in this aside: do not \
                      claim to run commands, read files, or edit anything. If the answer \
                      genuinely needs that, say so and suggest asking again after the \
                      current task finishes. This exchange is ephemeral and will not \
                      join the conversation history."
                .to_owned(),
            media: Vec::new(),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        },
        Message {
            role: MessageRole::User,
            content: prompt,
            media: Vec::new(),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        },
    ]
}

/// One live-activity digest line per recent ledger notification that says
/// what the session is DOING right now (tool/task/turn lifecycle); chatty
/// stream events (message/reasoning deltas) are skipped.
fn btw_activity_line(notification: &UiNotification) -> Option<String> {
    match notification {
        UiNotification::TurnStarted(event) => Some(format!("turn {} started", event.turn_id.0)),
        UiNotification::TurnCompleted(event) => Some(format!("turn {} completed", event.turn_id.0)),
        UiNotification::ToolStarted(event) => Some(format!("tool `{}` running", event.tool_name)),
        UiNotification::ToolCompleted(event) => Some(format!(
            "tool `{}` finished ({})",
            event.tool_name,
            match event.success {
                Some(true) => "ok",
                Some(false) => "failed",
                None => "done",
            }
        )),
        UiNotification::TaskUpdated(event) => {
            Some(format!("task \"{}\" {:?}", event.title, event.state))
        }
        _ => None,
    }
}

/// `session/btw` — answer a quick aside question out-of-band while the
/// session's live turn (if any) keeps running. ONE restricted LLM call: no
/// tools, capped output, hard timeout. The exchange is ephemeral — nothing
/// is appended to the session history and no notification is emitted, so
/// the live turn never sees it; the answer rides only on this RPC result.
///
/// The provider call runs DETACHED (codex round-1 P1): this fn returns as
/// soon as the aside is validated and snapshotted, so the connection's read
/// loop keeps serving interrupts/approvals — and the busy gate actually
/// gates — while the answer (up to 30s) is produced.
// Connection, ledger, turn registry, and the caller's profile scope arrive
// as separate per-request pieces — a flat dependency list.
#[allow(clippy::too_many_arguments)]
async fn handle_session_btw(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    active_turns: &SharedActiveTurns,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    id: String,
    params: SessionBtwParams,
) -> Option<tokio::task::JoinHandle<()>> {
    // Fold the topic into the canonical session key FIRST (codex round-1 P1):
    // the ingress gate scopes `session#topic`, so every lookup/guard below
    // must use the same folded key or a topic-scoped credential could read a
    // differently-scoped session's transcript.
    let session_id = session_key_with_optional_topic(&params.session_id, params.topic.as_deref());
    if let Err(error) = validate_session_scope(&session_id, None, connection_profile_id) {
        send_scope_error(ws, id, error);
        return None;
    }
    let question = params.question.trim().to_owned();
    if question.is_empty() {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params("session/btw requires a non-empty question"),
        );
        return None;
    }

    // ONE profile resolution shared by transcript AND provider, with
    // `resolve_sessions_for_lookup`'s exact precedence (session key →
    // connection → routed; the ws arm threads its session-open fallback in
    // through `routed_profile_id`) — a divergent pair would read one
    // profile's transcript and bill another profile's provider.
    let active_profile_id = session_id
        .profile_id()
        .or(connection_profile_id)
        .or(routed_profile_id)
        .map(ToOwned::to_owned);

    // Transcript tail from the same store turn persistence writes to
    // (#919.1 routing) — clone under the lock, drop it before the LLM call.
    let transcript_tail = {
        let Some(sessions) = resolve_sessions_for_lookup(
            state,
            connection_profile_id,
            routed_profile_id,
            &session_id,
        )
        .await
        else {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(session_id.0.clone()),
            );
            return None;
        };
        let mut sessions_guard = sessions.lock().await;
        if !sessions_guard.session_known(&session_id) {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::unknown_session(session_id.0.clone()),
            );
            return None;
        }
        let session = sessions_guard.get_or_create(&session_id).await;
        session
            .messages
            .iter()
            .rev()
            .take(BTW_TRANSCRIPT_TAIL_MESSAGES)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
    };

    // One aside per (profile, session) at a time — a second `/btw` while the
    // first is still answering is rejected, not queued.
    // Canonical default-profile key: absent profile means MAIN_PROFILE_ID
    // everywhere else (sessions lookup, runtime resolution) — the busy gate
    // must agree or an unscoped and a _main-routed aside race the same
    // effective session.
    let busy_key = (
        active_profile_id
            .clone()
            .unwrap_or_else(|| MAIN_PROFILE_ID.to_owned()),
        session_id.clone(),
    );
    {
        let Ok(mut in_flight) = btw_in_flight_sessions().lock() else {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error("btw in-flight registry poisoned"),
            );
            return None;
        };
        if !in_flight.insert(busy_key.clone()) {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::invalid_request("a btw aside is already answering for this session")
                    .with_data(json!({ "kind": "btw_busy" })),
            );
            return None;
        }
    }
    let in_flight_guard = BtwInFlightGuard(busy_key);

    // Everything past validation runs detached — the read loop must not wait
    // out a 30s provider call.
    let ws = ws.clone();
    let state = state.clone();
    let ledger = ledger.clone();
    let active_turns = active_turns.clone();
    let aside_profile_id = in_flight_guard.0.0.clone();
    let task = tokio::spawn(async move {
        let _in_flight_guard = in_flight_guard;

        // Live-activity digest from the ledger replay window's tail.
        let activity_lines = ledger
            .snapshot_with_cursor(&session_id, None)
            .map(|(replayed, _)| {
                replayed
                    .iter()
                    .filter_map(|event| match &event.event {
                        UiProtocolLedgerEvent::Notification(notification) => {
                            btw_activity_line(notification)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let activity_tail = activity_lines
            .iter()
            .rev()
            .take(BTW_ACTIVITY_TAIL_EVENTS)
            .rev()
            .cloned()
            .collect::<Vec<_>>();

        // The transcript tail only holds COMMITTED messages; a "what are you
        // working on?" mid-turn needs the answer being written right now.
        // Resolve the session's CURRENT turn through the registry and require
        // a NON-TERMINAL state — `ActiveTurn` entries are deliberately
        // retained in `Terminal(_)` for idempotent interrupts, and a finished
        // turn's leftover tail must not masquerade as in-flight (its text is
        // already in the committed transcript above). TurnId keying then
        // guarantees the tail belongs to exactly that turn — including turns
        // admitted a moment ago whose task has not streamed yet (their fresh
        // TurnId simply has no draft).
        let live_turn_id = {
            let registry = active_turns.lock().await;
            match registry.get(&session_id) {
                Some(entry) => {
                    // Profile check: the registry is process-global and keyed
                    // by bare SessionKey — two profiles' same-named sessions
                    // (supported for bare `web-*` ids) must never cross-read
                    // each other's stream.
                    if entry.profile_id != aside_profile_id {
                        None
                    } else {
                        let state = entry.state.lock().await;
                        if matches!(*state, TurnState::Terminal(_)) {
                            None
                        } else {
                            Some(entry.turn_id.clone())
                        }
                    }
                }
                None => None,
            }
        };
        let live_draft_tail = live_turn_id
            .as_ref()
            .map(|turn_id| btw_live_draft_tail(&session_id, turn_id))
            .unwrap_or_default();

        // The profile's shared provider chain — same profile the transcript
        // came from. `profile_runtime` also carries the data dir for the
        // usage ledger; the test override provides a bare provider only.
        let (llm, profile_runtime): (
            Arc<dyn octos_llm::LlmProvider>,
            Option<Arc<crate::runtime::ProfileRuntime>>,
        ) = match btw_test_provider() {
            Some(llm) => (llm, None),
            None => {
                match ensure_session_profile_runtime(&state, active_profile_id.as_deref()).await {
                    Ok(Some(runtime)) => (runtime.llm.clone(), Some(runtime)),
                    Ok(None) => {
                        let _ = send_rpc_error(
                            &ws,
                            Some(id),
                            RpcError::runtime_not_ready(
                                "no LLM provider available for this session",
                            ),
                        );
                        return;
                    }
                    Err(error) => {
                        let _ = send_rpc_error(&ws, Some(id), error);
                        return;
                    }
                }
            }
        };

        let messages = build_btw_messages(
            &transcript_tail,
            &activity_tail,
            &live_draft_tail,
            &question,
        );
        let config = btw_chat_config();
        // `&[]` tool specs IS the "no tools" restriction — the model cannot
        // call what it is never offered.
        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(BTW_TIMEOUT_SECS),
            llm.chat(&messages, &[], &config),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                let _ = send_rpc_error(
                    &ws,
                    Some(id),
                    RpcError::internal_error(format!("btw aside failed: {error}")),
                );
                return;
            }
            Err(_elapsed) => {
                let _ = send_rpc_error(
                    &ws,
                    Some(id),
                    RpcError::internal_error(format!(
                        "btw aside timed out after {BTW_TIMEOUT_SECS}s"
                    )),
                );
                return;
            }
        };

        // The answering SLOT's metadata (codex round-1 P2): `llm` is a
        // retry/failover chain, so re-asking `model_id()` after the fact can
        // name the primary lane rather than the fallback that answered.
        let metadata = llm.provider_metadata_for_index(response.provider_index);

        // Paid aside → usage ledger (codex round-1 P2), same shape as AppUI
        // turns but on an aside-specific channel so analytics can split them.
        if let Some(runtime) = profile_runtime.as_ref() {
            match PersistentUsageLedger::open(&runtime.data_dir).await {
                Ok(usage_ledger) => {
                    let model = (!metadata.model.is_empty()).then(|| metadata.model.clone());
                    let estimated_cost_usd =
                        model.as_deref().and_then(model_pricing).map(|pricing| {
                            pricing.cost_with_cache_for_metadata(
                                &metadata,
                                response.usage.input_tokens,
                                response.usage.output_tokens,
                                response.usage.cache_read_tokens,
                                response.usage.cache_write_tokens,
                            )
                        });
                    let cost_source = if estimated_cost_usd.is_some() {
                        UsageCostSource::CatalogEstimate
                    } else {
                        UsageCostSource::Unavailable
                    };
                    let event = UsageEvent::completed_run(
                        active_profile_id
                            .clone()
                            .unwrap_or_else(|| MAIN_PROFILE_ID.to_owned()),
                        session_id.0.clone(),
                        format!("btw-{id}"),
                        (!metadata.provider.is_empty()).then(|| metadata.provider.clone()),
                        model,
                        metadata.endpoint.clone(),
                        u64::from(response.usage.input_tokens),
                        u64::from(response.usage.output_tokens),
                        estimated_cost_usd,
                        cost_source,
                        "appui_btw",
                        None,
                    )
                    .with_cache_read_tokens(u64::from(response.usage.cache_read_tokens))
                    .with_cache_write_tokens(u64::from(response.usage.cache_write_tokens));
                    if let Err(error) = usage_ledger.record(event).await {
                        warn!(
                            session = %session_id.0,
                            error = %error,
                            "failed to record btw aside usage event"
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        session = %session_id.0,
                        error = %error,
                        "usage ledger unavailable; btw aside not recorded"
                    );
                }
            }
        }

        let answer = response
            .content
            .map(|content| content.trim().to_owned())
            .filter(|content| !content.is_empty());
        let Some(answer) = answer else {
            let _ = send_rpc_error(
                &ws,
                Some(id),
                RpcError::internal_error("btw aside returned an empty answer"),
            );
            return;
        };

        let result = octos_core::ui_protocol::SessionBtwResult {
            session_id,
            answer,
            model: Some(metadata.model.clone()),
        };
        send_serialized_rpc_result(
            &ws,
            id,
            octos_core::ui_protocol::methods::SESSION_BTW,
            result,
        );
    });
    Some(task)
}

#[derive(Debug, Clone, Copy)]
struct HydrateIncludeSet {
    messages: bool,
    threads: bool,
    turns: bool,
    pending_approvals: bool,
}

impl HydrateIncludeSet {
    fn from_request(include: &[String]) -> Self {
        if include.is_empty() {
            // Empty / absent = include all (UPCR-2026-009).
            return Self {
                messages: true,
                threads: true,
                turns: true,
                pending_approvals: true,
            };
        }
        let mut set = Self {
            messages: false,
            threads: false,
            turns: false,
            pending_approvals: false,
        };
        for token in include {
            match token.as_str() {
                hydrate_sections::MESSAGES => set.messages = true,
                hydrate_sections::THREADS => set.threads = true,
                hydrate_sections::TURNS => set.turns = true,
                hydrate_sections::PENDING_APPROVALS => set.pending_approvals = true,
                _ => {} // Unknown tokens silently dropped per UPCR.
            }
        }
        set
    }
}

/// Build the thread-graph projection used by both `session/hydrate` and
/// `thread/graph/get`. Returns `(threads, orphans)`.
fn build_thread_graph_entries(session: &octos_bus::Session) -> (Vec<ThreadGraphEntry>, Vec<u64>) {
    use std::collections::BTreeMap;

    // Group messages by thread_id, recording each message's enumerated
    // index (its `seq` for wire purposes).
    let mut groups: BTreeMap<String, Vec<(u64, &Message)>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut orphans: Vec<u64> = Vec::new();
    for (seq, msg) in session.messages.iter().enumerate() {
        let Some(tid) = msg.thread_id.as_ref() else {
            // System messages have no thread_id; skip them (consistent
            // with `Session::threads()`). Non-system messages without a
            // thread_id are orphans.
            if !matches!(msg.role, MessageRole::System) {
                orphans.push(seq as u64);
            }
            continue;
        };
        if !groups.contains_key(tid) {
            order.push(tid.clone());
        }
        groups
            .entry(tid.clone())
            .or_default()
            .push((seq as u64, msg));
    }

    let mut entries: Vec<ThreadGraphEntry> = Vec::with_capacity(order.len());
    for tid in order {
        let members = groups.remove(&tid).unwrap_or_default();
        // Find the rooting user message (first User in the thread). If
        // there is no user message in the group, the thread is anchored
        // on its first member regardless of role.
        let root = members
            .iter()
            .find(|(_, msg)| matches!(msg.role, MessageRole::User))
            .copied()
            .or_else(|| members.first().copied());
        let Some((root_seq, root_msg)) = root else {
            // Empty group is unreachable but harmless: every key in
            // `groups` was inserted with at least one member.
            continue;
        };
        let message_seqs: Vec<u64> = members.iter().map(|(seq, _)| *seq).collect();
        entries.push(ThreadGraphEntry {
            thread_id: tid,
            root_seq,
            root_client_message_id: root_msg.client_message_id.clone(),
            // The `Message` struct does not carry a typed `turn_id` today
            // (PR-F in the structural plan adds it). Until then, leave the
            // wire field absent for legacy rows.
            turn_id: None,
            message_seqs,
            // Status is populated from the active-turn registry by the
            // turn projection; without a typed `turn_id` link we surface
            // `unknown` here. Sibling UPCR-2026-011 fills in the per-turn
            // detail via `turn/state/get`.
            status: thread_status::UNKNOWN.to_owned(),
        });
    }

    // Sort by root_seq for deterministic output (matches
    // `Session::threads()` chronological ordering).
    entries.sort_by_key(|entry| entry.root_seq);
    orphans.sort_unstable();
    (entries, orphans)
}

/// Translate the in-memory `TurnState` into the wire enum.
fn turn_state_to_lifecycle(state: &TurnState) -> TurnLifecycleState {
    match state {
        TurnState::Active => TurnLifecycleState::Active,
        TurnState::Interrupting { .. } => TurnLifecycleState::Interrupting,
        TurnState::Terminal(reason) => match reason {
            TerminalReason::Completed => TurnLifecycleState::Completed,
            TerminalReason::Errored => TurnLifecycleState::Errored,
            TerminalReason::Interrupted => TurnLifecycleState::Interrupted,
        },
    }
}

#[derive(Debug, Default, Clone)]
struct TurnLedgerProjection {
    state: Option<TurnLifecycleState>,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    thread_id: Option<String>,
}

/// Project a turn's lifecycle from the durable ledger event stream. Walks
/// the events for the session looking for lifecycle notifications and
/// canonical v2 envelopes referencing the target `turn_id`. Returns
/// `state = None` when the ledger has no record.
fn project_turn_from_ledger(
    target: &TurnId,
    events: &[LedgeredUiProtocolEvent],
) -> TurnLedgerProjection {
    let mut projection = TurnLedgerProjection::default();
    for ev in events {
        let UiProtocolLedgerEvent::Notification(notification) = &ev.event else {
            continue;
        };
        match notification {
            UiNotification::TurnStarted(started) if started.turn_id == *target => {
                projection.started_at = Some(started.timestamp);
                if projection.state.is_none() {
                    projection.state = Some(TurnLifecycleState::Active);
                }
            }
            UiNotification::TurnCompleted(completed) if completed.turn_id == *target => {
                projection.completed_at = Some(Utc::now());
                projection.state = Some(TurnLifecycleState::Completed);
            }
            UiNotification::TurnError(errored) if errored.turn_id == *target => {
                projection.completed_at = Some(Utc::now());
                projection.state = Some(if errored.code == "interrupted" {
                    TurnLifecycleState::Interrupted
                } else {
                    TurnLifecycleState::Errored
                });
            }
            UiNotification::EnvelopeV2(envelope)
                if envelope.envelope.turn_id == target.0.to_string()
                    && projection.thread_id.is_none() =>
            {
                projection.thread_id = Some(envelope.envelope.thread_id.clone());
            }
            _ => {}
        }
    }
    projection
}

/// Combine the active-turn registry view with the ledger projection to
/// build the `turns` section of `session/hydrate`. Output is sorted by
/// `started_at` so consumers render turns in lifecycle order.
async fn collect_session_turns(
    session_id: &SessionKey,
    active_turns: &SharedActiveTurns,
    events: &[LedgeredUiProtocolEvent],
    threads: &[ThreadGraphEntry],
) -> Vec<HydratedTurn> {
    use std::collections::HashMap;

    // First: collect every turn_id we've seen in the ledger.
    let mut projections: HashMap<TurnId, TurnLedgerProjection> = HashMap::new();
    for ev in events {
        let UiProtocolLedgerEvent::Notification(notification) = &ev.event else {
            continue;
        };
        let turn_id = match notification {
            UiNotification::TurnStarted(e) => Some(e.turn_id.clone()),
            UiNotification::TurnCompleted(e) => Some(e.turn_id.clone()),
            UiNotification::TurnError(e) => Some(e.turn_id.clone()),
            _ => None,
        };
        let Some(turn_id) = turn_id else {
            continue;
        };
        if !projections.contains_key(&turn_id) {
            projections.insert(turn_id.clone(), TurnLedgerProjection::default());
        }
    }
    for turn_id in projections.keys().cloned().collect::<Vec<_>>() {
        let proj = project_turn_from_ledger(&turn_id, events);
        projections.insert(turn_id, proj);
    }

    // Overlay the active-turn registry's live state for the active turn,
    // if any.
    {
        let registry = active_turns.lock().await;
        if let Some(entry) = registry.get(session_id) {
            let live = {
                let state = entry.state.lock().await;
                turn_state_to_lifecycle(&state)
            };
            let proj = projections.entry(entry.turn_id.clone()).or_default();
            proj.state = Some(live);
        }
    }

    // Backfill thread_id from the thread graph when the ledger projection
    // didn't surface one yet for this turn.
    let mut turns: Vec<HydratedTurn> = projections
        .into_iter()
        .map(|(turn_id, proj)| {
            let thread_id = proj.thread_id.clone().or_else(|| {
                threads
                    .iter()
                    .find(|t| t.turn_id.as_ref() == Some(&turn_id))
                    .map(|t| t.thread_id.clone())
            });
            HydratedTurn {
                turn_id,
                state: proj.state.unwrap_or(TurnLifecycleState::Unknown),
                started_at: proj.started_at,
                completed_at: proj.completed_at,
                thread_id,
            }
        })
        .collect();
    turns.sort_by_key(|t| t.started_at.unwrap_or_else(Utc::now));
    turns
}

fn send_serialized_rpc_result<T: Serialize>(
    ws: &WsConnection,
    id: String,
    method: &str,
    result: T,
) {
    match serde_json::to_value(result) {
        Ok(result) => {
            let _ = send_rpc_result(ws, id, result);
        }
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!("failed to serialize {method} result: {error}")),
            );
        }
    }
}

/// `launch/resolve` — the pre-session launch probe. Resolves the launching
/// profile (requested `--profile` → folder-sticky → global default) and reports
/// whether the client should resume the folder's conversation, activate a new
/// one, or choose among profiles. Delegates to the tested
/// [`crate::runtime::launch`] decision core.
async fn handle_launch_resolve(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: octos_core::ui_protocol::LaunchResolveParams,
) {
    match resolve_launch_result(state, connection_profile_id, features, &params) {
        Ok(result) => {
            send_serialized_rpc_result(
                ws,
                id,
                octos_core::ui_protocol::methods::LAUNCH_RESOLVE,
                &result,
            );
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

/// Resolution core for [`handle_launch_resolve`], split out so the error paths
/// are a single `?`-chain. Applies the SAME cwd safety gates as `session/open`
/// and `session/list` before scanning the project's `.octos` store.
fn resolve_launch_result(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    params: &octos_core::ui_protocol::LaunchResolveParams,
) -> Result<octos_core::ui_protocol::LaunchResolveResult, RpcError> {
    use crate::runtime::launch::{LaunchDecision, resolve_launch_decision, scan_folder_sessions};
    use octos_core::ui_protocol::{LaunchDecisionKind, LaunchResolveResult};

    // The method is advertised only when `session.workspace_cwd.v1` is
    // negotiated; reject a client that calls it without the feature.
    if !features.session_workspace_cwd {
        return Err(RpcError::invalid_params(
            "launch/resolve requires feature session.workspace_cwd.v1",
        )
        .with_data(json!({
            "kind": "feature_required",
            "feature": UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
        })));
    }

    let cwd = params.cwd.trim();
    if cwd.is_empty() {
        return Err(RpcError::invalid_params(
            "launch/resolve requires a non-empty cwd",
        ));
    }
    // Path-safety ONLY (canonicalize + banned-root reject) — the SAME check the
    // write path runs before materializing `<cwd>/.octos`. Crucially NOT the
    // full `validate_session_workspace_allowed`, which additionally requires a
    // materialized `SessionRuntime` for the routed profile: launch/resolve is a
    // read-only decision that runs BEFORE any session is opened (and, in solo
    // `--stdio`, before any profile runtime is lazily bootstrapped), so gating
    // it on a loaded runtime made every Resume / CrossProfile / NoProfile
    // outcome unreachable — the folder always fell through to `activate`, or a
    // bare launch got a spurious `cwd_runtime_unavailable`. Found by the
    // launch-flow soak against a real `octos serve --stdio`.
    let workspace_root = canonical_existing_dir(cwd)?;
    validate_session_workspace_path_safety(&workspace_root)?;

    // Known profiles = every launchable brain that EXISTS, sourced from the
    // persistent `ProfileStore` (enabled, top-level), NOT just `state.profiles`
    // — the in-memory runtime map, which in solo `--stdio` is populated lazily
    // on `session/open` and so is EMPTY at bare-launch time. A profile is
    // launchable whether or not its runtime is loaded yet, so the scanner must
    // see its folder store and the default resolver must be able to name it.
    // Unioned with `state.profiles.keys()` so eager-load deployments stay
    // covered. A since-deleted profile's leftover store dir is ignored (the
    // scanner is bounded by this set). `BTreeSet` yields a sorted, deduped
    // listing for a deterministic default + cross-profile result. Empty (no
    // profiles) → `NoProfile`.
    let mut known_set: std::collections::BTreeSet<String> =
        state.profiles.keys().cloned().collect();
    if let Some(store) = state.profile_store.as_ref() {
        if let Ok(profiles) = store.list() {
            for profile in profiles {
                if profile.enabled && profile.parent_id.is_none() {
                    known_set.insert(profile.id);
                }
            }
        }
    }
    let known_profiles: Vec<String> = known_set.into_iter().collect();

    // Global default. An explicit user-chosen `default-profile` pointer (set via
    // the onboarding "make default" flow) wins when it still names a known
    // profile; otherwise fall back to the connection's own profile when
    // registered, else `_main` when present, else the first known profile.
    // `None` (→ `NoProfile`) only when no profile exists at all.
    let persisted_default = profile_store(state)
        .ok()
        .and_then(|store| store.default_profile())
        .filter(|candidate| known_profiles.iter().any(|known| known == candidate));
    let default_profile: Option<String> = persisted_default
        .or_else(|| {
            connection_profile_id
                .map(str::to_string)
                .filter(|candidate| known_profiles.iter().any(|known| known == candidate))
        })
        .or_else(|| {
            known_profiles
                .iter()
                .find(|known| known.as_str() == MAIN_PROFILE_ID)
                .cloned()
        })
        .or_else(|| known_profiles.first().cloned());

    let folder = scan_folder_sessions(&workspace_root, &known_profiles);
    let decision = resolve_launch_decision(
        params.profile_id.as_deref(),
        default_profile.as_deref(),
        &folder,
    );

    Ok(match decision {
        LaunchDecision::Resume { profile_id } => LaunchResolveResult {
            decision: LaunchDecisionKind::Resume,
            resolved_profile: Some(profile_id),
            existing_profiles: Vec::new(),
        },
        LaunchDecision::NeedsActivation { profile_id } => LaunchResolveResult {
            decision: LaunchDecisionKind::Activate,
            resolved_profile: Some(profile_id),
            existing_profiles: Vec::new(),
        },
        LaunchDecision::CrossProfile {
            launching_profile,
            existing_profiles,
        } => LaunchResolveResult {
            decision: LaunchDecisionKind::CrossProfile,
            resolved_profile: Some(launching_profile),
            existing_profiles,
        },
        LaunchDecision::NoProfile => LaunchResolveResult {
            decision: LaunchDecisionKind::NoProfile,
            resolved_profile: None,
            existing_profiles: Vec::new(),
        },
    })
}

fn session_key_with_optional_topic(session_id: &SessionKey, topic: Option<&str>) -> SessionKey {
    let Some(topic) = topic.map(str::trim).filter(|topic| !topic.is_empty()) else {
        return session_id.clone();
    };
    SessionKey(format!("{}#{topic}", session_id.base_key()))
}

fn appui_history_last_non_empty_assistant_after(history: &[Message], pre: usize) -> Option<String> {
    history
        .iter()
        .filter(|message| matches!(message.role, MessageRole::Assistant))
        .enumerate()
        .filter(|(idx, _)| *idx >= pre)
        .filter(|(_, message)| !message.content.is_empty())
        .last()
        .map(|(_, message)| message.content.clone())
}

/// #1134 — pick the assistant text that should be fed to
/// `apply_self_paced_response`.
///
/// Prefers `captured_response_content` (the agent_task's EndTurn
/// payload — the source of truth for `<<loop-next-in: ...>>`). Empty
/// content is treated as "no reply" so a blank EndTurn does not
/// stamp a default-delay reschedule and matches the non-empty filter
/// the session-history fallback applies.
///
/// Falls back to `history_fallback` (typically
/// [`appui_history_last_non_empty_assistant_after`]) only when the
/// captured content is `None` — i.e. the oneshot didn't fire
/// (interrupt / agent error / panic). The pre-#1134 shape used
/// `history_fallback` unconditionally, which is wrong when a
/// spawn_only / send_file path persists a background assistant row
/// AFTER the model's final reply: `.last()` selects the background
/// row and the LLM's reschedule hint never reaches the loop
/// scheduler.
#[cfg_attr(not(test), allow(dead_code))]
fn appui_loop_assistant_reply_for_self_paced(
    captured_response_content: Option<&str>,
    history_fallback: Option<String>,
) -> Option<String> {
    match captured_response_content {
        Some(content) if !content.is_empty() => Some(content.to_owned()),
        Some(_) => None,
        // No capture (interrupt / agent error before EndTurn): use the
        // history reply if present, else `Some("")`. Returning `Some("")`
        // for a true no-reply is deliberate — the empty reply carries no
        // `<<loop-next-in: …>>` sentinel, so `apply_self_paced_response`
        // stamps the DEFAULT delay. The old `None` here parked the loop at
        // `next_run_at_ms: None`, which the due-scan never visits again —
        // one interrupted turn silently killed the loop.
        None => Some(history_fallback.unwrap_or_default()),
    }
}

/// Map the wire-level reasoning effort (octos-core) to octos-llm's enum.
fn reasoning_effort_from_wire(
    level: octos_core::ui_protocol::ReasoningEffortLevel,
) -> octos_llm::ReasoningEffort {
    use octos_core::ui_protocol::ReasoningEffortLevel as L;
    use octos_llm::ReasoningEffort as E;
    match level {
        L::Low => E::Low,
        L::Medium => E::Medium,
        L::High => E::High,
        L::Max => E::Max,
    }
}

/// Post-terminal `spawn_only` drain: which progress event types must NOT be
/// forwarded to clients after the foreground turn has already emitted its
/// terminal `turn/completed`.
///
/// - `done` / `error`: the agent main loop already emitted the terminal turn
///   signal before the drain started; re-forwarding would double-emit.
/// - `token`: a late assistant delta carries the *foreground* turn's id (the
///   drain reuses the turn's `progress_context`). On legacy clients that arm
///   `live_reply` from `message_delta`, a delta for an already-completed turn
///   resurrects it and latches the input gate forever — the "queued N messages
///   after active turn" TUI/web wedge. The background `spawn_only` task reports
///   its real progress via `task_*` / `tool_*` / `file_*` events (all still
///   forwarded), never via raw foreground tokens, so dropping these loses
///   nothing user-facing. Pairs with the octoscode client guard that ignores
///   deltas for already-terminal turns (belt + suspenders).
///
/// Chars of a background REPORT result inlined into the parent conversation.
/// At or below this, the full text is inlined; above it, a preview of this
/// size is inlined plus a recovery pointer. The old behaviour (300-char
/// preview, no pointer) starved the parent of multi-KB child reports — and
/// with `read_task_output` returning empty for spawn children at the time,
/// models concluded the result "was lost" and re-did or overwrote the
/// child's work (mini4 re-review forensic, 2026-07-17).
const SPAWN_REPORT_INLINE_CAP_CHARS: usize = 4000;

/// Format a completed background task's REPORT result for injection into the
/// parent conversation: full text when small, else a bounded preview plus an
/// actionable pointer at `read_task_output` (which serves the
/// supervisor-recorded full text — see `TaskSupervisor::record_final_output`).
fn format_spawn_report_announcement(
    task_label: &str,
    raw_content: &str,
    task_id: Option<&str>,
) -> String {
    let mut chars = raw_content.chars();
    let preview: String = chars.by_ref().take(SPAWN_REPORT_INLINE_CAP_CHARS).collect();
    if chars.next().is_none() {
        // Fits under the cap — inline the whole thing.
        return format!("✅ **{task_label}** completed.\n\n{raw_content}");
    }
    let recovery = match task_id {
        Some(id) => format!("`read_task_output(task_handle=\"{id}\")`"),
        None => {
            "`read_task_output` with this task's handle from `check_background_tasks`".to_string()
        }
    };
    format!(
        "✅ **{task_label}** completed.\n\n{preview}…\n\n_[preview truncated at \
         {SPAWN_REPORT_INLINE_CAP_CHARS} chars — retrieve the FULL result with {recovery}]_"
    )
}

fn drain_should_skip_event(event_type: Option<&str>) -> bool {
    matches!(
        event_type,
        Some("done") | Some("error") | Some("token") | Some("reasoning_chunk")
    )
}

fn normalize_tool_context(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some(value.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn run_standalone_turn(
    ws: WsConnection,
    state: Arc<AppState>,
    ledger: Arc<UiProtocolLedger>,
    contracts: Arc<UiProtocolContractStores>,
    features: ConnectionUiFeatures,
    params: TurnStartParams,
    prompt: String,
    routed_profile_id: Option<String>,
    turn_state: Arc<TokioMutex<TurnState>>,
    mut interrupt_rx: mpsc::Receiver<()>,
    // `turn/steer` pending-input buffer for THIS turn (codex
    // `TurnState.pending_input`). The RPC pushes into it under the
    // active-turns registry lock; here it is threaded onto the per-turn
    // agent, whose loop drains it at each iteration boundary. The drained
    // callback registered below persists each steer row through the
    // canonical session path (same commit-observer emit the `turn/start`
    // prompt row gets) the moment it is folded into the conversation.
    // `None` for non-steerable turns.
    steer_buffer: Option<octos_agent::SharedSteerBuffer>,
    // #1128 codex P1 re-review #2 — when this turn is draining a
    // self-paced or maintenance LoopFire continuation, pass the loop
    // id here so `run_standalone_turn` can re-schedule it from the
    // model's `<<loop-next-in: ...>>` reply hint AFTER the assistant
    // message has been persisted into the per-session
    // `SessionRuntime.sessions` manager (which is the source of truth
    // for the persisted turn). Earlier shapes captured `pre_assistant_count`
    // off `state.sessions` (AppState's legacy manager) and never saw
    // the just-written reply.
    loop_id_for_self_paced: Option<String>,
    // OLP-CTRL 回合 4 (消费权归一): `true` ONLY when this turn drains a
    // STEER continuation — the sole turn allowed to read-and-clear the
    // reviewer-notes sidecar and emit the steer_consumed receipt. Every
    // other turn (interactive, loop, goal) must NOT swallow a steer
    // (round-2's coincidental consume was exactly that leak).
    _is_steer_continuation_turn: bool,
    // OLP-CTRL #8c ② — when this is a steer continuation turn, the exact
    // steer LINE it must consume (enqueue_ts, text) from the sidecar, so
    // consumption is per-line (exactly-once), never a whole-file clear
    // that would drop sibling steers enqueued but not yet run.
    _steer_line_to_consume: Option<(String, String)>,
) {
    let session_id = params.session_id.clone();
    let turn_id = params.turn_id.clone();
    let started = UiNotification::TurnStarted(octos_core::ui_protocol::TurnStartedEvent {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        timestamp: Utc::now(),
        // UPCR-2026-014 (M9-α-9): legacy WS turn-start path; topic is
        // not in scope here (the SSE bridge surfaces it via α-9).
        topic: None,
    });
    // turn/started is lifecycle. If the client cannot receive it we may as
    // well stop now — the rest of the turn is wasted work. Per FIX-03,
    // transition the turn to a terminal state so the registry doesn't keep
    // an orphaned `Active` entry.
    if send_notification_lifecycle(&ws, &ledger, started).is_err() {
        let _ = transition_to_terminal_settling_steers(
            &turn_state,
            TerminalReason::Errored,
            steer_buffer.as_ref(),
            SteerReturnSink::Live {
                ws: &ws,
                ledger: &ledger,
            },
            &session_id,
            &turn_id,
        )
        .await;
        contracts.scopes.evict_turn(&session_id, &turn_id);
        return;
    }

    // M11-F: resolve the per-session view through the
    // `ProfileRuntime` + `SessionRuntimeCache` path only. The legacy
    // single-agent fallback (`state.agent` / `validate_runtime`) was
    // deleted — `octos serve` bootstraps every profile in
    // `ProfileStore::list()` at startup, so an unregistered profile
    // here is a configuration bug, not a runtime fallback. Fail closed
    // with a typed `runtime_unavailable` terminal so the client sees
    // the same error shape it would for a SessionRuntime::bootstrap
    // failure.
    let active_profile_id = session_id
        .profile_id()
        .map(ToOwned::to_owned)
        .or(routed_profile_id.clone());
    let Some(profile_runtime) =
        resolve_session_profile_runtime(&state, active_profile_id.as_deref())
    else {
        let error = profile_runtime_unavailable_message(
            &state,
            active_profile_id.as_deref().unwrap_or("<unset>"),
        );
        try_emit_terminal(
            &turn_state,
            TerminalReason::Errored,
            &ws,
            &ledger,
            &session_id,
            &turn_id,
            Some(("runtime_unavailable", error.as_str())),
            None,
            steer_buffer.as_ref(),
            // Outer-loop #4 (§4.2): peer sessions release their held slot at the interrupted terminal; None = no peer context on this path.
        )
        .await;
        contracts.scopes.evict_turn(&session_id, &turn_id);
        return;
    };

    let workspace_profile_id = workspace_profile_scope(active_profile_id.as_deref(), &session_id);
    // Keep two meanings separate: every opened session has an effective
    // workspace root for tools/goals/UI, but only a Tier-1 client cwd or Tier-2
    // operator default is a runtime hint that may relocate transcript storage.
    // A derived Tier-3 workspace therefore yields `hint == None`.
    let workspace_binding = session_workspaces().snapshot(&workspace_profile_id, &session_id);
    let hint = workspace_binding
        .as_ref()
        .and_then(|binding| binding.runtime_hint.clone());
    let permissions_epoch = state.session_cache.session_generation(&session_id);
    let permissions = match effective_permissions_for_session(&state, &session_id) {
        Ok(permissions) => permissions,
        Err(error) => {
            let message = error.message.clone();
            try_emit_terminal(
                &turn_state,
                TerminalReason::Errored,
                &ws,
                &ledger,
                &session_id,
                &turn_id,
                Some(("permission_denied", message.as_str())),
                None,
                steer_buffer.as_ref(),
                // Outer-loop #4 (§4.2): peer sessions release their held slot at the interrupted terminal; None = no peer context on this path.
            )
            .await;
            contracts.scopes.evict_turn(&session_id, &turn_id);
            return;
        }
    };
    let session_runtime = match state
        .session_cache
        .get_or_init_with_permissions(
            &profile_runtime,
            session_id.clone(),
            hint,
            permissions,
            permissions_epoch,
        )
        .await
    {
        Ok(rt) => rt,
        Err(error) => {
            try_emit_terminal(
                &turn_state,
                TerminalReason::Errored,
                &ws,
                &ledger,
                &session_id,
                &turn_id,
                Some(("runtime_unavailable", &error.to_string())),
                None,
                steer_buffer.as_ref(),
                // Outer-loop #4 (§4.2): peer sessions release their held slot at the interrupted terminal; None = no peer context on this path.
            )
            .await;
            contracts.scopes.evict_turn(&session_id, &turn_id);
            return;
        }
    };
    // Per-project ledger isolation (#1666): every event this turn appends
    // must land under the session's per-cwd storage identity. `session/open`
    // registered it already for the normal flow; re-registering here is an
    // idempotent no-op that also covers turns whose runtime re-materialized
    // (e.g. after cache eviction) without a fresh open.
    register_session_ledger_scope(&state, &ledger, &session_runtime);
    let usage_profile_id = active_profile_id
        .clone()
        .or_else(|| session_id.profile_id().map(ToOwned::to_owned))
        .unwrap_or_else(|| profile_runtime.profile_id.clone());
    let usage_ledger = match PersistentUsageLedger::open(&session_runtime.profile.data_dir).await {
        Ok(ledger) => Some(Arc::new(ledger)),
        Err(error) => {
            warn!(
                session = %session_id.0,
                profile_id = %usage_profile_id,
                path = %session_runtime.profile.data_dir.join(USAGE_LEDGER_FILE).display(),
                error = %error,
                "usage ledger unavailable; continuing turn without durable usage record"
            );
            None
        }
    };
    // Session-cumulative usage base for cost emissions (codex #1632 P1):
    // this path builds a FRESH agent per turn, so without a seeded base
    // every `cost_update` reported turn-only "session" figures. Hydrate
    // from the ledger this same path writes each completed run to —
    // making the emitted `session_*` figures cover the whole session
    // across turns, reconnects, and the per-turn agent rebuild. The
    // completed-run write below lands before the client can start the
    // next turn on this session, so the next hydration includes it.
    let session_usage_base = octos_agent::SharedSessionUsage::default();
    if let Some(ledger) = usage_ledger.as_ref() {
        match ledger.session_totals(&session_id.to_string()).await {
            Ok(totals) if totals.run_count > 0 => {
                session_usage_base.seed(octos_agent::SessionUsageSnapshot {
                    input_tokens: totals.input_tokens,
                    output_tokens: totals.output_tokens,
                    spend_usd: totals.estimated_cost_usd,
                    priced_runs: if totals.estimated_cost_usd > 0.0 {
                        totals.run_count
                    } else {
                        0
                    },
                });
            }
            Ok(_) => {}
            Err(error) => {
                warn!(
                    session = %session_id.0,
                    error = %error,
                    "failed to hydrate session usage base from ledger"
                );
            }
        }
    }

    // Source the per-session primitives from the SessionRuntime.
    //
    // `tool_registry` is an OWNED `ToolRegistry` we mutate per-turn
    // (`set_background_result_sender`, `register(send_file_tool)`,
    // `supervisor().set_on_change`). We snapshot from the shared
    // `Arc<ToolRegistry>` so per-turn mutation does not race with the
    // cached SessionRuntime.
    let sessions = session_runtime.sessions.clone();
    // #1128 codex P1 re-review #2 — pre-turn assistant-message count
    // snapshot off the SessionRuntime's session manager (the source
    // of truth for persisted turns). Used at end-of-turn to find the
    // model's reply and re-schedule self-paced / maintenance loops.
    let needs_pre_assistant_count = loop_id_for_self_paced.is_some();
    let pre_assistant_count_for_post_turn: Option<usize> = if needs_pre_assistant_count {
        let mut guard = sessions.lock().await;
        let session = guard.get_or_create(&session_id).await;
        Some(
            session
                .get_history(usize::MAX)
                .iter()
                .filter(|message| matches!(message.role, MessageRole::Assistant))
                .count(),
        )
    } else {
        None
    };
    let mut tool_registry = session_runtime.tools.snapshot_excluding(&[]);
    tool_registry.set_active_context(normalize_tool_context(params.tool_context.as_deref()));
    // Stamp the per-turn snapshot with this session's key so
    // `spawn::register_with_lineage` writes `session_key:
    // Some(<session_id.0>)` onto every `BackgroundTask` it tracks
    // (`spawn.rs:2672`). Without this, `snapshot_excluding` clears
    // `session_key` to `None` and the
    // `TaskSupervisor::get_tasks_for_session` filter
    // (`task_supervisor.rs:2237`) drops every task — `session/tasks.list`
    // returns `[]` even after the live supervisor is registered with
    // `SessionTaskQueryStore` below. Mirrors `session_actor.rs:2671`
    // for the WS path.
    tool_registry.set_session_key(session_id.to_string());
    // RFC-0 (#1289): the `activate_tools` meta-tool was removed — no per-turn
    // re-registration/rewiring needed.
    // Slides session structural guardrail — PR #1265 follow-up.
    //
    let workspace_root: Option<PathBuf> = Some(session_runtime.workspace_root.clone());
    let llm_provider: Arc<dyn octos_llm::LlmProvider> = session_runtime.profile.llm.clone();
    let memory_store: Arc<octos_memory::EpisodeStore> = session_runtime.profile.memory.clone();
    let mut agent_config = session_runtime.agent.agent_config();
    // A human-driven turn does not become unattended merely because it
    // arrived over OUP. Local chat/ACP and remote interactive clients share
    // this policy; explicit profile caps still win for either intent.
    agent_config.max_iterations = crate::runtime::turn_policy::max_iterations(
        session_runtime.profile.max_iterations,
        crate::runtime::turn_policy::TurnIntent::Interactive,
    );
    // Per-session reasoning/thinking effort (TUI `/thinking`), persisted
    // server-side so it survives a full serve/TUI restart (in `--stdio` mode a
    // TUI restart respawns the serve; only the disk-backed value reloads).
    //
    // Precedence:
    //   1. A turn that CARRIES `reasoning_effort` wins — apply it AND persist it
    //      for this session so a later restart (or a turn that omits it) sees
    //      the same value.
    //   2. A turn that OMITS it falls back to the persisted stored value, so the
    //      stored choice is authoritative across a restart even before the
    //      client re-sends it.
    //   3. No turn-param and nothing stored → the gateway/profile default is
    //      left untouched (no override).
    // Providers translate the chosen effort per model (no-op for models without
    // a reasoning style).
    let effort_data_dir = sessions.lock().await.data_dir();
    let resolved_effort =
        crate::api::ui_protocol_reasoning_effort::resolve_and_persist_reasoning_effort(
            &effort_data_dir,
            &session_id,
            params.reasoning_effort,
            // A user turn is authoritative: when it omits the effort the user
            // chose "default", so clear the stored override. Server-initiated
            // continuations keep falling back to the stored value.
            true,
        )
        .await;
    if let Some(level) = resolved_effort {
        agent_config.reasoning_effort = Some(reasoning_effort_from_wire(level));
    }
    // Per-session system prompt override. Gateway path reads
    // `data_dir/session_prompts/<topic>.md` for structured-template
    // sessions (`/new slides X`, `/new site X`) and CONCATENATES the
    // session prompt onto its base prompt (see
    // `gateway_runtime.rs:1864-1878` — `format!("{base}\n\n{session_prompt}")`).
    // The AppUI/WS turn path never had any wiring for the session
    // prompt, so slides/site sessions on the web UI fell back to the
    // generic agent prompt and lost their workflow guidance.
    //
    // Mirror the gateway pattern: append the session prompt onto the
    // profile snapshot rather than replacing it. The snapshot carries
    // the profile's memory bank, skills index, persona, and other
    // dynamic context — none of which the session prompt should
    // supplant. The session prompt is workflow-specific guidance that
    // augments rather than replaces.
    // Same refresh-before-snapshot rule as the review path: the cached
    // agent's memory segment must be current before the per-turn agent
    // clones its prompt.
    session_runtime.agent.refresh_prompt_segments().await;
    let combined_memory_segment = session_runtime
        .agent
        .prompt_segment_snapshot(octos_agent::MEMORY_SEGMENT_NAME)
        .unwrap_or_default();
    let volatile_memory_context = octos_agent::volatile_memory_content(
        &combined_memory_segment,
        session_runtime.profile.memory_refresh_enabled,
    );
    // An empty bank needs no memory policy in the model-visible prompt. This
    // keeps fresh stdio/solo sessions from paying for memory instructions when
    // there is no memory to read or update.
    let stable_memory_policy = if should_emit_memory_snapshot(&volatile_memory_context) {
        octos_agent::stable_memory_instructions(session_runtime.profile.memory_refresh_enabled)
    } else {
        String::new()
    };
    let agent_snapshot = session_runtime
        .agent
        .system_prompt_snapshot_replacing_segment(
            octos_agent::MEMORY_SEGMENT_NAME,
            &stable_memory_policy,
        );
    let system_prompt_base = agent_snapshot;

    let slash_ctx = ws_slash::SlashCommandContext {
        sessions: sessions.clone(),
        session_id: session_id.clone(),
        data_dir: session_runtime.profile.data_dir.clone(),
        profile_id: session_id
            .profile_id()
            .map(ToOwned::to_owned)
            .or_else(|| routed_profile_id.clone()),
        workspace_root: Some(session_runtime.workspace_root.clone()),
    };
    if let Some(reply) = ws_slash::try_dispatch_slash_command(&prompt, &slash_ctx).await {
        let user_turn_id = turn_id.0.to_string();
        let user_message = pre_stamp_turn_thread_id(Message::user(prompt.clone()), &user_turn_id);
        let assistant_message = pre_stamp_turn_thread_id(Message::assistant(reply), &user_turn_id);
        {
            let mut mgr = sessions.lock().await;
            let _ = mgr.add_message_with_seq(&session_id, user_message).await;
            let _ = mgr
                .add_message_with_seq(&session_id, assistant_message)
                .await;
        }
        try_emit_terminal(
            &turn_state,
            TerminalReason::Completed,
            &ws,
            &ledger,
            &session_id,
            &turn_id,
            None,
            // Slash-command shortcut bypasses the LLM entirely; the
            // reply is canned and no token meter ran.
            None,
            steer_buffer.as_ref(),
        )
        .await;
        contracts.scopes.evict_turn(&session_id, &turn_id);
        return;
    }

    let raw_history: Vec<Message> = {
        let mut sessions = sessions.lock().await;
        let session = sessions.get_or_create(&session_id).await;
        // Validate the ledger against the canonical source head, not a bounded
        // prompt tail. A compacted snapshot can have source_seq values far
        // above 50; comparing that watermark with `get_history(50).len()` can
        // misclassify a stale snapshot and hide rows appended afterward.
        // Prompt bounding remains ContextManager's projection responsibility.
        session.messages.clone()
    };
    // Resolve a lazily-probed context window before threshold compaction.
    llm_provider.ensure_ready().await;
    let (
        mut history,
        context_manager,
        context_lifecycle_notifications,
        _appui_context_registration,
    ) = appui_context_history_for_agent(
        // Root the context ledger at the session's TRANSCRIPT root, not the
        // profile-global data dir: with `appui.sessions_in_cwd` the
        // transcript relocates to `<cwd>/.octos/<profile>` and a
        // profile-rooted context ledger is SHARED across projects that
        // reuse the same session key — project B's snapshot would beat
        // project A's raw history on rebuild and leak B's conversation
        // into A's LLM context (#1666). Flag-OFF: `sessions_root ==
        // profile.data_dir`, byte-identical.
        &session_runtime.sessions_root,
        &session_id,
        &raw_history,
        &llm_provider,
        session_compaction_llm_enabled(&session_id, &state),
        "appui_pre_turn",
    );
    for notification in context_lifecycle_notifications {
        if features.context_lifecycle_available() {
            let _ = send_notification_durable(&ws, &ledger, notification);
        } else {
            let _ = ledger.append_notification_from(notification, ws.connection_id);
        }
    }

    // For hosted multi-tenant standalone serve, the file API resolves
    // `/api/files/...` against the per-profile data dir (`<server_data>/
    // profiles/<profile>/data`), not the server-wide one. Plugin output
    // must land under the SAME root the file API will check, otherwise
    // `resolve_legacy_file_request` rejects it.
    //
    // The active profile id can come from three places, in order:
    //   1. `session_id.profile_id()` — when the SPA encodes it via
    //      `SessionKey::with_profile`. Bare-channel session ids
    //      (`web-…`) skip this.
    //   2. `routed_profile_id` — derived from the connection's `Host`
    //      header during WS handshake. Hosted admin-token requests land
    //      here; the SPA at `dspfac.crew.ominix.io` matches.
    //   3. The registered `ProfileRuntime`'s `data_dir` when M11-E
    //      materialized the SessionRuntime.
    // Falls back to the server-wide data dir for local sessions / dev.
    let plugin_root_dir = session_runtime.profile.data_dir.clone();

    // C1 fix: create the progress channel + drop counter UP HERE — BEFORE
    // the `enable_persistence` block below — so the supervisor `on_change`
    // callback (which captures clones of these) can be installed BEFORE
    // `enable_persistence`. The orphan-task sweep that runs inside
    // `enable_persistence` fires terminal `mark_failed("orphaned across
    // restart")` transitions; if `on_change` is installed AFTER persistence
    // (the pre-C1 ordering) the sweep's `notify_change` hits
    // `on_change == None` and the `task_updated` event is silently dropped,
    // leaving the TUI task count stuck at "N running". The channel itself is
    // consumed (`progress_rx` drained) much later in the function — only its
    // sender clone needs to exist this early.
    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::channel::<String>(PROGRESS_CHANNEL_CAPACITY);
    let progress_dropped = Arc::new(AtomicU64::new(0));

    // Wire `BackgroundResultSender` + `SendFileTool` so spawn_only tool
    // completions and explicit `send_file` calls persist as assistant
    // messages on the session and reach connected clients through the
    // post-commit canonical v2 envelope path. Without this, the api/serve
    // path drops spawn_only file deliveries on the floor — gateway wires the
    // equivalent in `session_actor.rs::deliver_background_notification`.
    //
    // The canonical persist
    // (`octos_bus::session::persist_message_through_canonical_path`)
    // serialises with other writers via a per-key Tokio mutex, so this is
    // safe to invoke from a `tokio::spawn`-driven background task that may
    // complete after the originating turn has ended. After each persist we
    // invalidate the cached `SessionManager` so `session/hydrate` and
    // `/api/sessions/:id/messages` reads pick up the new row instead of
    // the pre-persist snapshot (matches `ApiChannel::persist_to_session`'s
    // post-write invalidate at `api_channel.rs:1503`).
    //
    // A background commit carries task context through the task-local observer
    // override, which emits exactly one `background_child_completed` payload
    // after the canonical session write succeeds.
    {
        let bg_data_dir = sessions.lock().await.data_dir().to_path_buf();
        let bg_sessions = sessions.clone();
        let bg_session_id = session_id.clone();
        let bg_thread_id = turn_id.0.to_string();
        let bg_turn_id = turn_id.clone();
        let task_state_path = {
            let encoded_base = octos_bus::session::encode_path_component(session_id.base_key());
            let topic = session_id
                .topic()
                .filter(|topic| !topic.is_empty())
                .unwrap_or("default");
            let encoded_topic = octos_bus::session::encode_path_component(topic);
            bg_data_dir
                .join("users")
                .join(encoded_base)
                .join("sessions")
                .join(format!("{encoded_topic}.tasks.jsonl"))
        };
        let task_supervisor = tool_registry.supervisor();
        // PR #1324 follow-up (L3 WS coverage gap): wire the spawn_only
        // post-spawn failure signal BEFORE `enable_persistence` so the
        // orphan-task sweep at `task_supervisor.rs:1164-1166` —
        // which can `mark_failed` resurrected tasks — does NOT
        // silently drop the recovery turn. We re-inject the failure
        // into the global master continuation queue (drained on every
        // `appui_continuation_tick`). `default_agent_orchestrator()`
        // is a process-wide singleton, so the callback survives both
        // the per-turn `tool_registry` drop AND a closed WS
        // connection — on next subscribe, the queued
        // `External("spawn_only_failure")` continuation fires and
        // `master_continuation_prompt` renders the recovery body.
        //
        // #2020: the gateway now enqueues onto this same queue from its own
        // `set_on_failure_signal` (it used to own a separate
        // `ActorMessage::RecoveryHint` inbox), so both runtime modes
        // re-enter through one transport.
        //
        // The per-connection drain filter
        // (`maybe_spawn_appui_master_continuation_runner`) takes
        // exactly one continuation per session per tick, so the
        // dedupe key on the request collapses repeated `mark_failed`
        // calls onto one recovery turn even if `notify_failure`
        // re-fires through a sibling path.
        let progress_tx_for_tasks = progress_tx.clone();
        let task_progress_dropped = progress_dropped.clone();
        let change_profile_id = active_profile_id
            .clone()
            .or_else(|| routed_profile_id.clone())
            .unwrap_or_else(|| MAIN_PROFILE_ID.to_owned());
        task_supervisor.set_on_change(move |task| {
            forward_task_progress_to_channel(
                &progress_tx_for_tasks,
                &task_progress_dropped,
                task,
                Some(change_profile_id.as_str()),
            );
        });
        // Register the per-turn supervisor with `SessionTaskQueryStore`
        // so `session/tasks.list` and `session/status.get` can see live
        // spawn_only tasks after the SPA closes + reopens the chat
        // tab mid-flight. The gateway path does this at
        // `session_actor.rs:2668-2669`; the WS turn handler used to
        // skip it, so `query_json` returned `[]` on reopen, the
        // session-task tracker rendered empty, and the runtime tore
        // down its `watchSession` subscriber when `has_bg_tasks=false`.
        //
        // The store holds a `Weak<TaskSupervisor>` so dropping the
        // per-turn `tool_registry` at end of turn lets the entry get
        // pruned naturally by `lookup_live_supervisor`. `register` is
        // safe to call even when `task_query_store` was wired into
        // `AppState` (production); in tests with `None`, the WS path
        // simply skips the registration without erroring.
        if let Some(store) = state.task_query_store.as_ref() {
            store.register(&session_id, &task_supervisor, &bg_data_dir);
        }
        tool_registry.register(octos_agent::CheckBackgroundTasksTool::new(
            task_supervisor.clone(),
            session_id.to_string(),
        ));
        tool_registry.register(octos_agent::ReadTaskOutputTool::new(
            task_supervisor.clone(),
            session_id.to_string(),
            session_runtime.agent.subagent_output_router().cloned(),
            session_runtime.workspace_root.clone(),
        ));

        // Wire spawn_only contract-satisfied path.
        let payload_sessions = bg_sessions.clone();
        let payload_data_dir = bg_data_dir.clone();
        let payload_session_id = bg_session_id.clone();
        let payload_thread_id = bg_thread_id.clone();
        let payload_turn_id = bg_turn_id.clone();
        let payload_context_manager = context_manager.clone();
        let payload_context_dir = session_runtime.sessions_root.clone();
        let background_result_sender: octos_agent::tools::spawn::BackgroundResultSender =
            std::sync::Arc::new(move |payload: BackgroundResultPayload| {
                let sessions = payload_sessions.clone();
                let data_dir = payload_data_dir.clone();
                let session_id = payload_session_id.clone();
                let originating_thread_id = payload
                    .originating_thread_id
                    .clone()
                    .filter(|tid| !tid.is_empty());
                let thread_id = originating_thread_id
                    .clone()
                    .unwrap_or_else(|| payload_thread_id.clone());
                let task_label = payload.task_label.clone();
                // `effective_envelope_media` carries the artifact list on
                // the background-child payload. The `NotConfigured`
                // `send_file` fallback contributes its sent-file paths;
                // contract-satisfied payloads use their direct media list.
                // OUP persists the SAME media on its canonical completion:
                // replay/hydration verifies the exact durable row against
                // that envelope. Empty per-file companions remain durable
                // but are not visible answer rows. Other channel consumers
                // retain the producer's separate media fields unchanged.
                let media = super::ui_protocol_alpha9_bridge::effective_envelope_media(&payload);
                let kind = payload.kind;
                let raw_content = payload.content.clone();
                let task_id = payload.task_id.clone();
                // Pull the originating user cmid out before the async block
                // so `background_child_completed` can set
                // `response_to_client_message_id`. Empty strings collapse
                // to `None` so the reducer never sees an unusable id.
                let originating_client_message_id = payload
                    .originating_client_message_id
                    .clone()
                    .filter(|s| !s.is_empty());
                let originating_tool_call_id =
                    payload.tool_call_id.clone().filter(|s| !s.is_empty());
                let turn_id = payload_turn_id.clone();
                let context_manager = payload_context_manager.clone();
                let context_dir = payload_context_dir.clone();
                Box::pin(async move {
                    // `trim().is_empty()` so a whitespace-only `raw_content`
                    // (e.g. an emitter that printed just "\n") gets the
                    // friendly "delivered/completed" fallback bubble instead
                    // of a chat row containing just a newline.
                    let content_text = match kind {
                        BackgroundResultKind::Notification => {
                            if raw_content.trim().is_empty() && !media.is_empty() {
                                format!("✅ {} delivered.", task_label)
                            } else {
                                raw_content
                            }
                        }
                        BackgroundResultKind::Report => {
                            if raw_content.trim().is_empty() && !media.is_empty() {
                                format!("✅ {} completed.", task_label)
                            } else {
                                // Mini4 re-review forensic: the old
                                // `take(300) + "…"` preview (no pointer, no
                                // recovery path) starved the parent of a
                                // child's multi-KB report — combined with the
                                // then-empty `read_task_output`, models
                                // concluded the result "was lost" and re-did
                                // or overwrote the child's work.
                                format_spawn_report_announcement(
                                    &task_label,
                                    &raw_content,
                                    task_id.as_deref(),
                                )
                            }
                        }
                    };
                    let task_id_clean = task_id
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    let projection =
                        MessageProjectionOverride::BackgroundChild(BackgroundChildProjection {
                            parent_turn_id: turn_id.0.to_string(),
                            response_to_client_message_id: originating_client_message_id.clone(),
                            task_id: task_id_clean,
                            tool_call_id: originating_tool_call_id.clone(),
                            media: media.clone(),
                        });
                    let persisted = MESSAGE_PROJECTION_OVERRIDE
                        .scope(
                            Some(projection),
                            persist_assistant_with_media(
                                &sessions,
                                &data_dir,
                                &session_id,
                                content_text,
                                media,
                                thread_id,
                                &task_label,
                            ),
                        )
                        .await;
                    if let Some((message, seq)) = persisted.as_ref() {
                        record_appui_context_manager_background_message(
                            &context_dir,
                            &context_manager,
                            &session_id,
                            message,
                            *seq,
                        );
                    } else {
                        tracing::warn!(
                            session_id = %session_id.0,
                            task_label,
                            "background result persist failed; no canonical child envelope emitted"
                        );
                    }
                    persisted.is_some()
                })
            });
        tool_registry.set_background_result_sender(background_result_sender.clone());

        // Build the SendFile channel + path config UP HERE (before
        // SpawnTool construction) so we can hand the same `out_tx` and
        // base-dir set to BOTH the parent SendFileTool registration
        // AND the spawn-child SendFileTool factory. Pre-fix the child
        // registry only inherited builtins + plugins + pipeline_factory
        // — `send_file` was missing, and spawn preflight failed with
        // "required tool(s) not available on this host: send_file"
        // whenever a workspace-contract subagent declared `send_file`
        // among allowed_tools (e.g. mofa_slides post-completion
        // delivery). Live reproducer on mini3 dspfac session
        // slides-1779207239761-u8pt1j.
        let (out_tx, mut out_rx) =
            mpsc::channel::<octos_core::OutboundMessage>(SEND_FILE_CHANNEL_CAPACITY);
        let send_file_base = workspace_root
            .clone()
            .unwrap_or_else(|| bg_data_dir.clone());
        let send_file_extras: Vec<PathBuf> = {
            let mut v = vec![bg_data_dir.clone()];
            if plugin_root_dir != bg_data_dir {
                v.push(plugin_root_dir.clone());
            }
            v
        };
        let send_file_context_session = bg_session_id.0.clone();

        let (spawn_inbound_tx, _spawn_inbound_rx) = mpsc::channel::<InboundMessage>(32);
        let mut spawn_tool = octos_agent::SpawnTool::with_context(
            llm_provider.clone(),
            memory_store.clone(),
            session_runtime.workspace_root.clone(),
            spawn_inbound_tx,
            "api",
            session_id.to_string(),
        )
        .with_provider_policy(tool_registry.provider_policy().cloned())
        // #1607 (codex-review follow-up): inherit the session's effective
        // sandbox (the same `SandboxConfig` the parent `tool_registry` was
        // built from) so child command execution stays confined instead of
        // running on the host.
        .with_sandbox(session_runtime.sandbox.clone())
        .with_agent_config(agent_config.clone())
        .with_task_supervisor(
            task_supervisor.clone(),
            session_id.to_string(),
            task_state_path.clone(),
        )
        // Embed-on-save + recall parity: spawn workers save episodes by
        // default; without the profile's embedder those episodes are
        // stored vectorless and worker episodic recall silently skips.
        .with_optional_embedder(session_runtime.profile.embedder.clone())
        .with_hook_context(octos_agent::HookContext {
            session_id: Some(session_id.to_string()),
            profile_id: Some(session_runtime.profile.profile_id.clone()),
        })
        .with_background_result_sender(background_result_sender);
        if let Some(hooks) = session_runtime.profile.hook_executor.clone() {
            spawn_tool = spawn_tool.with_hooks(hooks);
        }
        if let Some(cache) = session_runtime.agent.file_state_cache().cloned() {
            spawn_tool = spawn_tool.with_parent_file_state_cache(cache);
        }
        if let Some(router) = session_runtime.agent.subagent_output_router().cloned() {
            spawn_tool = spawn_tool.with_parent_subagent_output_router(router);
        }
        if let Some(generator) = session_runtime.agent.subagent_summary_generator().cloned() {
            spawn_tool = spawn_tool.with_parent_subagent_summary_generator(generator);
        }
        let child_context_parent = context_manager.clone();
        // Child (forked sub-agent) context ledgers belong to the parent's
        // project store: with `appui.sessions_in_cwd` on, `sessions_root` is
        // the per-cwd root; profile-rooting them would leak child context
        // across projects sharing a child key (#1666). Flag-OFF: identical.
        let child_context_data_dir = session_runtime.sessions_root.clone();
        let child_context_parent_session = session_id.clone();
        spawn_tool = spawn_tool.with_child_prompt_context_manager_factory(Arc::new(
            move |request: octos_agent::tools::spawn::ChildPromptContextRequest| {
                let child_key = request.child_session_key.clone().unwrap_or_else(|| {
                    let worker_suffix: String = request
                        .worker_id
                        .chars()
                        .map(|ch| {
                            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                                ch
                            } else {
                                '_'
                            }
                        })
                        .collect();
                    format!(
                        "{}#spawn-{}",
                        child_context_parent_session.base_key(),
                        worker_suffix
                    )
                });
                let child_session_id = SessionKey(child_key);
                let child_manager = {
                    let parent = child_context_parent
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let fork = parent.fork_child_history(&ForkPolicy::default());
                    ContextManager::from_forked_child_context(
                        child_session_id.to_string(),
                        request.task_id.clone(),
                        fork,
                    )
                };
                publish_appui_context_status(&child_session_id, &child_manager);
                if let Err(error) = persist_appui_context_snapshot(
                    &child_context_data_dir,
                    &child_session_id,
                    &child_manager,
                ) {
                    warn!(
                        session = %child_session_id.0,
                        error = %error,
                        "failed to persist forked child context manager snapshot"
                    );
                }
                // NB: no `with_context_lifecycle_notify` here (child bridges
                // stay silent, unlike the parent turn bridge). A child's
                // forked context is SEPARATE from the parent's; emitting its
                // compaction events under the parent key would overwrite the
                // client's per-session context gauge with the child's
                // numbers, and under the child key (`…#spawn-…`) no client
                // ever opens the session. Surfacing child compactions needs
                // a dedicated child-attributed event — follow-up.
                Some(Arc::new(AppUiPromptContextBridge::new(
                    child_session_id,
                    child_context_data_dir.clone(),
                    Arc::new(StdMutex::new(child_manager)),
                )))
            },
        ));
        // Child SendFileTool factory: every spawned subagent's registry
        // gets a fresh `SendFileTool` wired to the SAME `out_tx`
        // channel as the parent, so spawn_only `files_to_send`
        // deliveries land via the canonical AppUI persist loop below.
        // Pre-fix (commit 28552bb9d added spawn on AppUI but no child
        // factory) the child registry was missing `send_file`, breaking
        // any subagent that the workspace contract auto-delivered via
        // it.
        {
            let factory_out_tx = out_tx.clone();
            let factory_base = send_file_base.clone();
            let factory_extras = send_file_extras.clone();
            let factory_session = send_file_context_session.clone();
            spawn_tool = spawn_tool.with_child_tool_factory(Arc::new(move || {
                let mut tool = octos_agent::SendFileTool::new(factory_out_tx.clone())
                    .with_base_dir(factory_base.clone());
                for extra in &factory_extras {
                    tool = tool.with_extra_allowed_dir(extra.clone());
                }
                tool.set_context("api", &factory_session);
                Arc::new(tool) as Arc<dyn octos_agent::tools::Tool>
            }));
        }
        tool_registry.register(spawn_tool);
        // RFC-0 (#1289): LRU deferral removed — no base-tool pin needed.

        // Wire the PARENT `send_file` for the legacy non-contract
        // `files_to_send` path and any explicit agent calls. The
        // spawn_only auto-background branch falls back to `send_file`
        // when the workspace contract is `NotConfigured`
        // (`execution.rs:549`) — without this registration, tools like
        // `deep_search` (no default api-mode workspace policy) emit
        // `files_to_send` that have nowhere to land.
        //
        // The `out_tx`/`send_file_base`/`send_file_extras`/`send_file_context_session`
        // bindings were created above (before SpawnTool construction)
        // so the SAME deps are used by both the parent tool and the
        // child factory wired into `spawn_tool`.
        let mut send_file_tool =
            octos_agent::SendFileTool::new(out_tx).with_base_dir(send_file_base.clone());
        for extra in &send_file_extras {
            send_file_tool = send_file_tool.with_extra_allowed_dir(extra.clone());
        }
        send_file_tool.set_context("api", &send_file_context_session);
        tool_registry.register(send_file_tool);

        // Drain `OutboundMessage`s emitted by `send_file` calls and persist
        // each one as an assistant message + media via the same canonical
        // path used by the spawn_only sender. Drops out when the turn ends
        // (the `out_tx` half is dropped along with the registry / agent
        // when the turn-scoped state is freed).
        //
        // A spawn-only companion is transcript-only: the following linked v2
        // background-child payload owns its media and visible completion.
        let consumer_sessions = bg_sessions.clone();
        let consumer_data_dir = bg_data_dir.clone();
        let consumer_session_id = bg_session_id.clone();
        let consumer_thread_id = bg_thread_id.clone();
        let consumer_context_manager = context_manager.clone();
        let consumer_context_dir = session_runtime.sessions_root.clone();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let thread_id = msg
                    .metadata
                    .get("thread_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| consumer_thread_id.clone());
                let is_spawn_complete_companion = msg
                    .metadata
                    .get("spawn_complete_companion")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let persist = persist_assistant_with_media(
                    &consumer_sessions,
                    &consumer_data_dir,
                    &consumer_session_id,
                    msg.content,
                    msg.media,
                    thread_id,
                    "send_file",
                );
                let persisted = if is_spawn_complete_companion {
                    MESSAGE_PROJECTION_OVERRIDE
                        .scope(Some(MessageProjectionOverride::Suppress), persist)
                        .await
                } else {
                    persist.await
                };
                if let Some((message, seq)) = persisted {
                    record_appui_context_manager_background_message(
                        &consumer_context_dir,
                        &consumer_context_manager,
                        &consumer_session_id,
                        &message,
                        seq,
                    );
                }
            }
        });
    }
    let progress_workspace_root = workspace_root
        .clone()
        .or_else(|| tool_registry.workspace_root().map(Path::to_path_buf));
    // RFC-0 (#1289): LRU tool deferral + the `activate_tools` recovery
    // meta-tool were removed. Voice turns now carry the full enabled tool set
    // like every other turn.
    // Re-apply the profile tool_policy AFTER this turn's per-session
    // channel/dispatcher tools were registered (send_file, peer_*, spawn,
    // bg_research, …). The snapshot at the top of the turn (31061) inherited
    // the profile-BOOTSTRAP policy, but these tools are added here at
    // turn-build time and would otherwise bypass an allow/deny list — so a
    // profile `tool_policy` only constrained the bootstrap roster, not the
    // per-turn roster the model actually sees. Symptom: octoscode ran a turn
    // with `tools=31` despite an 8-tool allow-list, drowning small local
    // models. Mirrors `session_actor.rs:3748` (the gateway path already does
    // this). No-op when no policy is set, so cloud/default behavior is
    // unchanged.
    if let Some(ref policy) = session_runtime.profile.tool_policy {
        tool_registry.apply_policy(policy);
    }
    // Wrap the per-turn `ToolRegistry` in an `Arc` here so we retain a
    // handle after `Agent::new_shared` consumes its own clone. The
    // post-terminal drain task (issue #961) inspects
    // `spawn_only_was_invoked()` to decide whether to continue forwarding
    // background progress events after the agent's main loop emitted
    // `done`/`error`.
    session_runtime
        .profile
        .apply_tool_envelope(&mut tool_registry);
    let tool_registry = Arc::new(tool_registry);

    // C1 fix: `progress_tx` / `progress_dropped` are now created earlier
    // (before the `enable_persistence` block) so the supervisor `on_change`
    // callback can be wired before the orphan sweep runs. See the note at
    // their construction site.
    // PR F (M8.10 thread-binding chain `#649 → #740`): bind the originating
    // `TurnId` into the reporter so every progress event the agent emits
    // carries `thread_id`. Closes the wire-side leak where standalone-turn
    // SSE events landed unbound and the SPA reducer had to fall back to
    // sticky-map heuristics.
    // M9-α-2 wiring: wrap the channel-bound reporter with
    // `LedgerToolProgressReporter` so `ProgressEvent::ToolProgress` events
    // (notably the pipeline heartbeat at `octos-pipeline/executor.rs:463`)
    // are mirrored onto the M9 ledger as `tool/progress.v1` notifications.
    // Without this wrap the heartbeat publishes into the channel-only path
    // and the SPA's tool-status bubble never refreshes for long-running
    // spawn_only tools (bg_research, podcast_generate, mofa_slides, ...).
    let inner_reporter: Arc<dyn octos_agent::ProgressReporter> = Arc::new(
        BoundedChannelReporter::new(progress_tx.clone(), progress_dropped.clone())
            .with_thread_id(Some(turn_id.0.to_string())),
    );
    let reporter: Arc<dyn octos_agent::ProgressReporter> = Arc::new(
        super::ui_protocol_alpha2_bridge::LedgerToolProgressReporter::new(
            inner_reporter,
            ledger.clone(),
            session_id.clone(),
            turn_id.clone(),
        ),
    );
    let progress_tx_for_result = progress_tx.clone();
    // C1 fix: the supervisor `on_change` callback is now wired earlier
    // (before `enable_persistence`, inside the background-result block) so
    // the orphan sweep's terminal `task_updated` events are not dropped. See
    // the note at that wiring site.
    drop(progress_tx);
    // M11-E: the agent is built per-turn (so per-turn callbacks layer in
    // without mutating shared session state), but its LLM, memory,
    // sandbox, and base system prompt come from the SessionRuntime
    // (preferred) or the legacy `state.agent`.
    //
    // M11-F regression fix REG-3: also propagate the profile-scope
    // hook executor (assembled once in `ProfileRuntime::bootstrap`
    // from `config.hooks + plugin_result.hooks`) onto the per-turn
    // rebuilt agent. Without this, every UI Protocol turn would bypass
    // the configured `before_tool_call` / `after_tool_call` /
    // `before_llm_call` / `after_llm_call` hooks because
    // `Agent::new_shared` resets `hooks: None`. We thread it directly
    // off the SessionRuntime's parent profile so the runtime layer
    // remains the single source of truth.
    // Volatile runtime state belongs at the semantic conversation tail. It
    // used to be concatenated into the first System message below, which
    // invalidated the entire provider KV prefix whenever a peer completed or a
    // monitor fired.
    let stable_system_prompt =
        append_workspace_root_hint(system_prompt_base.clone(), workspace_root.as_deref());

    let mut tail_context_events = Vec::new();
    // Empty memory is not a context event. In particular, a fresh stdio/solo
    // session must not pay for a synthetic `memory-snapshot` line on every
    // turn; the named memory segment already carries the stable policy when
    // memory is enabled and carries no content when the bank is empty.
    if should_emit_memory_snapshot(&volatile_memory_context) {
        tail_context_events.push((
            ContextEventKind::MemoryUpdate,
            "memory-snapshot",
            volatile_memory_context,
        ));
    }
    appui_append_tail_context_events(
        &session_runtime.sessions_root,
        &session_id,
        &llm_provider,
        &context_manager,
        &mut history,
        tail_context_events,
    );
    let prompt_cache_epoch_id = {
        let ordered_tools = tool_registry.specs();
        let mut manager = context_manager
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Lane identity must match what the serving lane reports after the
        // call (`provider_metadata_for_index`), otherwise a `label@host`
        // router tag on the configured lane reads as `model_route_changed`
        // on every request and the epoch flaps twice per call.
        let (lane_provider, lane_model) = prompt_cache_lane_identity(llm_provider.as_ref());
        let epoch = manager
            .reconcile_prompt_cache_epoch(
                &lane_provider,
                &lane_model,
                &stable_system_prompt,
                &ordered_tools,
            )
            .clone();
        publish_appui_context_status(&session_id, &manager);
        if let Err(error) =
            persist_appui_context_snapshot(&session_runtime.sessions_root, &session_id, &manager)
        {
            warn!(
                session = %session_id.0,
                error = %error,
                "failed to persist appui prompt cache epoch"
            );
        }
        tracing::debug!(
            session = %session_id.0,
            epoch_id = %epoch.epoch_id,
            invalidation_reason = %epoch.last_invalidation_reason,
            "appui prompt cache epoch reconciled"
        );
        epoch.epoch_id
    };

    let request_agent = Agent::new_shared(
        AgentId::new(format!("ui-protocol-{}", uuid::Uuid::now_v7())),
        llm_provider.clone(),
        tool_registry.clone(),
        memory_store.clone(),
    )
    .with_config(agent_config.clone())
    .with_system_prompt(stable_system_prompt)
    .with_prompt_cache_epoch_id(prompt_cache_epoch_id)
    .with_session_usage_base(session_usage_base.clone())
    // #1696 soak fix: thread the session key into every ToolContext this
    // turn builds. Without it anything reading
    // `ToolContext::parent_session_key` sees no session on AppUI turns
    // while the gateway actor path carried it fine.
    .with_parent_session_key(session_id.to_string())
    .with_reporter(reporter);
    let mut request_agent = if let Some(profile) = session_runtime.agent.profile() {
        request_agent
            .with_profile(profile)
            .with_agent_definitions(session_runtime.agent.agent_definitions())
    } else {
        request_agent
    };
    // In-loop compaction delivery (UPCR-2026-026 follow-up): mirror the
    // pre-turn lifecycle delivery — durable direct send for clients that
    // negotiated `context.lifecycle.v1`, ledger-only otherwise — so the
    // MID-TURN compaction pass (the one that actually fires when context
    // fills during a long turn) is visible to the client, not silent.
    let context_lifecycle_notify: ContextLifecycleNotify = {
        let ws = ws.clone();
        let ledger = ledger.clone();
        // `features` is Copy — the move closure captures its own copy.
        Arc::new(move |notification: UiNotification| {
            if features.context_lifecycle_available() {
                let _ = send_notification_durable(&ws, &ledger, notification);
            } else {
                let _ = ledger.append_notification_from(notification, ws.connection_id);
            }
        })
    };
    let mut prompt_context_bridge_inner = AppUiPromptContextBridge::new(
        session_id.clone(),
        // Mid-turn (compaction-bridge) context persists share the transcript
        // root — per-cwd under `appui.sessions_in_cwd` — for the same
        // cross-project isolation as the pre/post-turn sites above (#1666).
        session_runtime.sessions_root.clone(),
        context_manager.clone(),
    )
    .with_context_lifecycle_notify(context_lifecycle_notify);
    // Only wire the provider when `--llm-compaction` is on; a present provider
    // is what flips the in-loop bridge to the LLM-summarization path.
    if session_compaction_llm_enabled(&session_id, &state) {
        prompt_context_bridge_inner =
            prompt_context_bridge_inner.with_llm_compaction_provider(llm_provider.clone());
    }
    let prompt_context_bridge: Arc<dyn PromptContextManager> =
        Arc::new(prompt_context_bridge_inner);
    request_agent = request_agent.with_prompt_context_manager(prompt_context_bridge);
    if let Some(hooks) = session_runtime.profile.hook_executor.clone() {
        request_agent = request_agent.with_hooks(hooks);
    }
    // #2246 — the per-turn rebuild starts from `Agent::new_shared`, so the
    // bootstrap agent's hook context does not carry over; re-apply it here
    // (same ids the session's spawn tool receives above).
    request_agent = request_agent.with_hook_context(octos_agent::HookContext {
        session_id: Some(session_id.to_string()),
        profile_id: Some(session_runtime.profile.profile_id.clone()),
    });
    // Phase 3-A plumbing follow-up (Phase 1 gap): propagate the
    // `SessionScope` the cached `SessionRuntime` constructed at
    // `runtime/session.rs::bootstrap` onto this per-turn rebuilt agent.
    // Without this, every WS turn served by `Agent::new_shared` here
    // would observe `session_scope: None` and the Phase-2-A/B/C/D
    // consumers (bg_research working dir, plugin work_dir + path
    // validation, file tools' base_dir + path classification, shell +
    // spawn child CWD) would silently fall through to legacy paths —
    // re-introducing the mini5 NEW-06 contamination class on the WS
    // chat transport.
    //
    // Phase 3-A codex round-2 P1: when the cached scope's workspace
    // does NOT match `session_runtime.workspace_root` (i.e. a
    // coding-agent UI session opened with an explicit `cwd` hint via
    // `SessionOpenParams.cwd` or `appui.default_session_cwd` —
    // `resolve_workspace_root` honoured the hint while
    // `SessionRuntime::bootstrap` still built the cached
    // `SessionScope` from the canonical
    // `<data>/users/<id>/workspace` layout), build a fresh
    // **solo** scope rooted at the effective `workspace_root` and
    // propagate THAT.
    //
    // Round-1 of this PR tried to fix the mismatch by SKIPPING
    // propagation in this case, but codex correctly pointed out that
    // leaves hint sessions on the legacy unread-scope path — which
    // means `plugins::tool::PluginTool` falls back to its legacy
    // absolute-path rewrite branch (no workspace-boundary validation),
    // re-opening the path-escape class for hinted AppUI turns that
    // invoke a plugin with `audio_path=/etc/passwd` and friends.
    // A workspace-rooted solo scope keeps the Phase-2 consumers happy
    // (they see a valid scope, file tools / shell / pipeline workers
    // bind to the user-selected repo, and the plugin tool's
    // scope-aware path validation kicks in) without depending on the
    // pending bootstrap-side reconciliation.
    if let Some(scope) = session_runtime.agent.session_scope() {
        if scope.workspace() == session_runtime.workspace_root.as_path() {
            request_agent = request_agent.with_session_scope(scope.clone());
        } else {
            // #1377 Phase-3-B: this branch is now a DEFENSIVE FALLBACK
            // that should be unreachable. `SessionRuntime::bootstrap`
            // now roots the scope at the session's REAL `workspace_root`
            // for every shape — channel-prefixed (`:`) ids via the
            // encoded-path workspace, and coding-agent `workspace_hint`
            // sessions via a repo-rooted scope (root == workspace) — so
            // `scope.workspace() == session_runtime.workspace_root`
            // holds by construction and the `if` branch above fires.
            //
            // The earlier rounds deliberately skipped propagation for
            // the workspace-mismatch (hint) case because the scoped
            // resolver misclassified `up/...` handles and absolute
            // upload-tmpdir paths as OutOfScope. That blocker is gone:
            // uploads are now materialized into `<workspace>/uploads/`
            // at turn start and read by their workspace-relative path,
            // so there are no raw `up/` handles left for the scoped
            // resolver to misclassify (a pasted foreign `up/` handle is
            // instead REFUSED by the tenant-ownership gate — the goal).
            //
            // If we ever DO land here (a scope whose workspace does not
            // match the turn's workspace_root), propagating it would
            // misresolve relative paths against the wrong directory, so
            // the safe action is still to skip and fall back to the
            // legacy resolver. Log at warn so the unexpected mismatch is
            // visible rather than silent.
            tracing::warn!(
                session = %session_id.0,
                turn = %turn_id.0,
                scope_workspace = %scope.workspace().display(),
                runtime_workspace = %session_runtime.workspace_root.display(),
                "SessionScope workspace does not match SessionRuntime.workspace_root; \
                 NOT propagating (bootstrap should root every scope at workspace_root — \
                 this branch is expected to be unreachable post-#1377 Phase-3-B)"
            );
        }
    }
    // M11-F regression fix REG-1 follow-up (codex review): wire the
    // `activate_tools` back-reference on the per-turn rebuilt agent.
    // `ProfileRuntime::bootstrap` defers non-core groups + registers
    // the tool; without this wiring call, the LLM sees the tool in
    // `specs()` but `activate_tools` is unable to reach the registry
    // (its internal `Weak<ToolRegistry>` is empty). Gateway does the
    // equivalent at `session_actor.rs:2500`.

    let agent_session_id = session_id.clone();
    let approval_requester: Arc<dyn octos_agent::ToolApprovalRequester> =
        Arc::new(UiProtocolApprovalRequester {
            ws: ws.clone(),
            ledger: ledger.clone(),
            contracts: contracts.clone(),
            state: state.clone(),
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            features,
        });
    // UPCR-2026-023: install the structured-user-question bridge ONLY when the
    // connection negotiated `user_question.v1`. When unset the task-local stays
    // empty, so the agent's `ask_user_question` tool degrades to its
    // structured-metadata fallback and the turn never hard-blocks.
    let user_question_requester: Option<Arc<dyn octos_agent::UserQuestionRequester>> =
        features.user_question_v1.then(|| {
            Arc::new(SessionUserQuestionRequester {
                ws: ws.clone(),
                ledger: ledger.clone(),
                contracts: contracts.clone(),
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            }) as Arc<dyn octos_agent::UserQuestionRequester>
        });
    // PR F (M8.10): capture the originating `TurnId` as a string so the
    // tokio::spawn closure (which moves everything it touches) can pre-stamp
    // each persisted Assistant/Tool message with the correct thread_id.
    // Required because Patch 8 fails the persist closed if Assistant/Tool
    // arrives unbound — the previous derive-from-history fallback picked
    // the WRONG sibling user under rapid-fire concurrent turns.
    let turn_thread_id_for_persist = turn_id.0.to_string();
    let turn_thread_id_for_done = turn_thread_id_for_persist.clone();
    let context_manager_for_result = context_manager.clone();
    // Post-turn context persists go to the session's transcript root (per-cwd
    // when `appui.sessions_in_cwd` is on) so the snapshot the NEXT turn's
    // `appui_context_history_for_agent` loads is the same project's (#1666).
    let context_data_dir_for_result = session_runtime.sessions_root.clone();
    // `turn/steer` wiring: hand the per-turn pending-input buffer to the
    // agent loop. A drained steer is NOT persisted at drain time: it lands in
    // the agent's chronological `turn_output_log`, so the end-of-turn persist
    // loop below writes it at its model-visible position (after every row
    // the model had already seen) and stamps its in-flight twin in the
    // context ledger. Persisting at drain time gave the steer a LOWER durable
    // sequence than the turn's own prompt/answer rows, so a context ledger
    // rebuilt from session history (missing, stale or corrupt snapshot)
    // showed the model `steer → prompt → answer` instead of the chronology
    // it actually saw. The live prompt scratch already carries the drained
    // steer as an in-flight row, so the next model call and the next
    // snapshot keep it without a separate merge.
    if let Some(buffer) = steer_buffer.clone() {
        let steer_data_dir = sessions.lock().await.data_dir();
        let steer_session_id = session_id.clone();
        let drained_callback: octos_agent::SteerDrainedCallback = Arc::new(move |texts| {
            let data_dir = steer_data_dir.clone();
            let session_id = steer_session_id.clone();
            Box::pin(async move {
                // Canonical turn logging owns persistence; retain only the
                // upstream consumption receipt here, never a second write.
                if !texts.is_empty() {
                    crate::obs_events::append_obs_event(
                        &data_dir,
                        &crate::obs_events::ObsEvent::new(
                            "steer_consumed",
                            &format!("{} steer(s) drained into turn", texts.len()),
                        )
                        .session(Some(session_id.0.as_str())),
                    );
                }
            })
        });
        request_agent = request_agent
            .with_steer_buffer(buffer)
            .with_steer_drained_callback(drained_callback);
    }
    let usage_ledger_for_result = usage_ledger.clone();
    let usage_profile_id_for_result = usage_profile_id.clone();
    let usage_session_id_for_result = session_id.to_string();
    let usage_run_id_for_result = turn_id.0.to_string();
    // UPCR-2026-015 (M9-β-1): pull the pre-uploaded media paths off
    // the params and feed them to the agent loop. `process_message`
    // already accepts a `Vec<String>` of paths (used by the
    // `octos chat` CLI and gateway-mode message handler) — wiring it
    // here restores the legacy SSE chat handler's media delivery on
    // the WS transport. The legacy `rewrite_for` field is logged at
    // debug level for now; durable in-place rewrites land in a
    // follow-up that touches the per-session ledger replace path.
    // #1377: materialize non-image uploads into `<workspace>/uploads/` so the
    // agent reads them as ordinary workspace files (read_file/grep/list_dir/glob
    // all work) and global `up/` resolution can be refused for tenant isolation.
    // Images pass through unchanged (vision reads them directly). The persisted
    // user-row media (set from this list inside the agent loop) therefore stores
    // `uploads/<name>` so the `up/` handle never resurfaces in later turns; the
    // client-facing `UserMessage` envelope keeps the original handle for display.
    let raw_media: Vec<String> = params
        .media
        .iter()
        .map(|file_ref| file_ref.path.clone())
        .collect();
    let turn_media_paths: Vec<String> = octos_bus::file_handle::materialize_turn_uploads(
        &session_runtime.workspace_root,
        // #1377: bind to the session's OWNING TENANT so the materializer only
        // copies uploads owned by this tenant (cross-tenant handles dropped).
        // Use the runtime's profile id — NOT `scope.tenant_id()` — because
        // profile-qualified AppUI sessions (`:`-keyed) run under a profile but
        // may have no attached `SessionScope`; deriving the tenant only from the
        // scope would skip the ownership check for them (codex round-5 P1).
        Some(session_runtime.profile.profile_id.as_str()),
        &raw_media,
    );
    if let Some(rewrite_for) = params.rewrite_for.as_deref() {
        tracing::debug!(
            session = %session_id.0,
            turn = %turn_id.0,
            rewrite_for,
            "turn/start carries rewrite_for; current build forwards the prompt without in-place ledger rewrite (β-1 advisory)"
        );
    }
    // Stamp the originating session/turn id into a tokio task_local so
    // prompt-cache observation events attribute usage to the right
    // session/turn (see `octos_llm::cache_manifest`). Without this,
    // usage lands under the redacted "unattributed" stream key.
    let router_ctx = octos_llm::RouterContext {
        session_id: Some(session_id.0.clone()),
        turn_id: Some(turn_id.0.to_string()),
    };
    // #1128 codex P1 re-review #2 — clone the session manager Arc
    // before the agent_task spawn moves the original. We need the
    // clone alive in this outer scope for the post-turn self-paced
    // reschedule read below.
    let sessions_for_reschedule = sessions.clone();
    // #1134 — capture the turn's final `response.content` (the agent's
    // actual EndTurn payload) directly out of `agent_task` so the
    // post-turn self-paced reschedule block can parse the
    // `<<loop-next-in: ...>>` hint from the LLM reply without scanning
    // the session history. The session-history walk picks the LAST
    // assistant-row, which is wrong when a spawn_only / send_file path
    // persists a background assistant message AFTER the model's final
    // reply: `.last()` selects the background row (often with no
    // hint) and `apply_self_paced_response` falls back to the default
    // delay. A oneshot is the right shape because the spawn emits at
    // most one final response and we only need it once in the post-turn
    // block.
    let (final_reply_tx, final_reply_rx) = tokio::sync::oneshot::channel::<Option<String>>();
    // #1969 — shared token tracker, moved into the spawned agent task below.
    let token_tracker_task = std::sync::Arc::new(octos_agent::TokenTracker::new());
    // task-turn-interrupt-steer-correlation-logs: every agent-side log line
    // (LLM calls, tool batches, steer drains, EndTurn rounds) inherits
    // `session`/`turn` from this span (postfix `.instrument` keeps the block
    // itself untouched).
    let turn_span = crate::turn_trace::turn_span(&session_id, &turn_id);
    let agent_task = tokio::spawn(async move {
        // Wrap the agent.process_message future in the router context
        // (session/turn attribution for prompt-cache observation) so
        // every chat() the turn recurses through stays attributed.
        // UPCR-2026-023: nest the user-question task-local INSIDE the approval
        // scope, mirroring how `TOOL_APPROVAL_CTX` wraps the turn. The two are
        // orthogonal blocking bridges. The scope is installed only when the
        // connection negotiated `user_question.v1` (`user_question_requester`
        // is `Some`); otherwise the agent's `ask_user_question` tool sees no
        // requester and degrades to its structured-metadata fallback.
        // #1478: thread the explicit live-video signal into the turn so the
        // agent loop's video-call note (and the rich-output camera-frame
        // grounding) fire only when the client says this is a live camera turn
        // — never inferred from attachment types. Other attachment-context
        // fields stay at their serve-path defaults.
        let turn_attachments = octos_agent::TurnAttachmentContext {
            live_video: params.live_video,
            ..Default::default()
        };
        // #1969 — feed the shared token tracker (its clone `token_tracker_task`
        // was moved into this spawned agent task; the original stays in the
        // outer scope) so an INTERRUPTED turn's partial spend is readable after
        // the drain loop. Mirrors the pattern `session_actor` already uses.
        let message_future = request_agent.process_message_tracked_with_attachments(
            &prompt,
            &history,
            turn_media_paths,
            turn_attachments,
            token_tracker_task,
        );
        let scoped_message_future: std::pin::Pin<
            Box<
                dyn std::future::Future<Output = eyre::Result<octos_agent::ConversationResponse>>
                    + Send,
            >,
        > = match user_question_requester {
            Some(requester) => {
                Box::pin(octos_agent::tools::USER_QUESTION_CTX.scope(requester, message_future))
            }
            None => Box::pin(message_future),
        };
        let result = octos_llm::with_router_context(
            router_ctx,
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approval_requester, scoped_message_future),
        )
        .await;

        // Reuse the canonical persistence path for actual truncated output,
        // without turning an incomplete model response into a successful turn.
        let incomplete_message = result.as_ref().err().and_then(|error| {
            error.downcast_ref::<octos_agent::IncompleteResponseError>()
                .map(ToString::to_string)
        });
        let result = match result {
            Err(error) if incomplete_message.is_some() => Ok(error
                .downcast_ref::<octos_agent::IncompleteResponseError>()
                .expect("incomplete carrier checked above").partial.clone()),
            result => result,
        };
        match result {
            Ok(response) => {
                // #1134 — capture the LLM reply for the post-turn
                // self-paced reschedule block. The receiver of this
                // oneshot uses the captured content instead of
                // `history.last()`, so background assistant rows
                // (spawn_only / send_file companion writes) that
                // land below the LLM reply cannot mask the model's
                // `<<loop-next-in: ...>>` hint. We CLONE the string
                // so the in-spawn `done` event below can still emit
                // `response.content`.
                //
                // #1158 codex P2 follow-up: send the captured reply
                // ONLY AFTER persistence has committed. If the spawn
                // task is interrupted (or panics) between
                // `process_message` returning and persistence
                // completing, an early send would let the post-turn
                // block call `apply_self_paced_response` from a
                // reply that never made it into session history.
                // Defer the send until below the persist block.
                let mut captured_final_reply = response.content.clone();
                let mut cursor = None;
                // Issue #1332: capture the wire `message_id` for the
                // final assistant row so the `done` event can surface
                // it on `turn/completed.session_result.message_id`.
                // Format mirrors `MessageCommitObserver`:
                // `{session_id}:{committed_seq}:{timestamp_ns}`. Only
                // populated on the success branch of
                // `add_message_with_seq`, so the consumer sees a
                // `Some` only when the durable row actually exists.
                let mut final_assistant_message_id: Option<String> = None;
                // Issue #1337 codex round-2: capture the committed seq
                // for the assistant carrier row at the SAME moment we
                // stamp `final_assistant_message_id`. The loop-wide
                // `cursor` is updated for every persisted row and can
                // therefore advance past the assistant carrier when an
                // assistant row with `tool_calls` is followed by tool
                // rows in the trimmed-dedupe path documented above.
                // Using `cursor.seq` to build `TurnSessionResult` would
                // then surface a tool-row seq alongside the assistant
                // `message_id`, breaking the durable per-row identity
                // contract. This field pins the seq to the carrier
                // exactly like `final_assistant_message_id`.
                let mut final_assistant_committed_seq: Option<u64> = None;
                // #1158 codex P2 rev2 follow-up: `add_message_with_seq`
                // can fail (e.g. JSONL at MAX_SESSION_FILE_SIZE, I/O
                // error). Track whether the assistant row carrying
                // `response.content` actually persisted. If not, the
                // captured reply must NOT be released to the post-turn
                // reschedule block — that would let it call
                // `apply_self_paced_response` from a reply that never
                // hit session history, which is the exact failure mode
                // this fix is meant to prevent.
                let mut final_assistant_persisted = false;
                {
                    let mut sessions = sessions.lock().await;
                    let final_assistant = final_assistant_message_for_response(&response);
                    // Fleet-UX soak NEW-03 (mini3/mini5, 2026-05-23):
                    // the #1183 fix hid the synthesised spawn_only ack
                    // from the old wire surface, but the JSONL row was still committed
                    // with `role: assistant`. The page handler at
                    // `handle_session_messages_page` walks JSONL
                    // directly with NO source filter, so any SPA
                    // refresh / replay re-rendered the suppressed
                    // bubble. Soak captured 33 occurrences of the
                    // ack text in messages_page payloads under one
                    // turn — the "ghost ack" bug.
                    //
                    // Post-fix: when `synthesized_from_spawn_only`
                    // is set, skip the ack persist entirely. The
                    // background task's real outcome still flows via
                    // `BackgroundResultSender` (its own persist +
                    // canonical v2 child envelope), so the
                    // legitimate "task finished" signal is unaffected.
                    // Only the foreground synthesised "started" bubble
                    // — which the foreground cannot actually verify
                    // is dropped.
                    let skip_synthesized_spawn_only_ack =
                        should_skip_synthesized_spawn_only_ack_persist(
                            response.synthesized_from_spawn_only,
                            final_assistant.is_some(),
                        );
                    // Codex round-2/3 P2 on this PR: track the LAST
                    // non-empty preamble assistant content that lands.
                    // When we skip the synthesised ack and a preamble
                    // row carried content (e.g. self-paced loop says
                    // "Starting... <<loop-next-in: 60s>>" plus the
                    // spawn_only tool call), we substitute this
                    // captured text for `response.content` on the
                    // oneshot. That makes `apply_self_paced_response`
                    // parse the PREAMBLE's hint directly — bypassing
                    // the history-fallback walk which is unsafe in the
                    // presence of fast background completions (the
                    // walk uses `.last()` on assistant rows, so a
                    // BackgroundResultSender row that lands before the
                    // post-turn reschedule would otherwise win and
                    // drop the preamble's hint).
                    let mut last_persisted_preamble_assistant: Option<String> = None;
                    // NEW-16 defense-in-depth: per-turn message-index
                    // cursor. The main fix is the append-only
                    // `turn_output_log` upstream — but if some edge
                    // path re-entered this persist loop for the SAME
                    // `(session, turn)` pair, we MUST NOT double-write
                    // the same indexed row.
                    //
                    // Codex round-3 P1+P2: switched from immediate
                    // end-of-turn GC to TTL-based opportunistic
                    // pruning. The entry stays alive long enough for
                    // any realistic queued re-entry to observe it,
                    // and stale entries (from abort/panic paths
                    // where the outer task was cancelled) get
                    // pruned by subsequent persist activity rather
                    // than relying on cancellation-safe cleanup.
                    let persist_cursors = turn_persist_cursors();
                    let cursor_key = (
                        agent_session_id.0.clone(),
                        turn_thread_id_for_persist.clone(),
                    );
                    let cursor_already_advanced = {
                        let mut cursors = persist_cursors.lock().await;
                        prune_stale_turn_persist_cursors(&mut cursors);
                        cursors.get(&cursor_key).map(|e| e.next_index).unwrap_or(0)
                    };
                    for (message_index, message) in response.messages.iter().cloned().enumerate() {
                        // NEW-16 defense-in-depth: skip indexes already
                        // persisted for this `(session, turn)` pair.
                        // `cursor_already_advanced` reflects the
                        // highest-index-already-written + 1 (i.e. the
                        // NEXT index that needs writing). If the
                        // current index is below that watermark, an
                        // earlier invocation already wrote it.
                        if message_index < cursor_already_advanced {
                            tracing::debug!(
                                session = %agent_session_id.0,
                                turn = %turn_thread_id_for_persist,
                                message_index,
                                cursor_already_advanced,
                                "NEW-16 cursor guard skipped already-persisted message in this turn"
                            );
                            continue;
                        }
                        // Detect the exact row that carries
                        // `response.content` BEFORE pre-stamp /
                        // persist, so we can flip
                        // `final_assistant_persisted` only on its
                        // Ok branch.
                        //
                        // NEW-10 codex round-1 P2: extended from
                        // byte-exact equality to trimmed-equality
                        // via `is_final_assistant_carrier_under_trimmed_equality`
                        // so the carrier-flip stays in lockstep with
                        // the synth-skip helper
                        // (`final_assistant_content_already_persisted`).
                        // Without that, a trimmed-equal iter-N row
                        // that the synth-skip helper recognised as
                        // the duplicate carrier would land with
                        // `final_assistant_persisted=false`,
                        // `final_send` would resolve to `None`, and
                        // self-paced loops with `<<loop-next-in: ...>>`
                        // hints in the EndTurn text would fall back
                        // to the unsafe `.last()` history scan.
                        let is_final_assistant_carrier =
                            response.assistant_segments.message_iterations.iter().any(|(index, iteration)|
                                *index == message_index && *iteration == response.assistant_segments.final_iteration)
                            && is_final_assistant_carrier_under_trimmed_equality(
                                &message,
                                &response.content,
                            );
                        let preamble_assistant_content = if message.role == MessageRole::Assistant
                            && !message.content.trim().is_empty()
                        {
                            Some(message.content.clone())
                        } else {
                            None
                        };
                        let to_save =
                            pre_stamp_turn_thread_id(message, &turn_thread_id_for_persist);
                        let saved_for_context = to_save.clone();
                        let projection = assistant_message_projection(&response, message_index, &turn_thread_id_for_persist);
                        if let Ok(seq) = MESSAGE_PROJECTION_OVERRIDE.scope(projection, sessions
                            .add_message_with_seq(&agent_session_id, to_save))
                            .await
                        {
                            // NEW-16: advance the per-turn cursor
                            // ONLY after the JSONL write succeeded
                            // (and inside the session lock — note
                            // that `sessions` is the lock guard, so
                            // anyone re-entering the persist loop on
                            // this `(session, turn)` will see the
                            // updated cursor on their next read).
                            //
                            // Codex round-3: timestamp the entry so
                            // the TTL pruner can evict it.
                            {
                                let mut cursors = persist_cursors.lock().await;
                                let now = std::time::Instant::now();
                                let entry = cursors.entry(cursor_key.clone()).or_insert(
                                    TurnPersistCursorEntry {
                                        next_index: 0,
                                        last_touched_at: now,
                                    },
                                );
                                if message_index + 1 > entry.next_index {
                                    entry.next_index = message_index + 1;
                                }
                                entry.last_touched_at = now;
                            }
                            record_appui_context_manager_message(
                                &context_data_dir_for_result,
                                &context_manager_for_result,
                                &agent_session_id,
                                &saved_for_context,
                                seq,
                            );
                            cursor = Some(UiCursor {
                                stream: agent_session_id.0.clone(),
                                seq: seq as u64,
                            });
                            if is_final_assistant_carrier {
                                final_assistant_persisted = true;
                                // Issue #1332: stamp the wire message_id
                                // for the carrier row so `done` can
                                // populate `session_result.message_id`.
                                let ts_ns = saved_for_context
                                    .timestamp
                                    .timestamp_nanos_opt()
                                    .unwrap_or(0);
                                final_assistant_message_id =
                                    Some(format!("{}:{seq}:{ts_ns}", agent_session_id.0,));
                                // Issue #1337 codex round-2: pin the
                                // committed seq to the assistant carrier
                                // row so `session_result.committed_seq`
                                // stays aligned with `message_id`. The
                                // loop's outer `cursor` may advance to a
                                // following tool row when the assistant
                                // emitted `tool_calls`.
                                final_assistant_committed_seq = Some(seq as u64);
                            }
                            if let Some(content) = preamble_assistant_content {
                                last_persisted_preamble_assistant = Some(content);
                            }
                        }
                    }
                    let any_preamble_assistant_persisted =
                        last_persisted_preamble_assistant.is_some();
                    if let Some(message) = final_assistant {
                        // Fleet-UX soak NEW-03: when the loop fabricated
                        // the synthesised "Background work started for
                        // `<tool>`." ack, skip the JSONL persist
                        // entirely (see the
                        // `skip_synthesized_spawn_only_ack` comment
                        // above for the full rationale).
                        //
                        // Codex P2 (rounds 1-2) on this PR: a
                        // self-paced / `maintenance` loop firing a
                        // BARE spawn_only call with NO LLM preamble
                        // has only the synthesised ack as its
                        // turn-final assistant content. If we skip
                        // the ack persist AND leave
                        // `final_assistant_persisted=false`,
                        // `final_send` becomes `None`, the
                        // history-fallback scan also finds no
                        // non-empty row (because we skipped the
                        // persist), and `apply_self_paced_response`
                        // never fires. Since fire-time clears
                        // `next_run_at_ms` until
                        // `apply_self_paced_response` stamps a fresh
                        // delay, the loop would stall.
                        //
                        // BUT codex round-2 caught the dual:
                        // unconditionally flipping
                        // `final_assistant_persisted` regresses
                        // turns that DID emit a non-empty preamble
                        // carrying a model-provided
                        // `<<loop-next-in: 60s>>` hint. The post-turn
                        // rescheduler would prefer the captured
                        // (synthesised, hint-less) ack on the
                        // oneshot over the persisted preamble's
                        // hint, falling back to the 15-min default
                        // and dropping the model's requested delay.
                        //
                        // So flip ONLY when no non-empty assistant
                        // row landed during the preamble loop. With
                        // a preamble row present, leave
                        // `final_assistant_persisted` at false here
                        // — the rescheduler walks history, picks the
                        // preamble row, and honours its hint.
                        if skip_synthesized_spawn_only_ack {
                            // Codex rounds 1-3 P2 on this PR: route a
                            // self-paced / `maintenance` loop's
                            // reschedule signal deterministically WITHOUT
                            // touching the unsafe history-fallback walk
                            // (whose `.last()` semantics can race with a
                            // fast `BackgroundResultSender` row landing
                            // before the post-turn block runs).
                            //
                            // Strategy: always flip
                            // `final_assistant_persisted` to true on
                            // the skip path, and substitute the
                            // captured_final_reply with the persisted
                            // preamble's content when present
                            // (round-3 fix) — or fall back to the
                            // synth-ack text for a bare-spawn-only
                            // turn (round-1 fix). See
                            // `captured_final_reply_for_synth_ack_skip`
                            // for the truth table.
                            debug!(
                                session = %agent_session_id,
                                preamble_present = %any_preamble_assistant_persisted,
                                "skipping JSONL persist for synthesised spawn_only ack (NEW-03); \
                                 routing reschedule signal deterministically without the \
                                 history-fallback walk"
                            );
                            captured_final_reply = captured_final_reply_for_synth_ack_skip(
                                captured_final_reply,
                                last_persisted_preamble_assistant.as_deref(),
                            );
                            final_assistant_persisted = true;
                        } else {
                            // The `final_assistant` row is the synthesised
                            // carrier of `response.content`. By
                            // construction this is an Assistant row, and
                            // its content equals
                            // `response.content` (so it is the
                            // final-assistant carrier).
                            //
                            // NEW-16 codex review P1: extend the per-turn
                            // cursor guard to cover the synthesised final
                            // assistant row too. Treat it as VIRTUAL
                            // index `response.messages.len()` — the slot
                            // immediately past the last log row. If a
                            // re-entry into this persist block sees the
                            // cursor advanced to `messages.len() + 1`,
                            // the final row has already been written and
                            // the second invocation must skip.
                            let final_virtual_index = response.messages.len();
                            let cursor_already_at_final = {
                                let c = persist_cursors.lock().await;
                                c.get(&cursor_key).map(|e| e.next_index).unwrap_or(0)
                                    > final_virtual_index
                            };
                            if cursor_already_at_final {
                                tracing::debug!(
                                    session = %agent_session_id.0,
                                    turn = %turn_thread_id_for_persist,
                                    final_virtual_index,
                                    "NEW-16 cursor guard skipped already-persisted synthetic \
                                     final assistant row in this turn"
                                );
                                // The final row was committed by an earlier
                                // invocation. Flip `final_assistant_persisted`
                                // so downstream signalling (oneshot send)
                                // sees the same state as the original
                                // successful path.
                                final_assistant_persisted = true;
                            } else {
                                let to_save =
                                    pre_stamp_turn_thread_id(message, &turn_thread_id_for_persist);
                                let saved_for_context = to_save.clone();
                                let session_id_for_persist = agent_session_id.clone();
                                let projection = MessageProjectionOverride::AssistantSegment(
                                    final_assistant_segment_id(&response, &turn_thread_id_for_persist));
                                let commit = MESSAGE_PROJECTION_OVERRIDE.scope(Some(projection), sessions
                                    .add_message_with_seq(&session_id_for_persist, to_save))
                                    .await;
                                if let Ok(seq) = commit {
                                    // Advance the cursor past the virtual
                                    // final-row index so a re-entry skips it.
                                    {
                                        let mut c = persist_cursors.lock().await;
                                        let now = std::time::Instant::now();
                                        let entry = c.entry(cursor_key.clone()).or_insert(
                                            TurnPersistCursorEntry {
                                                next_index: 0,
                                                last_touched_at: now,
                                            },
                                        );
                                        if final_virtual_index + 1 > entry.next_index {
                                            entry.next_index = final_virtual_index + 1;
                                        }
                                        entry.last_touched_at = now;
                                    }
                                    record_appui_context_manager_message(
                                        &context_data_dir_for_result,
                                        &context_manager_for_result,
                                        &agent_session_id,
                                        &saved_for_context,
                                        seq,
                                    );
                                    cursor = Some(UiCursor {
                                        stream: agent_session_id.0.clone(),
                                        seq: seq as u64,
                                    });
                                    final_assistant_persisted = true;
                                    // Issue #1332: stamp message_id for
                                    // the synthesised final-assistant
                                    // row too (parity with the carrier
                                    // branch above so `done`'s
                                    // `session_result` is populated
                                    // whichever branch wrote the row).
                                    let ts_ns = saved_for_context
                                        .timestamp
                                        .timestamp_nanos_opt()
                                        .unwrap_or(0);
                                    final_assistant_message_id =
                                        Some(format!("{}:{seq}:{ts_ns}", agent_session_id.0,));
                                    // Issue #1337 codex round-2: parity
                                    // with the carrier branch — pin the
                                    // committed seq to this synthesised
                                    // assistant row so the consumer can
                                    // build `TurnSessionResult` from a
                                    // seq that actually points at the
                                    // assistant row (not whatever last
                                    // touched `cursor`).
                                    final_assistant_committed_seq = Some(seq as u64);
                                }
                            }
                        }
                    }
                    // NEW-16 codex round-3 P1+P2: do NOT GC the
                    // cursor here AND do NOT GC at the bottom of
                    // `run_standalone_turn`. The TTL pruner
                    // (`prune_stale_turn_persist_cursors`) handles
                    // eviction opportunistically on subsequent
                    // persist activity. That covers:
                    //   - queued same-`(session, turn)` re-entry
                    //     (entry stays alive until TTL expires)
                    //   - abort/panic paths (outer GC would be
                    //     skipped on cancellation; TTL evicts
                    //     anyway)
                    //   - long-running server map bound (TTL
                    //     caps memory at "entries created within
                    //     the last 5 minutes")
                }
                // #1158 codex P2 rev3 follow-up: distinguish two
                // shapes of "empty captured reply":
                //
                //   * `response.content.is_empty()` — the model
                //     explicitly EndTurn'd with no text. The picker
                //     contract treats `Some("")` as an authoritative
                //     blank signal and suppresses the history
                //     fallback. We must NOT downgrade to `None`
                //     here, otherwise an earlier non-empty assistant
                //     row that landed in history would be picked up
                //     as the loop's "last reply" and reschedule from
                //     stale content.
                //
                //   * `response.content` non-empty BUT the
                //     assistant-row persist failed — the captured
                //     reply was never committed to history, so we
                //     send `None` and let the post-turn block fall
                //     back to the history scan (matches pre-#1134
                //     aborted-turn behaviour).
                let final_send = if incomplete_message.is_some() {
                    // An incomplete fragment must not trigger self-paced work
                    // or fall back to a previous turn's answer.
                    Some(String::new())
                } else if response.content.is_empty() || final_assistant_persisted {
                    Some(captured_final_reply)
                } else {
                    None
                };
                let _ = final_reply_tx.send(final_send);
                if let Some(usage_ledger) = usage_ledger_for_result.as_ref() {
                    let provider_metadata = response.provider_metadata.clone();
                    let provider = provider_metadata
                        .as_ref()
                        .map(|meta| meta.provider.clone())
                        .or_else(|| {
                            let provider = request_agent.provider_name();
                            (!provider.is_empty()).then(|| provider.to_string())
                        });
                    let model = provider_metadata
                        .as_ref()
                        .map(|meta| meta.model.clone())
                        .or_else(|| {
                            let model = request_agent.model_id();
                            (!model.is_empty()).then(|| model.to_string())
                        });
                    // Attributed by the agent loop: each response priced at
                    // the model that produced it. Re-pricing the turn total
                    // at the final model mispriced cross-model turns (codex
                    // #1632 P1); the reprice fallback covers legacy paths.
                    let estimated_cost_usd = response.estimated_spend_usd.or_else(|| {
                        model.as_deref().and_then(model_pricing).map(|pricing| {
                            pricing.cost_with_cache_for_provider(
                                provider.as_deref().unwrap_or(""),
                                model.as_deref().unwrap_or(""),
                                response.token_usage.input_tokens,
                                response.token_usage.output_tokens,
                                response.token_usage.cache_read_tokens,
                                response.token_usage.cache_write_tokens,
                            )
                        })
                    });
                    let cost_source = if estimated_cost_usd.is_some() {
                        UsageCostSource::CatalogEstimate
                    } else {
                        UsageCostSource::Unavailable
                    };
                    let event = UsageEvent::completed_run(
                        usage_profile_id_for_result.clone(),
                        usage_session_id_for_result.clone(),
                        usage_run_id_for_result.clone(),
                        provider,
                        model,
                        provider_metadata
                            .as_ref()
                            .and_then(|meta| meta.endpoint.clone()),
                        u64::from(response.token_usage.input_tokens),
                        u64::from(response.token_usage.output_tokens),
                        estimated_cost_usd,
                        cost_source,
                        "appui",
                        None,
                    )
                    .with_cache_read_tokens(u64::from(response.token_usage.cache_read_tokens))
                    .with_cache_write_tokens(u64::from(response.token_usage.cache_write_tokens));
                    if let Err(error) = usage_ledger.record(event).await {
                        warn!(
                            session = %usage_session_id_for_result,
                            run = %usage_run_id_for_result,
                            error = %error,
                            "failed to record AppUI usage event"
                        );
                    }
                }
                // Issue #1332: include `message_id` so the consumer
                // can build `TurnSessionResult` for the
                // `turn/completed` lifecycle envelope. Absent when no
                // final-assistant row persisted (no synthesis +
                // skipped carrier + JSONL write failures).
                //
                // Issue #1337 codex round-2: also include
                // `final_assistant_committed_seq` so the consumer can
                // build `TurnSessionResult.committed_seq` from the
                // assistant carrier's seq instead of the loop's last
                // `cursor.seq` (which may point at a tool row when the
                // assistant emitted `tool_calls`).
                if let Some(message) = incomplete_message {
                    let partial_result = TurnErrorPartialResult {
                        session_result: final_assistant_message_id
                            .zip(final_assistant_committed_seq)
                            .map(|(message_id, committed_seq)| TurnSessionResult {
                                message_id, committed_seq, client_message_id: None,
                            }),
                    };
                    let error = json!({
                        "type": "error", "code": "output_truncated", "message": message,
                        "partial_result": partial_result,
                        "tokens_in": response.token_usage.input_tokens,
                        "tokens_out": response.token_usage.output_tokens,
                        "tokens_cache": u64::from(response.token_usage.cache_read_tokens)
                            + u64::from(response.token_usage.cache_write_tokens),
                        "token_usage": EnvelopeTokenUsage {
                            input_tokens: u64::from(response.token_usage.input_tokens),
                            output_tokens: u64::from(response.token_usage.output_tokens),
                            reasoning_tokens: u64::from(response.token_usage.reasoning_tokens),
                            cache_read_tokens: u64::from(response.token_usage.cache_read_tokens),
                            cache_write_tokens: u64::from(response.token_usage.cache_write_tokens),
                        },
                    });
                    let _ = progress_tx_for_result.send(error.to_string()).await;
                    return;
                }
                let done = json!({
                    "type": "done",
                    "content": response.content,
                    "tokens_in": response.token_usage.input_tokens,
                    "tokens_out": response.token_usage.output_tokens,
                    // #1650 — cache reads/writes are DISJOINT from
                    // `input_tokens` (anthropic: prompt = input + cache_read
                    // + cache_creation). Carried for consumers that want the
                    // TRUE token cost instead of just the uncached suffix +
                    // output. `tokens_in`/`tokens_out` keep their prior
                    // meaning for every other consumer of this event.
                    "tokens_cache": (response.token_usage.cache_read_tokens as u64)
                        + (response.token_usage.cache_write_tokens as u64),
                    "cursor": cursor,
                    "message_id": final_assistant_message_id,
                    "final_assistant_committed_seq": final_assistant_committed_seq,
                    "thread_id": turn_thread_id_for_done,
                });
                let _ = progress_tx_for_result.send(done.to_string()).await;
            }
            Err(error) => {
                // #1134 — signal the post-turn block that no LLM reply
                // is available on the error path. The receiver falls
                // back to the session-history scan, matching the
                // pre-#1134 behaviour.
                let _ = final_reply_tx.send(None);
                // Codex round-2 MAJOR 2: prefer the typed user-actionable
                // message over the raw LLM Display string. The agent
                // loop's classifier (`HarnessError::classify_report` at
                // loop_runner.rs:419) maps a raw `LlmError` to a
                // user-friendly `HarnessError` variant whose
                // `message()` says "top up or switch provider" /
                // "check your API key" — but the loop currently bails
                // with the ORIGINAL `eyre::Report` (still wrapping
                // `LlmError`), so `error.to_string()` here would emit
                // the raw `"API error (lane): provider quota exhausted
                // — HTTP 403 ..."` instead. Downcast and re-classify
                // so the SPA sees the actionable text.
                let wire_message = classify_runtime_error_message(&error);
                // #1969 — the agent loop now attaches the turn's accumulated
                // usage to the bailed error (`PartialTurnUsage`, via
                // `attach_partial_usage`). Surface it on the error event —
                // mirroring the `done` event's `tokens_in`/`tokens_out`/
                // `tokens_cache` so the SPA sees the real spend.
                // `downcast_ref` still sees the carrier through the `LlmError`
                // classification above (both stay reachable: `wrap_err` layers
                // the carrier without hiding the inner error). Absent carrier
                // (non-loop error) → zeros.
                let (err_tokens_in, err_tokens_out, err_tokens_cache) = error
                    .downcast_ref::<octos_agent::PartialTurnUsage>()
                    .map(|partial| {
                        let usage = &partial.total;
                        (
                            u64::from(usage.input_tokens),
                            u64::from(usage.output_tokens),
                            u64::from(usage.cache_read_tokens)
                                + u64::from(usage.cache_write_tokens),
                        )
                    })
                    .unwrap_or((0, 0, 0));
                let token_usage = error.downcast_ref::<octos_agent::PartialTurnUsage>()
                    .map(|partial| EnvelopeTokenUsage {
                        input_tokens: u64::from(partial.total.input_tokens),
                        output_tokens: u64::from(partial.total.output_tokens),
                        reasoning_tokens: u64::from(partial.total.reasoning_tokens),
                        cache_read_tokens: u64::from(partial.total.cache_read_tokens),
                        cache_write_tokens: u64::from(partial.total.cache_write_tokens),
                    });
                let error = json!({
                    "type": "error",
                    "message": wire_message,
                    "tokens_in": err_tokens_in,
                    "tokens_out": err_tokens_out,
                    "tokens_cache": err_tokens_cache,
                    "token_usage": token_usage,
                });
                let _ = progress_tx_for_result.send(error.to_string()).await;
            }
        }
    }.instrument(turn_span));
    let _abort_guard = AbortOnDrop {
        abort: agent_task.abort_handle(),
    };

    let mut saw_delta = false;
    let mut task_output_delta_tracker = TaskOutputDeltaTracker::default();
    let progress_context = ProgressMappingContext::new(session_id.clone(), turn_id.clone());
    let mut interrupt_observed = false;

    loop {
        // Race progress events against the interrupt signal so an interrupt
        // can wake us out of `progress_rx.recv()` even if the agent task is
        // mid-await. The state mutex is the actual race winner; this select
        // is a notification, not a guard.
        //
        // task-interrupt-breaks-progress-wait: on `Interrupted` we BREAK
        // right here. The old shape (`continue`, then a post-select check)
        // re-entered the select with the interrupt arm disabled and waited
        // for the NEXT progress event — a silent long tool (`bash sleep …`)
        // held the terminal back indefinitely and the client's 5 s
        // turn/interrupt ack timed out.
        let event = match crate::turn_loop::next_turn_loop_step(
            &mut interrupt_rx,
            &mut progress_rx,
            interrupt_observed,
        )
        .await
        {
            crate::turn_loop::TurnLoopStep::Interrupted => {
                // The handler transitioned state to `Interrupting`. Drop any
                // remaining progress events on the floor; they are no longer
                // observable to the client.
                interrupt_observed = true;
                break;
            }
            crate::turn_loop::TurnLoopStep::Closed => break,
            crate::turn_loop::TurnLoopStep::Progress(data) => {
                match serde_json::from_str::<Value>(&data) {
                    Ok(event) => event,
                    Err(_) => continue,
                }
            }
        };
        match event.get("type").and_then(Value::as_str) {
            Some("done") => {
                if !saw_delta {
                    if let Some(content) = event.get("content").and_then(Value::as_str) {
                        if !content.is_empty() {
                            // message/delta is ephemeral per spec § 9 — drops
                            // are silent at DEBUG.
                            let _ = send_notification_ephemeral(
                                &ws,
                                &ledger,
                                UiNotification::MessageDelta(MessageDeltaEvent {
                                    session_id: session_id.clone(),
                                    topic: None,
                                    turn_id: turn_id.clone(),
                                    text: content.to_string(),
                                }),
                            );
                        }
                    }
                }
                // The agent_task spawn builds this JSON from
                // `response.token_usage` so we observe the SAME numbers that
                // flow through the cost accountant / supervisor hooks.
                let tokens_in = event.get("tokens_in").and_then(Value::as_u64).unwrap_or(0);
                let tokens_out = event.get("tokens_out").and_then(Value::as_u64).unwrap_or(0);
                // Issue #1332: thread `done` payload data into the
                // `turn/completed` lifecycle event. Tokens come from the
                // same `response.token_usage` values the cost accountant
                // sees; `session_result` is built from the cursor +
                // message_id the persist block stamped when the final
                // assistant row committed. The SSE bridge that
                // originally fed these fields was removed in
                // M9-α-5/α-6 (PR #855) — without this wiring the
                // capability-gated fields stayed `None` for every
                // WS-driven turn, leaving capability-aware clients
                // unable to tell "no data" from "stub".
                let done_cursor: Option<UiCursor> = event
                    .get("cursor")
                    .filter(|value| !value.is_null())
                    .and_then(|value| serde_json::from_value(value.clone()).ok());
                // Issue #1337 codex round-2: prefer the carrier-pinned
                // committed_seq for `TurnSessionResult`. The
                // assistant-row carrier captures its seq at stamp time;
                // the loop's `cursor` may have advanced past it onto a
                // following tool row in the trimmed-dedupe path (where
                // an assistant row with tool_calls is followed by tool
                // rows). Building `session_result.committed_seq` from
                // `cursor.seq` would break per-row identity because
                // `message_id` still points at the assistant row.
                let session_result = build_turn_session_result_from_done(&event);
                let details = TurnCompletionDetails {
                    cursor: done_cursor,
                    tokens_in: Some(u32::try_from(tokens_in).unwrap_or(u32::MAX)),
                    tokens_out: Some(u32::try_from(tokens_out).unwrap_or(u32::MAX)),
                    session_result,
                    outcome: Some(TurnTerminalOutcome::Completed),
                    token_usage: None,
                    partial_result: None,
                };
                // FIX-04: flush any accumulated drops before the lifecycle
                // terminal so the client knows the cursor is incomplete.
                flush_replay_lossy(&ws, &ledger, &session_id, &progress_dropped);
                // Keep continuation admission out of the terminal→receipt gap.
                // The dispatcher takes this same registry lock before claiming
                // a queued synthesis. No provider work is awaited under it.
                let active_registry = active_turns_registry();
                let admission = active_registry.lock().await;
                try_emit_terminal(
                    &turn_state,
                    TerminalReason::Completed,
                    &ws,
                    &ledger,
                    &session_id,
                    &turn_id,
                    None,
                    Some(details),
                    steer_buffer.as_ref(),
                )
                .await;
                drop(admission);
                break;
            }
            Some("error") => {
                let _message = event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("turn failed")
                    .to_string();
                let (code, wire_msg): (&str, String) = (
                    event
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("runtime_error"),
                    event
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("turn failed")
                        .to_string(),
                );
                let _turn_outcome = if code.contains("rate_limit")
                    || code.contains("rate_limited")
                    || wire_msg.contains("rate_limit")
                    || wire_msg.contains("rate_limited")
                    || code.contains("429")
                    || wire_msg.contains("429")
                {
                    TurnTerminalOutcome::RateLimited
                } else {
                    TurnTerminalOutcome::Errored
                };
                flush_replay_lossy(&ws, &ledger, &session_id, &progress_dropped);
                try_emit_terminal(
                    &turn_state,
                    TerminalReason::Errored,
                    &ws,
                    &ledger,
                    &session_id,
                    &turn_id,
                    Some((code, wire_msg.as_str())),
                    Some(TurnCompletionDetails {
                        // Only the structured current-turn result may supply
                        // this total. Progress cost updates are session totals.
                        token_usage: event
                            .get("token_usage")
                            .and_then(|value| serde_json::from_value(value.clone()).ok()),
                        partial_result: event
                            .get("partial_result")
                            .and_then(|value| serde_json::from_value(value.clone()).ok()),
                        ..Default::default()
                    }),
                    steer_buffer.as_ref(),
                )
                .await;
                break;
            }
            _ => {
                forward_progress_event(
                    &ws,
                    &ledger,
                    &session_id,
                    &progress_context,
                    contracts.as_ref(),
                    progress_workspace_root.as_deref(),
                    &mut task_output_delta_tracker,
                    &mut saw_delta,
                    &event,
                );
            }
        }
    }

    if interrupt_observed {
        // Stop the agent so any in-flight LLM/tool await unblocks promptly.
        agent_task.abort();
        // Esc/`/stop`/`turn/interrupt` did not used to break a still-running
        // `spawn_only` background task (e.g. `bg_research` / `bg_research`):
        // those detach into their OWN `tokio::spawn` whose `JoinHandle` is
        // dropped, not awaited, so `agent_task.abort()` above never touched
        // them and a hung pipeline kept running for minutes after the user
        // asked to stop. Cancel every non-terminal background task this
        // session registered (turns are serialized per session, so the live
        // ones belong to the turn being interrupted). `supervisor.cancel`
        // fires the per-task cancel token the spawn_only worker now races
        // against (`agent/execution.rs`), dropping the in-flight pipeline /
        // LLM / web_search future at its next poll. Idempotent: already-
        // terminal tasks return `AlreadyTerminal` and are skipped.
        cancel_session_spawn_only_tasks(&tool_registry.supervisor(), &session_id);
        // #1707 round 5 (board item #7): a terminal mirror that lands AFTER
        // this interrupt (task-status re-forward, restart replay into a
        // fresh runtime) must not re-enter the session as a
        // ChildCompleted / ScatterJoinComplete continuation — the user just
        // asked for everything to stop. Purge the session's pending
        // terminal continuations under one state lock, tombstone them
        // durably (single batched `record_continuations_coalesced`), and
        // stamp the delivered marks so a same-process re-forward collapses.
        // Scoped to the terminal pair only: GoalContinue/GoalWrapUp/LoopFire/
        // External own their lifecycle (goal pause, loop delete, fleet
        // outbox). Idempotent: a replayed interrupt finds nothing pending
        // and returns 0.
        //
        // #1707 round 5 codex round 2 (board item #13) — the purge is scoped
        // to the full `(session, profile, workspace)` triple, not the bare
        // session id: a same-named session under another profile, or the same
        // profile+session rebound to a different project folder
        // (`sessions_in_cwd`), keeps its own pending terminal items. The
        // profile is THIS turn's resolved runtime profile
        // (`session_runtime.profile.profile_id`) — the same id every agent
        // the turn spawned was registered with. The workspace is THIS turn's
        // effective tool workspace (`session_runtime.workspace_root`), the
        // same root the tool registry was bound to and the canonical cwd the
        // children inherited (the continuations' `payload:workspace` stamps
        // come from `agent.cwd`). An empty root string normalizes to `None`,
        // matching unstamped items only.
        //
        // #21 (round-4, codex #17 B3) — BOTH endpoints now encode the root
        // with `workspace_scope_encode` (hex of the raw OsStr bytes): the
        // stamp side (peer task registration) and this purge side share one
        // lossless representation, so a non-UTF-8 root stamps and purges as
        // the SAME scope instead of one side collapsing to `None` via
        // `to_str()` and silently widening the match.
        // FIX-04: also flush any accumulated drops before the lifecycle
        // terminal so the client knows the cursor is incomplete.
        flush_replay_lossy(&ws, &ledger, &session_id, &progress_dropped);
        // FIX-08: drain pending approvals tied to the interrupted turn before
        // emitting the terminal `turn/error code=interrupted`. Ordering on the
        // wire/ledger:
        //   1. agent aborted (above) — no new requests will ever arrive.
        //   2. one `approval/cancelled` per still-pending approval (durable).
        //   3. exactly one `turn/error code=interrupted` (via try_emit_terminal).
        // This matches the FIX-08 spec: cancel events appear in the ledger
        // before the terminal, so reconnect-replay clients see "moot" before
        // they see "turn gone". `cancel_pending_for_turn` is atomic
        // (single write-lock over the per-call store) and idempotent (a
        // replayed interrupt finds nothing pending and returns []).
        //
        // FIX-06 interaction: this only touches per-call pending entries.
        // `approve_for_session` scopes are turn-independent and survive;
        // `approve_for_turn` scopes are evicted by `evict_turn` below.
        //
        // TODO(M9-FIX-07-followup): mirror each cancellation into the audit
        // log (`decision: "cancelled"`, `reason: "turn_interrupted"`). FIX-08
        // intentionally limits scope to the durable ledger path; the audit
        // tap can be added without re-reading the spec.
        let cancelled = contracts.approvals.cancel_pending_for_turn(
            &session_id,
            &turn_id,
            approval_cancelled_reasons::TURN_INTERRUPTED,
        );
        for entry in cancelled {
            let _ = send_notification_durable(
                &ws,
                &ledger,
                UiNotification::ApprovalCancelled(ApprovalCancelledEvent::turn_interrupted(
                    session_id.clone(),
                    entry.approval_id,
                    entry.turn_id,
                )),
            );
        }
        // UPCR-2026-023: drain pending structured user-questions for the
        // interrupted turn, mirroring the approval drain. Dropping each
        // oneshot resolves the blocked `ask_user_question` tool to a
        // `Cancelled` outcome; the turn then terminates via the
        // `turn/error code=interrupted` emitted below (Phase-1 reuses the
        // terminal path rather than a dedicated question-cancelled signal).
        contracts.user_questions.cancel_pending_for_turn(
            &session_id,
            &turn_id,
            approval_cancelled_reasons::TURN_INTERRUPTED,
        );
        // Handler is awaiting our terminal emission + ack. Emit exactly once.
        try_emit_terminal(
            &turn_state,
            TerminalReason::Interrupted,
            &ws,
            &ledger,
            &session_id,
            &turn_id,
            Some((
                "interrupted",
                captured_interrupt_origin(&turn_state).await.message(),
            )),
            None,
            steer_buffer.as_ref(),
        )
        .await;
        // codex #2 residual — a client-interrupted peer takes THIS branch, not
        // the Completed/errored arms, so its fleet was never evaluated: a last
        // interrupted peer could leave the fleet unsynthesized. Evaluate here
        // too. An interrupt writes no fresh result.md, so if this was the peer's
        // only turn the fleet-done check correctly HOLDS (it's not done); a
        // persistent peer with a prior result is synthesized against that. The
        // point is to not SKIP evaluation on the interrupt path. No-op for
        // non-peer sessions.
        // #2003 — also evaluate on the MASTER-idle edge. No-ops for a peer
        // session (handled just above) and for a session with no fleet.
    }

    let _ = agent_task.await;

    // `turn/steer` turn-end residual: the loop keeps the turn alive for
    // steers that land BEFORE its final EndTurn check, but a steer accepted
    // after that check — or, far more commonly, one accepted while a tool
    // was running when the client then INTERRUPTED the turn (incident
    // 2026-08-17: 32 s between accept and abort) — never drains.
    // task-return-unconsumed-steer-inputs: the terminal gate
    // (`transition_to_terminal_settling_steers`) already returned every
    // leftover BEFORE the terminal frame, and the state lock in
    // `handle_turn_steer` guarantees nothing was accepted after the flip —
    // so this is a safety net that should find an empty buffer. If it ever
    // does not, returning late is still better than dropping the text.
    if let Some(buffer) = steer_buffer.as_ref() {
        let late = settle_leftover_steers(
            buffer,
            SteerReturnSink::Live {
                ws: &ws,
                ledger: &ledger,
            },
            &session_id,
            &turn_id,
            interrupt_observed,
        );
        if late > 0 {
            warn!(
                session = %session_id.0,
                turn = %turn_id.0,
                late,
                "turn/steer leftovers found AFTER the terminal gate — ordering invariant violated"
            );
        }
    }

    // NEW-16 codex round-3 P1+P2: GC moved from here to the
    // TTL-based opportunistic pruner inside the persist block.
    // Earlier rounds 1-2 evicted here, but that left two gaps:
    //   1. A queued same-`(session, turn)` re-entry scheduled
    //      AFTER this outer task completed would see a cleaned
    //      cursor and could re-persist (the guard's whole point
    //      was to prevent that).
    //   2. Connection-close abort of the OUTER
    //      `run_standalone_turn` future would skip this GC line
    //      entirely, leaking entries until process restart.
    // The TTL pruner solves both: cursor lives long enough for
    // realistic re-entries, and stale entries get evicted by
    // subsequent persist activity rather than relying on a
    // cancellation-safe cleanup path.

    // Issue #961: when the LLM invoked a `spawn_only` tool (e.g.
    // `bg_research`), the agent's main loop emits `done`/`error` and the
    // function would otherwise return — but the spawned background task
    // continues running for minutes, emitting `tool/progress` and
    // `task/updated` events via the supervisor's `set_on_change` hook.
    // Those events would be silently rejected with `Closed(..)` because
    // `progress_rx` is about to drop. Hand the receiver to a detached
    // drain task that keeps forwarding events until every sender clone
    // is released (which happens when the spawn task's `bg_reporter` Arc
    // drops at completion).
    //
    // We intentionally do NOT drain on `interrupt_observed`: the user
    // asked us to stop, so further progress is moot. We also do NOT
    // handle `done`/`error` event types in the drain — those are
    // agent-main-loop terminal signals that have already been emitted
    // above; the drain is strictly post-terminal.
    if !interrupt_observed && tool_registry.spawn_only_was_invoked() {
        let drain_ws = ws.clone();
        let drain_ledger = ledger.clone();
        let drain_session_id = session_id.clone();
        let drain_progress_context = progress_context.clone();
        let drain_contracts = contracts.clone();
        let drain_workspace_root = progress_workspace_root.clone();
        tokio::spawn(async move {
            let mut drain_tracker = task_output_delta_tracker;
            let mut drain_saw_delta = false;
            while let Some(data) = progress_rx.recv().await {
                let event: Value = match serde_json::from_str(&data) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                // `done`/`error` were already emitted by the agent's
                // main-loop terminal path; `token` would resurrect the
                // now-completed foreground turn on legacy clients (the
                // input-gate wedge). See `drain_should_skip_event`.
                if drain_should_skip_event(event.get("type").and_then(Value::as_str)) {
                    continue;
                }
                forward_progress_event(
                    &drain_ws,
                    &drain_ledger,
                    &drain_session_id,
                    &drain_progress_context,
                    drain_contracts.as_ref(),
                    drain_workspace_root.as_deref(),
                    &mut drain_tracker,
                    &mut drain_saw_delta,
                    &event,
                );
            }
            // `drain_saw_delta` only matters when callers want to backfill
            // the terminal `done`'s assistant content; the drain runs
            // post-terminal so the flag is intentionally discarded here.
            let _ = drain_saw_delta;
        });
    }

    // #1134 — await the agent_task's oneshot for the turn's final
    // `response.content`. The receiver resolves to:
    //   * `Ok(Some(content))` — the agent reached EndTurn and sent the
    //     model's actual reply BEFORE persisting any background
    //     assistant rows. The self-paced reschedule below parses
    //     `<<loop-next-in: ...>>` directly off this string.
    //   * `Ok(None)`  — the agent task returned an error.
    //   * `Err(_)`    — the sender dropped (interrupt path or task
    //     panic). The session-history fallback below is the correct
    //     conservative behaviour.
    //
    // `agent_task.await` has already returned above, so the sender
    // half is guaranteed to be either delivered or dropped — this
    // `await` will not block.
    let final_response_content: Option<String> = final_reply_rx.await.ok().flatten();

    // #1128 codex P1 re-review #2 — apply self-paced rescheduling
    // AFTER the model reply has been persisted to the per-session
    // session manager. This is the AppUI parity for what
    // `SessionActor::drain_master_continuations` does for the chat
    // path.
    //
    // #1134 — prefer the captured `response.content` from the
    // agent_task spawn. The session-history scan it replaces picks
    // `.last()` of all non-empty assistant rows after `pre`; when a
    // spawn_only / send_file path persists a background row AFTER
    // the model's final reply, `.last()` selects the background row
    // and `apply_self_paced_response` falls back to the default
    // delay. The captured content is the actual EndTurn payload, so
    // the `<<loop-next-in: ...>>` hint always wins. Fall back to the
    // session-history scan ONLY if the oneshot didn't fire (interrupt
    // / agent error path).
    if let (Some(_loop_id), Some(pre)) = (
        loop_id_for_self_paced.as_ref(),
        pre_assistant_count_for_post_turn,
    ) {
        // Only walk the session history when the oneshot didn't fire
        // — the common path (agent reached EndTurn) reads
        // `final_response_content` directly and avoids the lock
        // altogether.
        let history_fallback: Option<String> = if final_response_content.is_some() {
            None
        } else {
            let mut guard = sessions_for_reschedule.lock().await;
            let session = guard.get_or_create(&session_id).await;
            let history = session.get_history(usize::MAX);
            appui_history_last_non_empty_assistant_after(history, pre)
        };
        let assistant_reply = appui_loop_assistant_reply_for_self_paced(
            final_response_content.as_deref(),
            history_fallback,
        );
        if let Some(_reply) = assistant_reply {
            let _profile_for_reschedule = session_id
                .profile_id()
                .map(ToOwned::to_owned)
                .or_else(|| routed_profile_id.clone())
                .unwrap_or_default();
        }
    }

    // FIX-06: a turn that ends — for any reason — must drop its
    // `approve_for_turn` policy entries so a subsequent turn can't reuse
    // them. The state-machine entry itself is intentionally retained here
    // so a follow-up `turn/interrupt` for this `turn_id` can return
    // `{interrupted: false, terminal_state: "completed"}` instead of
    // `unknown_turn`. The entry is reaped on connection close
    // (`abort_connection_turns`) or when a new `turn/start` replaces it.
    contracts.scopes.evict_turn(&session_id, &turn_id);
}

/// Outcome of transitioning into a terminal state. `None` means we lost the
/// race — state was already terminal — and the caller must NOT emit anything.
struct TerminalTransition {
    /// The final terminal reason reflected on the wire. May differ from the
    /// caller's `expected` if state was `Interrupting`.
    reason: TerminalReason,
    /// Pending ack channel from a concurrent interrupt handler; signal after
    /// the wire-side emission completes.
    ack: Option<oneshot::Sender<()>>,
}

/// Issue #1337 codex round-2: build a `TurnSessionResult` from the
/// agent-task `done` JSON. Sources `committed_seq` from the
/// carrier-pinned `final_assistant_committed_seq` rather than the
/// loop-wide `cursor.seq`.
///
/// Trimmed-dedupe scenario the loop's `cursor.seq` cannot describe
/// correctly:
///
///   1. Assistant carrier row is persisted at seq N (with `tool_calls`).
///   2. Subsequent tool rows persist at seq N+1, N+2, ...
///   3. The loop's `cursor` ends up at seq N+2.
///   4. `final_assistant_message_id` was stamped at the assistant row
///      (seq N) — that's the durable per-row identity.
///
/// Building `TurnSessionResult` from `cursor.seq` here would pair the
/// assistant's `message_id` (seq N) with a tool-row `committed_seq`
/// (seq N+2). Capability-aware clients that key off
/// `session_result.{committed_seq, message_id}` would see a contract
/// violation. This helper sources `committed_seq` from
/// `final_assistant_committed_seq` so it pins to the assistant row
/// regardless of how many tool rows followed in the same turn.
///
/// Degrades to `None` when either the carrier seq or the message_id is
/// absent (no synthesis + skipped carrier + JSONL write failures).
fn build_turn_session_result_from_done(event: &Value) -> Option<TurnSessionResult> {
    let committed_seq = event
        .get("final_assistant_committed_seq")
        .and_then(Value::as_u64)?;
    let message_id = event
        .get("message_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)?;
    Some(TurnSessionResult {
        committed_seq,
        message_id,
        // `turn/start` does not carry a per-prompt `client_message_id`
        // (see `TurnStartParams`), so the originating-cmid lookup
        // degrades to `None` on the WS path. The gateway path
        // (`SessionActor::*`) carries cmids end to end — once that
        // path also pipes through `TurnCompletionDetails`, this branch
        // picks up cmid naturally.
        client_message_id: None,
    })
}

/// Cancel every still-running `spawn_only` background task this session
/// registered, on `turn/interrupt`.
///
/// Root cause this closes: a `spawn_only` tool (`bg_research` /
/// `bg_research`) detaches into its OWN `tokio::spawn` whose `JoinHandle` is
/// dropped, not awaited. The interrupt path's `agent_task.abort()` only stops
/// the foreground agent loop, so a hung pipeline kept running for minutes
/// after the user pressed Esc / `/stop`. `supervisor.cancel` fires each task's
/// cancel token, which the spawn_only worker now races against
/// (`octos-agent/src/agent/execution.rs`) and drops the in-flight pipeline /
/// LLM / web_search future at its next poll.
///
/// Turns are serialized per session, so the live background tasks belong to
/// the turn being interrupted. Already-terminal tasks are skipped
/// (`cancel` would return `AlreadyTerminal`); the call is idempotent.
fn cancel_session_spawn_only_tasks(
    supervisor: &octos_agent::TaskSupervisor,
    session_id: &SessionKey,
) {
    for task in supervisor.get_tasks_for_session(&session_id.to_string()) {
        if !task.status.is_terminal() {
            let _ = supervisor.cancel(&task.id);
        }
    }
}

/// Atomically transition the turn state to `Terminal(_)` exactly once.
/// `Active` → `Terminal(expected)`. `Interrupting { ack }` →
/// `Terminal(Interrupted)` with `ack` for the caller to signal. `Terminal(_)`
/// is left intact — caller is the loser of a race and must not emit.
async fn transition_to_terminal(
    turn_state: &TokioMutex<TurnState>,
    expected: TerminalReason,
) -> Option<TerminalTransition> {
    let mut state = turn_state.lock().await;
    let (reason, ack) = match std::mem::replace(&mut *state, TurnState::Active) {
        TurnState::Active => (expected, None),
        TurnState::Interrupting { ack, .. } => (TerminalReason::Interrupted, Some(ack)),
        TurnState::Terminal(prior) => {
            *state = TurnState::Terminal(prior);
            return None;
        }
    };
    *state = TurnState::Terminal(reason);
    Some(TerminalTransition { reason, ack })
}

/// Translate an `eyre::Report` escaping the agent loop into the
/// user-facing string the SPA renders inside a `runtime_error` envelope.
///
/// Codex round-2 MAJOR 2: the agent loop bails with the original
/// `eyre::Report` (still wrapping `LlmError`), so a naive
/// `error.to_string()` here would surface the operator-grade Display
/// string ("API error (anthropic/...): provider quota exhausted —
/// HTTP 403 ...") instead of the user-actionable
/// `HarnessError::message()` ("Provider quota exhausted (anthropic/...)
/// — top up or switch provider (...)").
///
/// Resolution ladder:
///   1. If a `HarnessError` is already in the chain (future-proofing
///      for loops that bail with a typed wrapper), use its
///      `message()` directly.
///   2. Else if an `LlmError` is in the chain, re-classify it via
///      `HarnessError::from_llm_error` and emit that message.
///   3. Fall through to the raw report Display string for non-LLM
///      failures (tool execution errors, plugin protocol errors, etc.)
///      where the report already carries the right text.
fn classify_runtime_error_message(error: &eyre::Report) -> String {
    use octos_agent::HarnessError;
    use octos_llm::LlmError;
    // Lane-attributed composites carry the complete account of which lanes
    // failed and which API style addressed each one. A typed cause names only
    // its carrier lane, so lead with the actionable typed message and retain
    // the composite summary when it is present.
    let outer = error.to_string();
    let with_lane_summary = |typed: &str| {
        if outer.contains("api_style=") {
            format!("{typed} [lanes: {outer}]")
        } else {
            typed.to_owned()
        }
    };
    for cause in error.chain() {
        if let Some(harness) = cause.downcast_ref::<HarnessError>() {
            return with_lane_summary(harness.message());
        }
    }
    for cause in error.chain() {
        if let Some(llm) = cause.downcast_ref::<LlmError>() {
            return with_lane_summary(HarnessError::from_llm_error(llm).message());
        }
    }
    outer
}

/// UPCR-2026-014 follow-up (issue #1332): optional token + session_result
/// payload threaded from the agent-task `done` event into the lifecycle
/// terminal emit. Missing on paths with no LLM token data (M9 fixture,
/// slash-command shortcut, review/start).
#[derive(Debug, Default, Clone)]
struct TurnCompletionDetails {
    cursor: Option<UiCursor>,
    tokens_in: Option<u32>,
    tokens_out: Option<u32>,
    session_result: Option<TurnSessionResult>,
    // Populated on the standalone-turn `done` path; read only by feature
    // combinations that project the terminal outcome into the lifecycle emit.
    #[allow(dead_code)]
    outcome: Option<TurnTerminalOutcome>,
    /// Exact failed-turn usage, not the input/output session cost snapshot.
    token_usage: Option<EnvelopeTokenUsage>,
    partial_result: Option<TurnErrorPartialResult>,
}

/// Atomically transition state and emit exactly one terminal event. No-op if
/// the state is already `Terminal(_)`. See `transition_to_terminal` for the
/// state-machine details.
///
/// `completion_details` is consulted only when `expected_reason` is
/// `Completed`, except `token_usage`, which also accompanies an error.
/// Populated on the standalone-turn path
/// from `done` (input/output tokens + cursor + per-row identity); left as
/// `None` for paths that do not run the LLM (slash command, review/start
/// scatter-join, M9 fixture replays).
#[allow(clippy::too_many_arguments)]
/// #48b — the Errored-terminal observability decision: returns the
/// `malformed_exhausted` event DETAIL (the marker's payload — from just
/// after the marker up to the first `:`, i.e. `feedback_limit=3
/// observed_malformed=4`) when the terminal message STARTS WITH the marker
/// (prefix only — a marker buried mid-text does not trigger), else None
/// (the ordinary turn_error path applies unchanged).
fn malformed_exhausted_detail_for_terminal(message: &str) -> Option<String> {
    let rest = message.strip_prefix(octos_agent::MALFORMED_TOOLCALL_EXHAUSTED_MARKER)?;
    let rest = rest.trim_start();
    let detail = rest.split(':').next().unwrap_or("").trim();
    (!detail.is_empty()).then(|| detail.to_owned())
}

#[allow(clippy::too_many_arguments)] // #48b: pre-existing 9-arg terminal emitter; not widened by this change
async fn try_emit_terminal(
    turn_state: &TokioMutex<TurnState>,
    expected_reason: TerminalReason,
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    turn_id: &TurnId,
    error_payload: Option<(&str, &str)>,
    completion_details: Option<TurnCompletionDetails>,
    // task-return-unconsumed-steer-inputs: the turn's `turn/steer` buffer, if
    // it has one. Settled (returned as `turn/steer_dropped`) after the state
    // flips to Terminal and BEFORE the terminal frame below — see
    // `settle_leftover_steers`.
    steer_buffer: Option<&octos_agent::SharedSteerBuffer>,
) {
    // Single terminal gate: state → Terminal, then the steer settlement
    // (`turn/steer_dropped`), then — below — the terminal frame.
    let Some(TerminalTransition { reason, ack }) = transition_to_terminal_settling_steers(
        turn_state,
        expected_reason,
        steer_buffer,
        SteerReturnSink::Live { ws, ledger },
        session_id,
        turn_id,
    )
    .await
    else {
        return;
    };

    // Terminal events are lifecycle: failure to deliver does not change the
    // state-machine outcome (the entry stays terminal for replay/idempotency)
    // but the ledger is still appended so reconnect-replay can catch up.
    match reason {
        TerminalReason::Completed => {
            let details = completion_details.unwrap_or_default();
            let tokens_in = details.tokens_in;
            let tokens_out = details.tokens_out;
            let _ = send_notification_lifecycle(
                ws,
                ledger,
                UiNotification::TurnCompleted(TurnCompletedEvent {
                    session_id: session_id.clone(),
                    topic: None,
                    turn_id: turn_id.clone(),
                    cursor: details.cursor,
                    // UPCR-2026-014 (M9-α-9) addendum fields. The SSE
                    // bridge that originally fed `session_result` was
                    // removed in M9-α-5/α-6 (PR #855); issue #1332 wires
                    // the same values straight from the agent task's
                    // `done` event so the WS path is no longer dormant.
                    tokens_in,
                    tokens_out,
                    session_result: details.session_result,
                }),
            );
            // UPCR-2026-014 M9-γ dual-emit: parallel canonical
            // `turn_completed` envelope. The hard-barrier inside
            // `emit_envelope` flips the thread's `completed` flag, so
            // any further envelope on the same thread is dropped at
            // the live emit site (spec § 14.6). Token usage zero-fills
            // reasoning / cache_read / cache_write until the upstream
            // propagation lands (legacy `tokens_in`/`tokens_out` are
            // `Option<u32>` and only the first two are populated
            // today).
            let token_usage = EnvelopeTokenUsage {
                input_tokens: tokens_in.map(u64::from).unwrap_or(0),
                output_tokens: tokens_out.map(u64::from).unwrap_or(0),
                reasoning_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            };
            let _ = ledger.emit_envelope(
                session_id,
                turn_id.0.to_string(),
                Payload::TurnCompleted { token_usage },
                None,
            );
        }
        TerminalReason::Errored => {
            let (code, message) = error_payload.unwrap_or(("runtime_error", "turn failed"));
            // #48b — OLP observability: when the terminal error CARRIES the
            // malformed-exhausted marker as a PREFIX, emit ONLY the
            // `malformed_exhausted` event row (detail derived from the
            // marker's payload) — no turn_error row for this terminal; the
            // marker in the middle of an ordinary message does not trigger.
            if let Some(detail) = malformed_exhausted_detail_for_terminal(message) {
                if let Some(data_dir) = ledger.config_data_dir() {
                    crate::obs_events::append_obs_event(
                        &data_dir,
                        &crate::obs_events::ObsEvent::new("malformed_exhausted", &detail)
                            .session(Some(session_id.0.as_str())),
                    );
                }
                let _ = send_turn_error_with_details(
                    ws,
                    ledger,
                    session_id,
                    turn_id,
                    code,
                    message,
                    completion_details,
                );
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
                return;
            }
            let _ = send_turn_error_with_details(
                ws,
                ledger,
                session_id,
                turn_id,
                code,
                message,
                completion_details,
            );
        }
        TerminalReason::Interrupted => {
            let (code, message) = error_payload.unwrap_or(("interrupted", "turn interrupted"));
            let _ = send_turn_error(ws, ledger, session_id, turn_id, code, message);
        }
    }

    if let Some(ack) = ack {
        let _ = ack.send(());
    }
}

/// UPCR-2026-014 M9-γ dual-emit helper for `MessageDelta` /
/// `ReasoningDelta` / `ToolStarted` / `ToolProgress` / `ToolCompleted` (the legacy
/// notifications surfaced by `forward_progress_event`).
///
/// For every legacy notification produced by the progress mapper, this
/// helper appends a parallel `projection/envelope` notification to the
/// ledger. The per-connection live filter
/// (`live_event_passes_capability_filter`) routes each connection to
/// exactly one shape — legacy clients see only the legacy event,
/// `projection.envelope.v1` clients see only the envelope.
///
/// Mapping from the legacy notification's `turn_id` (UUIDv7 from
/// `pre_stamp_turn_thread_id`) to envelope `thread_id`: stringify the
/// turn UUID. This mirrors the convention that
/// `pre_stamp_turn_thread_id` uses for assistant/tool row writes, so
/// envelopes and persisted message rows share the same thread identity.
///
/// `ToolCompleted.success` mapping (lossy per § 14.2 + ADR):
///   - `Some(true)`  → `EnvelopeToolEndStatus::Complete`
///   - `Some(false)` → `EnvelopeToolEndStatus::Error`
///   - `None`        → `EnvelopeToolEndStatus::Complete` (fallback;
///     legacy emits without a success bit are rare and the projection
///     treats them as a clean exit).
///
/// `Skipped` and `Aborted` variants require future signal sources
/// (deadline-skip plumbing, interrupt propagation) and are NOT
/// Compact preview of a tool call's arguments for envelope display, bounded
/// to [`octos_core::ui_protocol::ENVELOPE_TOOL_ARGUMENTS_PREVIEW_MAX`].
/// Object arguments render as `key: value` pairs (`command: "cargo test",
/// timeout: 30`) — the human `shell(…)` reading, not raw JSON braces. This is
/// display fidelity only; the full arguments stay on the live
/// `ToolStarted` notification and in the agent transcript.
fn envelope_tool_arguments_preview(arguments: &Value) -> String {
    let rendered = match arguments {
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| format!("{key}: {value}"))
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    };
    octos_core::truncated_utf8(
        &rendered,
        octos_core::ui_protocol::ENVELOPE_TOOL_ARGUMENTS_PREVIEW_MAX,
        "…",
    )
}

/// reachable through `ToolCompleted` today.
fn emit_envelope_for_legacy_notification(
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    notification: &UiNotification,
) {
    use octos_core::ui_protocol::EnvelopeToolEndStatus;
    let (thread_id, payload, client_message_id): (String, Payload, Option<String>) =
        match notification {
            UiNotification::MessageDelta(event) => (
                event.turn_id.0.to_string(),
                Payload::AssistantDelta {
                    text: event.text.clone(),
                },
                None,
            ),
            UiNotification::ReasoningDelta(event) => (
                event.turn_id.0.to_string(),
                Payload::ReasoningDelta {
                    text: event.text.clone(),
                },
                None,
            ),
            UiNotification::ToolStarted(event) => (
                event.turn_id.0.to_string(),
                Payload::ToolStart {
                    tool_call_id: event.tool_call_id.clone(),
                    name: event.tool_name.clone(),
                    // Display fidelity for the tool card (`shell(cd … && …)`),
                    // bounded so a 1MB tool-arg blob never lands in every
                    // persisted envelope + hydrate replay.
                    arguments_preview: event
                        .arguments
                        .as_ref()
                        .map(envelope_tool_arguments_preview)
                        // `{}` args render as "" — the spec says omit, not
                        // empty-string.
                        .filter(|preview| !preview.is_empty()),
                },
                None,
            ),
            UiNotification::ToolProgress(event) => {
                let Some(message) = event.message.clone() else {
                    return;
                };
                (
                    event.turn_id.0.to_string(),
                    Payload::ToolProgress {
                        tool_call_id: event.tool_call_id.clone(),
                        message,
                    },
                    None,
                )
            }
            UiNotification::ToolCompleted(event) => {
                let status = match event.success {
                    Some(true) | None => EnvelopeToolEndStatus::Complete,
                    Some(false) => EnvelopeToolEndStatus::Error,
                };
                let error = match status {
                    // Bounded like `output_preview`: the error source can be
                    // arbitrary-length tool output, and this string lands in
                    // the durable ledger + every hydrate replay.
                    EnvelopeToolEndStatus::Error => event.output_preview.as_deref().map(|s| {
                        octos_core::truncated_utf8(
                            s,
                            octos_core::ui_protocol::ENVELOPE_TOOL_OUTPUT_PREVIEW_MAX,
                            "…",
                        )
                    }),
                    _ => None,
                };
                (
                    event.turn_id.0.to_string(),
                    Payload::ToolEnd {
                        tool_call_id: event.tool_call_id.clone(),
                        status,
                        error,
                        reason: None,
                        // Result excerpt for the `⎿ …` line under the card.
                        // `ToolCompletedEvent.output_preview` is already a
                        // preview upstream; re-bound defensively so ledger
                        // growth is capped no matter what the emitter sent.
                        output_preview: event.output_preview.as_deref().map(|preview| {
                            octos_core::truncated_utf8(
                                preview,
                                octos_core::ui_protocol::ENVELOPE_TOOL_OUTPUT_PREVIEW_MAX,
                                "…",
                            )
                        }),
                        duration_ms: event.duration_ms,
                    },
                    None,
                )
            }
            _ => return,
        };
    let _ = ledger.emit_envelope(session_id, thread_id, payload, client_message_id);
}

fn progress_assistant_iteration(event: &Value) -> Option<u32> {
    event
        .get("iteration")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

/// Both ordinary and marker-filtered voice deltas use the producer identity.
fn emit_progress_envelope(
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    notification: &UiNotification,
    iteration: Option<u32>,
) {
    if let UiNotification::MessageDelta(delta) = notification
        && let Some(iteration) = iteration
    {
        let thread = delta.turn_id.0.to_string();
        let _ = ledger.emit_envelope_v2(
            session_id,
            thread.clone(),
            PayloadV2::AssistantDelta {
                text: delta.text.clone(),
                assistant_segment_id: super::events::assistant_segment_id_for_iteration(
                    &thread, iteration,
                ),
            },
            None,
        );
    } else {
        emit_envelope_for_legacy_notification(ledger, session_id, notification);
    }
}

/// Dispatch a single non-terminal progress JSON value out to the WS / ledger.
///
/// Extracted from `run_standalone_turn` so the spawn_only post-terminal drain
/// task (issue #961) can reuse the same fan-out as the main `select!` loop.
/// The function only handles `_ =>` (non-`done`/`error`) events; callers must
/// route `"done"` and `"error"` through their own terminal paths.
///
/// `saw_delta` is updated to `true` if a `MessageDelta` notification was
/// produced (callers in the main loop use it to backfill the final assistant
/// content on the terminal). The drain task passes a discard reference since
/// it runs strictly after the terminal has already been emitted.
#[allow(clippy::too_many_arguments)]
fn forward_progress_event(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    progress_context: &ProgressMappingContext,
    contracts: &UiProtocolContractStores,
    progress_workspace_root: Option<&Path>,
    task_output_delta_tracker: &mut TaskOutputDeltaTracker,
    saw_delta: &mut bool,
    event: &Value,
) {
    if let Some(delta) = task_output_delta_tracker.observe_progress_event(session_id, event) {
        // task/output/delta is durable: drops surface as
        // protocol/replay_lossy so the client can resync.
        let _ = send_notification_durable(ws, ledger, UiNotification::TaskOutputDelta(delta));
    }
    let mut mapping = map_progress_json(progress_context, event);
    apply_progress_contract_side_effects(
        contracts,
        progress_context,
        progress_workspace_root,
        event,
        &mut mapping,
    );
    for notification in mapping.notifications {
        // UPCR-2026-014 M9-γ dual-emit: every legacy notification gets a
        // canonical envelope appended to the ledger alongside it. The
        // per-connection live filter (`live_event_passes_capability_filter`)
        // routes each connection to exactly one shape: legacy clients see
        // only the legacy notification, projection.envelope.v1 clients
        // see only the envelope.
        emit_progress_envelope(
            ledger,
            session_id,
            &notification,
            progress_assistant_iteration(event),
        );
        match notification {
            UiNotification::MessageDelta(_) => {
                *saw_delta = true;
                let _ = send_notification_ephemeral(ws, ledger, notification);
            }
            UiNotification::ReasoningDelta(_) => {
                let _ = send_notification_ephemeral(ws, ledger, notification);
            }
            UiNotification::ApprovalRequested(request) => {
                if send_notification_durable(
                    ws,
                    ledger,
                    UiNotification::ApprovalRequested(request.clone()),
                )
                .is_err()
                {
                    cancel_approval_after_request_send_failure(
                        contracts,
                        ws,
                        ledger,
                        &request.session_id,
                        &request.approval_id,
                        &request.turn_id,
                    );
                }
            }
            notification => {
                let _ = send_notification_durable(ws, ledger, notification);
            }
        }
    }
    if let Some(warning) = mapping.warning {
        let _ = send_notification_durable(ws, ledger, UiNotification::Warning(warning));
    }
    if let Some(status) = mapping.status {
        // Tag with this connection's id so its forwarder skips
        // the broadcast copy after the direct send below.
        let event = ledger.append_progress_from(status.event, ws.connection_id);
        let _ = send_ledger_event_durable(ws, ledger, event.event);
    }
}

fn apply_progress_contract_side_effects(
    contracts: &UiProtocolContractStores,
    context: &ProgressMappingContext,
    workspace_root: Option<&Path>,
    event: &Value,
    mapping: &mut UiProgressMapping,
) {
    for notification in mapping.notifications.iter_mut() {
        if let UiNotification::ApprovalRequested(request) = notification {
            harden_progress_emitted_approval(request);
            contracts.approvals.request(request.clone());
        }
    }

    let Some(status) = mapping.status.as_mut() else {
        return;
    };
    let Some(notice) = status.event.metadata.file_mutation.as_mut() else {
        return;
    };
    let explicit_diff = event.get("diff").and_then(Value::as_str);
    let materialized_diff = if explicit_diff.is_none() {
        materialize_file_mutation_diff(notice, workspace_root)
    } else {
        None
    };
    let diff = explicit_diff.or(materialized_diff.as_deref());
    // `diff_previews(None)` returns the singleton installed during
    // connection-open (durable when `state.sessions` is wired,
    // ephemeral otherwise). The store does its own write-ahead before
    // the in-memory map update.
    contracts.diff_previews(None).upsert_file_mutation(
        context.session_id.clone(),
        &context.turn_id,
        notice,
        diff,
    );
}

/// Harden an `ApprovalRequestedEvent` produced from a tool/progress payload.
///
/// Tools can emit their own `approval_requested` progress event, which
/// `map_approval_requested` lifts straight into a notification. Two
/// invariants must be enforced before the event lands in the pending
/// approval store or on the wire:
///
/// 1. Risk is always sourced from the manifest. A tool-claimed risk on the
///    upstream payload is logged at WARN and dropped — it would otherwise
///    let `rm_rf` self-attest as `low`.
/// 2. Path-shaped strings inside the typed details (`cwd`,
///    `filesystem.paths`, `filesystem.writable_roots`,
///    `sandbox.writable_roots`) are passed through `sanitize_display_path`
///    so RTL overrides, zero-width characters, and traversal sequences
///    cannot spoof the rendered path.
fn harden_progress_emitted_approval(event: &mut ApprovalRequestedEvent) {
    if let Some(claimed) = event.risk.as_deref() {
        tracing::warn!(
            tool = %event.tool_name,
            claimed_risk = %claimed,
            "tool-emitted approval risk is ignored; using manifest-declared risk"
        );
    }
    event.risk = Some(server_risk_for(&event.tool_name));

    let Some(typed) = event.typed_details.as_mut() else {
        return;
    };
    if let Some(command) = typed.command.as_mut() {
        if let Some(cwd) = command.cwd.as_deref() {
            command.cwd = Some(sanitize_display_path(cwd));
        }
    }
    if let Some(filesystem) = typed.filesystem.as_mut() {
        for path in filesystem.paths.iter_mut() {
            *path = sanitize_display_path(path);
        }
        for root in filesystem.writable_roots.iter_mut() {
            *root = sanitize_display_path(root);
        }
    }
    if let Some(sandbox) = typed.sandbox.as_mut() {
        for root in sandbox.writable_roots.iter_mut() {
            *root = sanitize_display_path(root);
        }
    }
}

fn materialize_file_mutation_diff(
    notice: &UiFileMutationNotice,
    workspace_root: Option<&Path>,
) -> Option<String> {
    let path = PathBuf::from(&notice.path);
    let absolute_path = if path.is_absolute() {
        path
    } else if let Some(workspace_root) = workspace_root {
        workspace_root.join(path)
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let git_root = find_git_root_for_path(&absolute_path)?;
    let relative_path = absolute_path.strip_prefix(&git_root).ok()?;

    // Primary: working-tree vs index for a TRACKED file — the common edit case.
    let output = Command::new("git")
        .arg("-C")
        .arg(&git_root)
        .arg("diff")
        .arg("--")
        .arg(relative_path)
        .output()
        .ok()?;
    // A non-zero exit is a genuine git error (a corrupt/overridden index, or a
    // path git rejects) — NOT "the file is untracked". Give up rather than
    // mis-render a tracked file as wholly-added through the fallback below.
    if !output.status.success() {
        return None;
    }
    if let Some(diff) = finish_diff_preview(output.stdout) {
        return Some(diff);
    }

    // Fallback: a brand-new file the agent just created is UNTRACKED, so the
    // plain `git diff` above reports nothing and the preview would render an
    // empty "ready" box with no hunks. Render the whole file as additions.
    render_untracked_file_as_additions(&git_root, relative_path, &absolute_path)
}

/// Trim, cap, and drop-if-empty a raw `git diff` stdout into a preview body.
/// `None` means "no renderable diff".
fn finish_diff_preview(stdout: Vec<u8>) -> Option<String> {
    let diff = String::from_utf8(stdout).ok()?;
    let diff = truncate_utf8(diff.trim_end().to_owned(), MAX_DIFF_PREVIEW_BYTES);
    (!diff.is_empty()).then_some(diff)
}

/// Render a newly-created UNTRACKED file as an all-additions diff via
/// `git diff --no-index /dev/null <file>` (read-only — it never touches the
/// index). `--no-index` exits 1 when the two paths differ (the normal case),
/// so a renderable diff is judged by non-empty output rather than the exit
/// code. `/dev/null` is Unix-only; on other platforms the file keeps today's
/// behavior (empty preview, no regression).
#[cfg(unix)]
fn render_untracked_file_as_additions(
    git_root: &Path,
    relative_path: &Path,
    absolute_path: &Path,
) -> Option<String> {
    if !absolute_path.is_file() || !file_is_untracked(git_root, relative_path) {
        return None;
    }
    // Containment guard: this fallback reads the file directly, so a notice
    // path that escapes the repo — a `..` component, or a symlinked directory
    // that the lexical `strip_prefix` accepted — must NOT be read. Resolve BOTH
    // sides (so a symlinked repo root, e.g. macOS `/tmp` -> `/private/tmp`,
    // still matches) and derive the repo-relative path from the *canonical*
    // file. `strip_prefix` succeeds only when the file lives below the real git
    // root, so it doubles as the containment check.
    let canonical_root = git_root.canonicalize().ok()?;
    let canonical_file = absolute_path.canonicalize().ok()?;
    let safe_relative = canonical_file.strip_prefix(&canonical_root).ok()?;
    // Diff the SYMLINK-FREE canonical path (relative to the canonical root) so
    // git does not re-follow a symlink we already resolved, and the preview
    // still shows the tidy repo-relative path.
    //
    // Residual TOCTOU (accepted): a concurrent process could swap one of the
    // now-real parent directories for an outside-pointing symlink between this
    // check and git's open. Closing that fully needs `openat2(RESOLVE_BENEATH)`
    // / a capability-`Dir`, which this `deny(unsafe_code)` crate can't express
    // without a carve-out; in the multi-tenant deployment the sandbox's
    // per-tenant FS isolation bounds that race, and the static `..`/symlink
    // vectors (the reachable ones) are closed above.
    let output = Command::new("git")
        .arg("-C")
        .arg(&canonical_root)
        .arg("diff")
        .arg("--no-index")
        .arg("--")
        .arg("/dev/null")
        .arg(safe_relative)
        .output()
        .ok()?;
    finish_diff_preview(output.stdout)
}

#[cfg(not(unix))]
fn render_untracked_file_as_additions(
    _git_root: &Path,
    _relative_path: &Path,
    _absolute_path: &Path,
) -> Option<String> {
    None
}

/// True when git is not tracking `relative_path` (newly created, never
/// `git add`ed). `git ls-files --error-unmatch` exits 0 when the path IS
/// tracked, 1 when it is not, and 128 on a genuine git error. Treat ONLY the
/// explicit unmatched code (1) as untracked, so a repo-level failure is never
/// mistaken for "new file" (which would wrongly render it as wholly-added).
#[cfg(unix)]
fn file_is_untracked(git_root: &Path, relative_path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(git_root)
        .arg("ls-files")
        .arg("--error-unmatch")
        .arg("--")
        .arg(relative_path)
        .output()
        .map(|output| output.status.code() == Some(1))
        .unwrap_or(false)
}

fn find_git_root_for_path(path: &Path) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    start
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    value.truncate(boundary);
    value
}

fn truncate_for_display(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn prompt_text(input: &[InputItem]) -> Option<String> {
    let parts = input
        .iter()
        .filter_map(|item| match item {
            InputItem::Text { text } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
        .collect::<Vec<_>>();

    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn task_id_field(event: &Value) -> Option<TaskId> {
    event.get("task_id").and_then(Value::as_str)?.parse().ok()
}

fn task_output_delta_text(event: &Value) -> Option<String> {
    match event.get("type").and_then(Value::as_str)? {
        "tool_progress" | "task_progress" | "task_output" => string_field(
            event,
            &["text", "output", "progress_message", "message", "status"],
        ),
        "tool_end" => string_field(event, &["output_preview"]),
        _ => None,
    }
    .filter(|text| !text.is_empty())
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn runtime_unavailable_error(message: impl Into<String>) -> RpcError {
    RpcError::internal_error(message).with_data(json!({
        "kind": "runtime_unavailable",
    }))
}

/// True when `error` (anywhere in its eyre chain) is an OS permission-denied
/// failure. The common trigger is session bootstrap failing to create
/// `<workspace>/.octos-workspace.toml` in a folder the user can't write to.
/// Structural — downcasts to `std::io::Error` — not string matching.
fn is_permission_denied_error(error: &eyre::Report) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

/// Clear, actionable RPC error for the "session workspace folder isn't writable"
/// case (a session/open bootstrap denied writing its `.octos-workspace.toml`).
/// Names the folder so the user can act; the client renders `data.message`
/// verbatim (preferred over the numeric code), so it reads as a plain sentence
/// instead of an opaque "failed to bootstrap session runtime".
fn workspace_not_writable_error(workspace: Option<&str>) -> RpcError {
    let sentence = match workspace {
        Some(path) => format!(
            "Can't start a session in {path} — the folder isn't writable (permission denied). \
             octos needs to create a .octos-workspace.toml there. Start octos in a folder you \
             own, or make this one writable."
        ),
        None => "Can't start a session — the workspace folder isn't writable (permission denied). \
                 Start octos in a folder you own, or make it writable."
            .to_string(),
    };
    RpcError::internal_error(sentence.clone()).with_data(json!({
        "kind": "workspace_not_writable",
        "workspace": workspace,
        "message": sentence,
    }))
}

/// Clear, actionable RPC error for "another octos process already owns this
/// profile's data directory" — redb is single-writer-single-process, so a
/// second `octos serve` against the same data dir can never open the episode
/// store.
///
/// This is a deployment mistake with two concrete fixes, not an internal
/// fault, so it gets its own `kind` (clients can render a remedy instead of a
/// stack-shaped string) and the sentence names both ways out. `data.message`
/// is rendered verbatim by clients, matching [`workspace_not_writable_error`].
fn data_dir_locked_error(profile_id: &str, error: &eyre::Report) -> RpcError {
    let sentence = format!(
        "Can't start a session for profile '{profile_id}' — another octos process already \
         owns this profile's data directory, and its storage allows only one writer. \
         Stop the other `octos serve` (if it is supervised, e.g. by launchd, stop the \
         service rather than the process — it will be restarted otherwise), or give this \
         instance its own storage with `--instance-data-dir <dir>`. Details: {error:#}"
    );
    RpcError::internal_error(sentence.clone()).with_data(json!({
        "kind": "data_dir_locked",
        "profile_id": profile_id,
        "message": sentence,
    }))
}

fn assistant_message_projection(
    response: &octos_agent::ConversationResponse,
    message_index: usize,
    turn: &str,
) -> Option<MessageProjectionOverride> {
    response
        .assistant_segments
        .message_iterations
        .iter()
        .find(|(index, _)| *index == message_index)
        .map(|(_, iteration)| {
            MessageProjectionOverride::AssistantSegment(
                super::events::assistant_segment_id_for_iteration(turn, *iteration),
            )
        })
}

fn final_assistant_segment_id(response: &octos_agent::ConversationResponse, turn: &str) -> String {
    let identity = super::events::assistant_segment_id_for_iteration(
        turn,
        response.assistant_segments.final_iteration,
    );
    // A controller-authored final can follow a tool-bearing model preamble
    // from this same iteration. It is not that preamble's canonical carrier.
    if response
        .assistant_segments
        .message_iterations
        .iter()
        .any(|(index, iteration)| {
            *iteration == response.assistant_segments.final_iteration
                && response.messages.get(*index).is_some_and(|message| {
                    message.role == MessageRole::Assistant
                        && !is_metadata_only_assistant_row(message)
                })
        })
    {
        format!("{identity}:final")
    } else {
        identity
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn final_assistant_message_for_response(
    response: &octos_agent::ConversationResponse,
) -> Option<Message> {
    if response.content.is_empty()
        || response
            .assistant_segments
            .message_iterations
            .iter()
            .any(|(index, iteration)| {
                *iteration == response.assistant_segments.final_iteration
                    && response.messages.get(*index).is_some_and(|message| {
                        is_final_assistant_carrier_under_trimmed_equality(
                            message,
                            &response.content,
                        )
                    })
            })
    {
        return None;
    }
    let mut message = Message::assistant(response.content.clone());
    message.reasoning_content = response.reasoning_content.clone();
    Some(message)
}

#[cfg(test)]
fn final_assistant_message(
    messages: &[Message],
    content: &str,
    reasoning_content: Option<String>,
) -> Option<Message> {
    if content.is_empty() || final_assistant_content_already_persisted(messages, content) {
        return None;
    }

    let mut message = Message::assistant(content.to_owned());
    message.reasoning_content = reasoning_content;
    Some(message)
}

/// NEW-10 (Fleet-UX soak, mini3/kimi-k2.5, 2026-05-23): kimi-k2.5
/// occasionally repeats its iter-N preamble verbatim in the EndTurn
/// `response.content` after a fan-out of `get_weather` calls. The
/// iter-N `response.messages` Assistant carrier (the one that holds
/// `tool_calls`) sometimes lands with text content **trimmed-equal**
/// to `response.content` but differing by trailing whitespace
/// (`\n`, a frequent streaming artefact). The pre-fix byte-exact
/// dedupe in `final_assistant_message` missed; both rows persisted;
/// the SPA rendered two identical chat bubbles around the tool chip.
///
/// Strategy (codex round-1 P2 narrowing):
///
/// 1. **Empty-after-trim guard.** If the final content collapses to
///    `""` after `trim()`, the caller has nothing meaningful to
///    dedupe against — return `false` so the helper is composable
///    in isolation. The caller's `content.is_empty()` branch
///    remains the single source of truth for "skip synthesis".
///
/// 2. **Trimmed-equality ONLY**, over every prior Assistant
///    message. Catches the actual reported pattern
///    (whitespace-only-difference) without the false-positive risk
///    a substring-containment check would carry. Codex round 1
///    flagged that substring containment could suppress legitimate
///    final answers in shapes like `iter-N="I'll verify whether
///    <answer>..."` followed by `final="<answer>"` — `<answer>` is
///    a substring of the iter-N text, but the iter-N row is NOT a
///    duplicate carrier of the final answer. Restricting to
///    trimmed-equality eliminates that ambiguity: only when the
///    iter-N content equals the final content (modulo whitespace)
///    can we be sure it is a duplicate render of the same answer.
///
/// 3. **Legitimate two-bubble flows are preserved.** When the
///    iter-N carrier holds a preamble like "Looking that up..."
///    and the EndTurn produces a different long answer, the two
///    contents are not trimmed-equal — both rows persist as before
///    and the SPA renders the preamble bubble + the answer bubble
///    around the tool chip.
///
/// Round-1 P2 carrier-flip companion: callers that wire
/// `final_assistant_persisted` (the `response.messages` persist loop
/// in `handle_chat_turn`) MUST also use this helper's semantics
/// when deciding whether an iter-N row counts as the final-assistant
/// carrier — see `is_final_assistant_carrier_under_trimmed_equality`
/// for the matching predicate. Without that, a trimmed-equal iter-N
/// row that flipped this helper to true would land with
/// `final_assistant_persisted=false`, the `final_send` oneshot
/// would resolve to `None`, and self-paced loops carrying
/// `<<loop-next-in: ...>>` hints in the EndTurn text could fall
/// back to the unsafe `.last()` history scan.
#[cfg(test)]
fn final_assistant_content_already_persisted(messages: &[Message], content: &str) -> bool {
    let trimmed_content = content.trim();
    if trimmed_content.is_empty() {
        return false;
    }
    messages
        .iter()
        .any(|message| is_assistant_row_trimmed_equal(message, trimmed_content))
}

/// Trimmed-equality predicate shared between the synth-skip helper
/// (`final_assistant_content_already_persisted`) and the persist-loop
/// carrier flip (`is_final_assistant_carrier_under_trimmed_equality`).
///
/// Both call sites MUST use this same predicate, otherwise a row
/// that the synth-skip helper recognises as a duplicate carrier
/// would not flip `final_assistant_persisted` in the persist loop,
/// causing the `final_send` oneshot to resolve to `None` for
/// self-paced loops (codex round-1 P2 on PR #1231).
///
/// Tool rows return `false` because tool outputs render as chips,
/// not chat bubbles, so they cannot be the source of (or target of
/// dedupe against) a duplicate bubble.
///
/// Caller is responsible for trimming `trimmed_content` BEFORE the
/// call. Passing the un-trimmed value is a silent bug: a final
/// content of `"X "` would not match an iter-N carrier of `"X"`.
fn is_assistant_row_trimmed_equal(message: &Message, trimmed_content: &str) -> bool {
    message.role == MessageRole::Assistant && message.content.trim() == trimmed_content
}

/// NEW-10 codex round-1 P2 carrier-flip companion: returns true
/// when the iter-N `message` from `response.messages` is the
/// trimmed-equal carrier of `response_content` and the synth-skip
/// helper (`final_assistant_content_already_persisted`) would
/// recognise it as such.
///
/// The persist loop in `handle_chat_turn` calls this to decide
/// whether to flip `final_assistant_persisted=true` after the
/// `add_message_with_seq` Ok branch. The pre-#1231 byte-exact
/// `message.content == response.content` check is a strict subset
/// of this; this helper extends it to the trimmed case so callers
/// cover the same set of rows the synth-skip helper deduplicates.
///
/// `response_content` is the raw (un-trimmed) value; the helper
/// trims internally to keep both call sites in lockstep without
/// the caller having to remember to pre-trim.
fn is_final_assistant_carrier_under_trimmed_equality(
    message: &Message,
    response_content: &str,
) -> bool {
    if response_content.is_empty() {
        return false;
    }
    let trimmed_response = response_content.trim();
    if trimmed_response.is_empty() {
        return false;
    }
    is_assistant_row_trimmed_equal(message, trimmed_response)
}

/// On the NEW-03 synthesised-spawn-only-ack skip path, pick the text
/// to feed the post-turn self-paced reschedule oneshot.
///
/// Inputs:
/// - `synth_ack_content`: the synthesised "Background work started
///   for `<tool>`. ..." text from `response.content`.
/// - `last_persisted_preamble_assistant`: the LAST non-empty
///   preamble assistant row that the persist loop committed for this
///   turn, if any.
///
/// Strategy:
/// - When a preamble row was persisted (e.g. a self-paced loop emits
///   "Starting... <<loop-next-in: 60s>>" alongside the spawn_only
///   tool call), return that preamble text. The post-turn parser
///   reads its `<<loop-next-in: ...>>` hint deterministically
///   without depending on the history-fallback walk (whose `.last()`
///   semantics can race with a fast `BackgroundResultSender` row
///   landing before the post-turn block runs). (Codex round-3 P2.)
/// - Otherwise return the synth-ack text. The parser finds no hint
///   and `apply_self_paced_response` stamps the default delay,
///   keeping a bare-spawn-only self-paced loop scheduled. (Codex
///   round-1 P2.)
#[cfg_attr(not(test), allow(dead_code))]
fn captured_final_reply_for_synth_ack_skip(
    synth_ack_content: String,
    last_persisted_preamble_assistant: Option<&str>,
) -> String {
    last_persisted_preamble_assistant
        .map(ToOwned::to_owned)
        .unwrap_or(synth_ack_content)
}

/// NEW-03 (PR #1190): the gate that decides whether the JSONL persist
/// site must SKIP committing the synthesised "Background work started
/// for `<tool>`." ack row.
///
/// The gate is INTENTIONALLY tool-name-agnostic. It keys solely on
/// two booleans observable at the persist site:
///
/// - `synthesized_from_spawn_only`: the agent-loop flag that lifts to
///   `true` only when the CURRENT iteration's response contains ANY
///   tool call where `tools.is_spawn_only(tc.name)` returns true
///   (`agent/loop_runner.rs::process_message_inner`). Any
///   plugin/manifest-declared spawn_only tool, plus the builtin
///   `bg_research` registered with `mark_spawn_only` in
///   `runtime/profile.rs`, flips this bit — so the gate naturally
///   covers `podcast_generate`, `podcast_voices` (if spawn_only-marked
///   by its manifest), `bg_research`, `mofa_slides`, `bg_research`,
///   and any future spawn_only tool with no change here.
/// - `final_assistant_in_scope`: the local
///   `final_assistant_message(...)` returned `Some(_)` — i.e. the
///   synthesised carrier Message exists and would otherwise be
///   committed. Without this guard a `synthesized_from_spawn_only`
///   response whose `final_assistant_message` filter returned `None`
///   (defensive: the carrier was filtered out upstream) would set the
///   skip flag, but the surrounding `if let Some(message) =
///   final_assistant` block already short-circuits in that case.
///   Keeping both conditions in one helper makes the invariant
///   testable.
///
/// Round-2 soak NEW-03 follow-up: extracted from the inline persist
/// site so the name-agnostic invariant can be locked in by a
/// parameter-only unit test, without requiring a full
/// `run_standalone_turn` harness.
fn should_skip_synthesized_spawn_only_ack_persist(
    synthesized_from_spawn_only: bool,
    final_assistant_in_scope: bool,
) -> bool {
    synthesized_from_spawn_only && final_assistant_in_scope
}

async fn abort_connection_turns(
    active_turns: &SharedActiveTurns,
    connection_turns: &SharedConnectionTurns,
    scopes: &ScopePolicy,
    ledger: &UiProtocolLedger,
    approvals: &PendingApprovalStore,
    user_questions: &PendingQuestionStore,
) {
    let turns = std::mem::take(&mut *connection_turns.lock().await);
    if turns.is_empty() {
        return;
    }

    let mut active = active_turns.lock().await;
    for (session_id, registered) in turns {
        let turn_id = registered.turn_id.clone();
        // Reused client IDs do not confer ownership of a newer dispatch.
        if active
            .get(&session_id)
            .is_some_and(|current| !registered.matches(current))
        {
            continue;
        }
        let mut aborted_state: Option<Arc<TokioMutex<TurnState>>> = None;
        let mut aborted_steer: Option<octos_agent::SharedSteerBuffer> = None;
        let should_abort = active
            .get(&session_id)
            .is_some_and(|active| registered.matches(active));
        if should_abort {
            if let Some(active) = active.remove(&session_id) {
                aborted_state = Some(active.state.clone());
                aborted_steer = active.steer.clone();
                active.abort.abort();
            }
        }
        // #920.1: append a durable terminal event so reconnect-replay
        // sees this turn end. Without this the in-flight turn vanishes
        // from the live registry but no `turn/error` lands, so clients
        // render an indefinite spinner. Use the same single-fire
        // transition the rest of the lifecycle uses so we don't race
        // with a natural completion / interrupt that may already have
        // flipped state to Terminal.
        if let Some(state) = aborted_state {
            // task-return-unconsumed-steer-inputs: same terminal gate as the
            // live path — state Terminal → `turn/steer_dropped` (ledger-only:
            // the connection is gone, the reconnecting client replays it) →
            // terminal. Without this the aborted turn task's safety-net drain
            // would land AFTER this terminal and a same-process reconnect
            // would lose the steer.
            if let Some(transition) = transition_to_terminal_settling_steers(
                state.as_ref(),
                TerminalReason::Interrupted,
                aborted_steer.as_ref(),
                SteerReturnSink::LedgerOnly { ledger },
                &session_id,
                &turn_id,
            )
            .await
            {
                let _ = ledger.append_notification(UiNotification::TurnError(TurnErrorEvent {
                    session_id: session_id.clone(),
                    topic: None,
                    turn_id: turn_id.clone(),
                    code: "connection_closed".to_owned(),
                    message: "connection closed before turn completed".to_owned(),
                    token_usage: None,
                    partial_result: None,
                }));
                // OLP L1 (slice 5): turn_error event, best-effort. The
                // ledger's configured data dir is the same root serve
                // writes events.jsonl to; None (RAM-only) drops the event.
                if let Some(data_dir) = ledger.config_data_dir() {
                    crate::obs_events::append_obs_event(
                        &data_dir,
                        &crate::obs_events::ObsEvent::new(
                            "turn_error",
                            "connection closed before turn completed",
                        )
                        .session(Some(session_id.0.as_str())),
                    );
                }
                if let Some(ack) = transition.ack {
                    let _ = ack.send(());
                }
            }
        }
        // #920.2: cancel every still-pending approval for the aborted
        // turn and append a durable `approval/cancelled` for each so a
        // reconnect doesn't re-show a modal for a dead turn.
        let cancelled = approvals.cancel_pending_for_turn(
            &session_id,
            &turn_id,
            approval_cancelled_reasons::TURN_INTERRUPTED,
        );
        for entry in cancelled {
            let _ = ledger.append_notification(UiNotification::ApprovalCancelled(
                ApprovalCancelledEvent {
                    session_id: session_id.clone(),
                    topic: session_id.topic().map(ToOwned::to_owned),
                    approval_id: entry.approval_id,
                    turn_id: entry.turn_id,
                    reason: approval_cancelled_reasons::TURN_INTERRUPTED.to_owned(),
                },
            ));
        }
        // UPCR-2026-023: also drain pending structured user-questions for the
        // aborted turn so a reconnect doesn't re-show a question for a dead
        // turn and the blocked `ask_user_question` tool unblocks (Cancelled).
        user_questions.cancel_pending_for_turn(
            &session_id,
            &turn_id,
            approval_cancelled_reasons::TURN_INTERRUPTED,
        );
        // FIX-06: connection close is the de-facto "session close" hook in
        // v1alpha1 — drop every recorded scope for this session so it cannot
        // outlive the WebSocket. Per M9-FIX-06 § "Out of scope", an explicit
        // `session/close` wire event would be a cleaner trigger; until then
        // this best-effort hook is the canonical place.
        scopes.evict_session(&session_id);
    }
}

/// Build the wire frame for a JSON value, returning `None` and incrementing
/// the lifecycle-error counter on serialization failure (which only happens
/// when a payload contains non-serializable data; treat as lifecycle).
fn frame_for<T: serde::Serialize>(value: &T) -> Option<WsMessage> {
    match app_ui_codec::to_compact_json(value) {
        Ok(text) => frame_text_within_cap(text).map(WsMessage::text),
        Err(error) => {
            metrics::counter!("ws.send.error.lifecycle").increment(1);
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                error = %error,
                "failed to serialize ws frame"
            );
            None
        }
    }
}

/// Turn a serialized frame into a deliverable (< [`MAX_TEXT_FRAME_BYTES`]) body,
/// or `None` if it cannot be made deliverable.
///
/// HARD GUARANTEE / last-resort behavior: [`frame_for`] must NEVER cause an
/// over-cap frame to be enqueued/sent as a successful send. [`preview_oversized_frame`]
/// truncates the largest string field(s) and then the largest JSON array(s) to
/// fit under [`TRUNCATED_FRAME_TARGET_BYTES`]; that recovers every realistic
/// oversized frame (one dominant string OR a large history/list array). If after
/// ALL of that the body is STILL over [`MAX_TEXT_FRAME_BYTES`] — only reachable
/// for a pathological frame (one giant non-string scalar, or non-JSON / parse
/// failure that cannot be rewritten) — we return `None`.
///
/// Returning `None` is safe at every one of the five [`frame_for`] call sites.
/// NOTIFICATION callers tolerate `None` by SKIPPING the enqueue (never
/// unwraps/panics) — dropping a notification is fine because no request id is
/// awaiting a reply:
///   * `send_raw_notification_ephemeral` -> `.ok_or(SendError::BackpressureDrop)?`
///   * `send_notification_ephemeral` -> `.ok_or(SendError::BackpressureDrop)?`
///   * `frame_from_ledger` -> returns the `Option<WsMessage>` directly (caller
///     `send_ledger_event_durable` maps `None` -> `Err(SendError::BackpressureDrop)`).
///
/// REQUEST/RESPONSE callers MUST NOT silently drop on `None` — the request
/// handlers ignore the returned `Result` (`let _ =`), so a dropped reply would
/// strand the client on its request id forever. They instead synthesize a
/// guaranteed-tiny same-id minimal JSON-RPC error reply (see
/// [`send_minimal_rpc_error_fallback`]):
///   * `send_rpc_result` -> on `None`, send a minimal same-id error response.
///   * `send_rpc_error` -> on `None`, send a minimal same-id error (drop the
///     oversized `error.data` / `error.message`).
///
/// So a still-over-cap NOTIFICATION frame is dropped (not sent) and logged,
/// which is strictly better than emitting a contract-violating over-cap frame:
/// the over-cap frame would be rejected by the transport's `frame_too_large`
/// guard anyway, so it is undeliverable either way — `None` just makes that
/// explicit and observable (no over-cap WsMessage is ever constructed). For an
/// over-cap RESPONSE the client still receives a same-id error reply.
fn frame_text_within_cap(text: String) -> Option<String> {
    let previewed = preview_oversized_frame(text);
    if previewed.len() > MAX_TEXT_FRAME_BYTES {
        metrics::counter!("ws.send.error.over_cap_dropped").increment(1);
        // Surface the routing method (best-effort parse) + byte size so this is
        // observable. The over-cap body is dropped, never enqueued.
        let method = serde_json::from_str::<Value>(&previewed)
            .ok()
            .and_then(|v| v.get("method").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| "<unparseable>".to_owned());
        tracing::warn!(
            target: "octos::ui_protocol::ws",
            method = %method,
            bytes = previewed.len(),
            cap = MAX_TEXT_FRAME_BYTES,
            "outbound frame still over cap after truncation; dropping (not enqueued)"
        );
        return None;
    }
    Some(previewed)
}

/// Oversized outbound frame -> head+tail preview (truncate-in-place, NO disk).
///
/// Today an outbound UI-protocol frame whose serialized length exceeds
/// [`MAX_TEXT_FRAME_BYTES`] (1 MiB) is undeliverable: the transport's
/// `validate_text_frame_boundary` rejects it with `frame_too_large` and the
/// content is lost ("Message too large"). This is the codex pattern for
/// user-facing output: when a frame would exceed the cap, truncate its
/// largest string field(s) IN PLACE to a head + tail preview (keep the
/// beginning AND the end, drop the middle) with a byte-count marker, so the
/// frame becomes deliverable (< 1 MiB) and the user still sees the start and
/// the end of the dominant payload. NO disk write, NO FileRef, NO file store,
/// NO session/owner scoping (that whole class of complexity is intentionally
/// avoided — it is the entire point of this approach).
///
/// Because EVERY outbound frame flows through [`frame_for`], this covers
/// every oversized frame type: `assistant_persisted` / `message_delta`
/// notifications, `tool_end` errors, the retired persisted-message /
/// `ToolCompleted` notifications, and `session/hydrate` RPC results.
///
/// Algorithm (serialized frame `> MAX_TEXT_FRAME_BYTES`):
///   1. Parse the frame JSON. Parse failure -> return the original unchanged;
///      [`frame_text_within_cap`] then drops it (returns `None`) because it is
///      still over cap and cannot be rewritten.
///   2. Find the LARGEST string field (the dominant payload) by JSON-escaped
///      length, descending recursively into objects/arrays.
///   3. Rewrite that field to a head+tail preview: keep the first H and last T
///      bytes (UTF-8 char-boundary safe — never split a codepoint), drop the
///      middle, insert `\n…… [<N> bytes truncated] ……\n` between head and
///      tail (N = dropped byte count of the ORIGINAL field). The field's
///      budget is computed by ESCAPED length so the rewritten frame is
///      provably under the target. Already-previewed field PATHS are tracked
///      in a `HashSet` so each is rewritten at most once (idempotent, no
///      content-sniffing).
///   4. Re-serialize; if still over (multiple dominant fields), truncate the
///      next-largest field too; repeat until under cap.
///   5. STRUCTURAL case: if no string field can be further truncated but the
///      frame is still over target, find the LARGEST JSON array and drop its
///      middle/trailing elements (keeping valid JSON — elements are simply
///      removed; the count is implicit, no heterogeneous marker is injected
///      into an array of objects). Re-measure and keep iterating over strings
///      and arrays until under target or nothing left to shrink. Most
///      structurally-huge frames are big because of one large array
///      (history/list), so this recovers the realistic structural case while
///      keeping the frame valid JSON.
///   6. LAST RESORT: if after ALL string + array truncation the body is STILL
///      over target (pathological: one giant non-string scalar, or a parse
///      failure that could not be rewritten), return the best-effort body
///      unchanged. [`frame_text_within_cap`] then observes it is still over
///      [`MAX_TEXT_FRAME_BYTES`] and returns `None` so `frame_for` drops it
///      (never enqueues an over-cap frame). Never panics, never emits an
///      over-cap frame as a successful send.
fn preview_oversized_frame(text: String) -> String {
    // Fast path: under the cap -> byte-identical pass-through.
    if text.len() <= MAX_TEXT_FRAME_BYTES {
        return text;
    }
    let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
        // Not parseable as JSON -> cannot rewrite; return unchanged. The
        // caller (`frame_text_within_cap`) drops it as still-over-cap.
        return text;
    };

    // Single-pass design (黑板第 2 条 2c, replacing the O(payload × rounds)
    // truncate-one/re-serialize loop that cost ~10s on multi-MB hydrate
    // replies):
    //   1. measure the serialized length ONCE;
    //   2. one walk collects every truncatable string (path, escaped len,
    //      raw len), sorted largest-first;
    //   3. compute each field's escaped budget against a running overhead
    //      (current length minus everything already truncated), preview it
    //      head+tail WITHOUT re-serializing, and subtract the savings —
    //      until the running estimate fits the target;
    //   4. serialize ONCE to verify; if the estimate was optimistic (rare:
    //      escape-factor drift), run ONE structural fallback round
    //      (array-shrink loop) — so the whole function performs at most 2
    //      full serializations.
    let initial_len = match serde_json::to_string(&value) {
        Ok(s) => s.len(),
        Err(_) => return text,
    };
    if initial_len <= TRUNCATED_FRAME_TARGET_BYTES {
        return serde_json::to_string(&value).unwrap_or(text);
    }

    // Pass 1: collect all truncatable strings, largest first.
    let mut candidates: Vec<(Vec<PathSeg>, usize, usize)> = Vec::new();
    let mut path: Vec<PathSeg> = Vec::new();
    collect_truncatable_strings(&value, &mut path, &mut candidates);
    candidates.sort_by_key(|c| std::cmp::Reverse(c.1));

    let mut running_len = initial_len;
    for (path, field_escaped_len, field_raw_len) in &candidates {
        if running_len <= TRUNCATED_FRAME_TARGET_BYTES {
            break;
        }
        // Overhead = current frame minus this field's escaped contribution
        // (escaped bytes + two surrounding quote bytes).
        let overhead = running_len.saturating_sub(field_escaped_len + 2);
        let field_escaped_budget = TRUNCATED_FRAME_TARGET_BYTES
            .saturating_sub(overhead)
            .saturating_sub(2);
        let preview = match build_head_tail_preview(
            field_at_path(&value, path)
                .and_then(Value::as_str)
                .unwrap_or(""),
            *field_raw_len,
            field_escaped_budget,
        ) {
            Some(preview) => preview,
            None => UNPREVIEWABLE_STUB.to_owned(),
        };
        // Running estimate: new field escaped length is at most the budget
        // we handed out (marker reserve included); estimate conservatively
        // with the actual preview's escaped length instead — one cheap
        // scan, no serialization.
        let new_escaped = json_escaped_len_bytes(preview.as_bytes());
        if !set_field_at_path(&mut value, path, Value::String(preview)) {
            return text;
        }
        running_len = overhead + 2 + new_escaped;
    }

    // Structural fallback: strings alone could not fit (or did, and this
    // verifies it). Serialize ONCE to verify the estimate.
    let mut serialized = match serde_json::to_string(&value) {
        Ok(s) => s,
        Err(_) => return text,
    };
    if serialized.len() <= TRUNCATED_FRAME_TARGET_BYTES {
        return serialized;
    }

    // Still over target -> structural case (many huge sibling strings each
    // stubbed, or giant non-string payload). Fall back to the array-shrink
    // loop, reusing the original helpers. Bounded: each round removes >= 1
    // element and there are finitely many.
    let mut rounds = 0usize;
    loop {
        rounds += 1;
        debug_assert!(
            rounds <= 64,
            "preview_oversized_frame structural fallback exceeded round bound"
        );
        if rounds > 64 {
            return serialized;
        }
        if !shrink_largest_array(&mut value) {
            return serde_json::to_string(&value).unwrap_or(text);
        }
        serialized = match serde_json::to_string(&value) {
            Ok(s) => s,
            Err(_) => return text,
        };
        if serialized.len() <= TRUNCATED_FRAME_TARGET_BYTES {
            return serialized;
        }
    }
}

/// Single-walk collection of every truncatable string field: same
/// eligibility rules as `collect_truncatable_strings` (large enough to be
/// worth truncating, not already carrying the full truncation-marker
/// sentinel) but gathers ALL candidates (path, escaped len, raw len) in
/// one pass instead of re-walking per truncation round.
fn collect_truncatable_strings(
    value: &Value,
    path: &mut Vec<PathSeg>,
    out: &mut Vec<(Vec<PathSeg>, usize, usize)>,
) {
    match value {
        Value::String(s) => {
            let escaped = json_escaped_len_bytes(s.as_bytes());
            if escaped > MARKER_ESCAPED_RESERVE_BYTES && !contains_full_truncation_marker(s) {
                out.push((path.clone(), escaped, s.len()));
            }
        }
        Value::Array(items) => {
            for (idx, item) in items.iter().enumerate() {
                path.push(PathSeg::Index(idx));
                collect_truncatable_strings(item, path, out);
                path.pop();
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                path.push(PathSeg::Key(key.clone()));
                collect_truncatable_strings(item, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Drop elements from the LARGEST JSON array in `value` to shrink the frame,
/// keeping the result valid JSON. Returns `true` if it removed at least one
/// element (so the caller should re-measure and keep iterating); `false` if
/// there is no array with > 1 element left to shrink.
///
/// We remove from the MIDDLE outward (keeping the head and tail of the list, so
/// a history/timeline still shows its start and end), and we never inject a
/// heterogeneous marker element — for an array of objects that would break a
/// consumer's element schema. The dropped count is implicit (consumers see a
/// shorter list); validity is preserved.
fn shrink_largest_array(value: &mut Value) -> bool {
    // Locate the path of the largest (by element count) array with > 1 element.
    let mut best: Option<(Vec<PathSeg>, usize)> = None;
    let mut path: Vec<PathSeg> = Vec::new();
    walk_for_largest_array(value, &mut path, &mut best);
    let Some((array_path, len)) = best else {
        return false;
    };
    if len <= 1 {
        return false;
    }
    let Some(Value::Array(items)) = field_at_path_mut(value, &array_path) else {
        return false;
    };
    // Remove ~half of the elements from the middle, but always at least one and
    // at least enough to make progress on a very large array. Keep the head and
    // tail so the start/end of the list survive.
    let drop_count = (items.len() / 2).max(1);
    let keep = items.len() - drop_count;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let total = items.len();
    // Retain the first `head` and last `tail` elements; drop the middle.
    let mut idx = 0usize;
    items.retain(|_| {
        let keep_this = idx < head || idx >= total - tail;
        idx += 1;
        keep_this
    });
    true
}

/// Walk the tree recording the path of the array with the most elements.
fn walk_for_largest_array(
    value: &Value,
    path: &mut Vec<PathSeg>,
    best: &mut Option<(Vec<PathSeg>, usize)>,
) {
    match value {
        Value::Array(items) => {
            let is_better = match best {
                Some((_, best_len)) => items.len() > *best_len,
                None => true,
            };
            if is_better {
                *best = Some((path.clone(), items.len()));
            }
            for (idx, item) in items.iter().enumerate() {
                path.push(PathSeg::Index(idx));
                walk_for_largest_array(item, path, best);
                path.pop();
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                path.push(PathSeg::Key(key.clone()));
                walk_for_largest_array(item, path, best);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Resolve a path to a `&mut Value` (for shrinking an array in place).
fn field_at_path_mut<'a>(value: &'a mut Value, path: &[PathSeg]) -> Option<&'a mut Value> {
    let mut current = value;
    for seg in path {
        current = match (current, seg) {
            (Value::Object(map), PathSeg::Key(key)) => map.get_mut(key)?,
            (Value::Array(items), PathSeg::Index(idx)) => items.get_mut(*idx)?,
            _ => return None,
        };
    }
    Some(current)
}

/// Target serialized ceiling for a rewritten frame. Held well under the
/// 1 MiB [`MAX_TEXT_FRAME_BYTES`] cap so the marker, JSON escaping, and the
/// RPC envelope can never push the deliverable frame back over the hard cap.
/// 900 KiB leaves > 124 KiB of headroom.
const TRUNCATED_FRAME_TARGET_BYTES: usize = 900 * 1024;

/// Reserved ESCAPED-length headroom for the truncation marker inserted
/// between head and tail. The marker is `\n…… [<N> bytes truncated] ……\n`:
/// two `\n` (escape to 2 bytes each = 4), four `…` (each a 3-byte UTF-8 char
/// that escapes 1:1 = 12), the fixed ` [ bytes truncated] ` scaffold
/// (~22 bytes, all 1:1), and N rendered as decimal digits (1:1). N is a byte
/// count of a single frame field, so it is at most ~20 digits. 128 bytes is
/// comfortably conservative; the head/tail budget reserves it so the marker
/// can never push the field past its escaped budget.
const MARKER_ESCAPED_RESERVE_BYTES: usize = 128;

/// Minimum escaped budget below which a field cannot hold even a marker +
/// a few bytes of head/tail; such a field is replaced with this stub so the
/// iteration can continue reducing other fields. Rare (pathological envelope).
const UNPREVIEWABLE_STUB: &str = "[oversized field omitted]";

/// Per-byte JSON-escaped length of a UTF-8 byte buffer, EXCLUDING the
/// surrounding quotes. Matches `serde_json`'s default string serialization
/// (mirrors `octos_pipeline::fidelity`'s private byte-class helper; replicated
/// here rather than taking a dependency edge on a private pipeline internal):
/// * `"`, `\`, `\n`, `\r`, `\t`, backspace (0x08), form feed (0x0c) -> 2 bytes;
/// * other C0 control bytes (no short escape) -> `\u00XX` = 6 bytes;
/// * everything else (incl. multi-byte UTF-8 lead/continuation bytes) -> 1.
///
/// Operating on raw bytes lets us probe arbitrary offsets without panicking on
/// a non-`&str`-boundary slice; the per-byte cost equals the char-wise escape
/// because every UTF-8 continuation/lead byte is in the 1:1 arm.
fn json_escaped_len_bytes(bytes: &[u8]) -> usize {
    let mut len = 0usize;
    for &b in bytes {
        len += match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            0x00..=0x1f => 6,
            _ => 1,
        };
    }
    len
}

/// JSON-escaped length of a single `char`, using the same byte-class rules as
/// [`json_escaped_len_bytes`]. Any non-ASCII scalar (>= U+0080) escapes 1:1 to
/// its UTF-8 byte length; only the ASCII range can incur a short escape (2) or
/// a `\u00XX` control escape (6).
fn json_escaped_len_char(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Number of leading bytes of `s` whose JSON-escaped length fits within
/// `escaped_budget`, stopped at a UTF-8 char boundary so a codepoint is never
/// split. Walks chars from the front, accumulating each char's escaped cost.
fn head_bytes_within_escaped_budget(s: &str, escaped_budget: usize) -> usize {
    let mut used_escaped = 0usize;
    let mut raw_end = 0usize;
    for (idx, ch) in s.char_indices() {
        let cost = json_escaped_len_char(ch);
        if used_escaped + cost > escaped_budget {
            break;
        }
        used_escaped += cost;
        raw_end = idx + ch.len_utf8();
    }
    raw_end
}

/// Number of trailing bytes of `s` whose JSON-escaped length fits within
/// `escaped_budget`, stopped at a UTF-8 char boundary. Walks chars from the
/// back. Returns the raw byte length of the kept suffix.
fn tail_bytes_within_escaped_budget(s: &str, escaped_budget: usize) -> usize {
    let mut used_escaped = 0usize;
    let mut raw_start = s.len();
    for (idx, ch) in s.char_indices().rev() {
        let cost = json_escaped_len_char(ch);
        if used_escaped + cost > escaped_budget {
            break;
        }
        used_escaped += cost;
        raw_start = idx;
    }
    s.len() - raw_start
}

/// Build a head+tail preview string for `field` whose JSON-ESCAPED length is
/// `<= field_escaped_budget`. Keeps the first H and last T bytes (UTF-8 safe),
/// dropping the middle, with `\n…… [<N> bytes truncated] ……\n` between them
/// (N = `original_raw_len` minus kept head+tail bytes). Returns `None` when
/// the budget cannot even hold the marker plus a non-empty head, so the caller
/// can fall back. Head-leaning 50/50 split of the post-marker budget.
fn build_head_tail_preview(
    field: &str,
    original_raw_len: usize,
    field_escaped_budget: usize,
) -> Option<String> {
    // Reserve room for the marker; the rest is head + tail.
    let content_escaped_budget = field_escaped_budget.checked_sub(MARKER_ESCAPED_RESERVE_BYTES)?;
    if content_escaped_budget == 0 {
        return None;
    }
    // Head-leaning split: head gets the ceil-half, tail the floor-half.
    let head_escaped_budget = content_escaped_budget.div_ceil(2);
    let tail_escaped_budget = content_escaped_budget - head_escaped_budget;

    let head_len = head_bytes_within_escaped_budget(field, head_escaped_budget);
    // The tail must not overlap the head.
    let remaining = &field[head_len..];
    let tail_len = tail_bytes_within_escaped_budget(remaining, tail_escaped_budget);

    let head = &field[..head_len];
    let tail = &field[field.len() - tail_len..];
    let dropped = original_raw_len.saturating_sub(head_len + tail_len);

    // If we kept the entire field (nothing dropped) the preview would not be a
    // truncation at all; that only happens when the field already fit the
    // budget, which the caller's serialized-size loop would not have reached.
    Some(format!("{head}\n…… [{dropped} bytes truncated] ……\n{tail}"))
}

/// A path into a JSON value: object keys and array indices.
#[derive(Clone, PartialEq, Eq, Hash)]
enum PathSeg {
    Key(String),
    Index(usize),
}

/// True iff `s` already contains the EXACT full head+tail truncation-marker
/// scaffold produced by [`build_head_tail_preview`]:
/// `\n…… [<N> bytes truncated] ……\n` (N a decimal byte count). Used as a
/// secondary "already previewed" guard in [`collect_truncatable_strings`] that is
/// robust to array-shrink index drift (path-set staleness).
///
/// This matches the COMPLETE scaffold — the leading `\n…… [` prefix and the
/// `] ……\n` suffix with a parseable decimal between — NOT the bare phrase
/// `bytes truncated`. A user payload that merely contains the phrase (or even
/// `[123 bytes truncated]` without the `……` glyph scaffold) is therefore NOT
/// matched and is still eligible for truncation, preserving the
/// `payload_containing_marker_phrase_is_still_truncated` guarantee.
fn contains_full_truncation_marker(s: &str) -> bool {
    // Anchor on the marker prefix, then verify the suffix scaffold follows with
    // only decimal digits in between (the `<N> bytes truncated` slot).
    let mut search_from = 0;
    while let Some(rel) = s[search_from..].find("\n…… [") {
        let after_prefix = search_from + rel + "\n…… [".len();
        if let Some(suffix_rel) = s[after_prefix..].find(" bytes truncated] ……\n") {
            let digits = &s[after_prefix..after_prefix + suffix_rel];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
        // Advance past this prefix occurrence and keep scanning.
        search_from = after_prefix;
    }
    false
}

/// Resolve a path to a `&Value` (for reading the current field text).
fn field_at_path<'a>(value: &'a Value, path: &[PathSeg]) -> Option<&'a Value> {
    let mut current = value;
    for seg in path {
        current = match (current, seg) {
            (Value::Object(map), PathSeg::Key(key)) => map.get(key)?,
            (Value::Array(items), PathSeg::Index(idx)) => items.get(*idx)?,
            _ => return None,
        };
    }
    Some(current)
}

/// Replace the value at `path` with `new_value`. Returns false if the path no
/// longer resolves.
fn set_field_at_path(value: &mut Value, path: &[PathSeg], new_value: Value) -> bool {
    let mut current = value;
    for (i, seg) in path.iter().enumerate() {
        let is_last = i + 1 == path.len();
        match (current, seg) {
            (Value::Object(map), PathSeg::Key(key)) => {
                let Some(slot) = map.get_mut(key) else {
                    return false;
                };
                if is_last {
                    *slot = new_value;
                    return true;
                }
                current = slot;
            }
            (Value::Array(items), PathSeg::Index(idx)) => {
                let Some(slot) = items.get_mut(*idx) else {
                    return false;
                };
                if is_last {
                    *slot = new_value;
                    return true;
                }
                current = slot;
            }
            _ => return false,
        }
    }
    false
}

fn appui_evidence_dir() -> Option<PathBuf> {
    std::env::var_os("OCTOSCODE_M15_UX_OUTPUT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

static APPUI_EVIDENCE_JSONL_APPEND_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn append_appui_evidence_jsonl(name: &str, value: Value) {
    let Some(dir) = appui_evidence_dir() else {
        return;
    };
    append_appui_evidence_jsonl_at(&dir, name, value);
}

fn append_appui_evidence_jsonl_at(dir: &Path, name: &str, value: Value) {
    if let Err(error) = std::fs::create_dir_all(dir) {
        tracing::debug!(%error, "failed to create AppUI evidence directory");
        return;
    }
    let path = dir.join(name);
    let line = match serde_json::to_string(&value) {
        Ok(line) => line,
        Err(error) => {
            tracing::debug!(%error, "failed to serialize AppUI evidence");
            return;
        }
    };
    let mut line = line.into_bytes();
    line.push(b'\n');
    let _append_guard = APPUI_EVIDENCE_JSONL_APPEND_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            tracing::debug!(%error, "failed to create AppUI evidence parent directory");
            return;
        }
    }
    if let Err(error) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(&line)
        })
    {
        tracing::debug!(%error, "failed to append AppUI evidence");
    }
}

fn append_appui_transcript_frame(direction: &str, frame: Value) {
    append_appui_evidence_jsonl(
        "appui-transcript.jsonl",
        json!({
            "ts": Utc::now().to_rfc3339(),
            "direction": direction,
            "frame": frame,
        }),
    );
}

/// task-return-unconsumed-steer-inputs: where a `turn/steer_dropped` return
/// goes. The live connection sends durably AND ledgers; a closed connection
/// (see `abort_connection_turns`) can only ledger — the reconnecting client
/// replays it, still ahead of the terminal.
pub(crate) enum SteerReturnSink<'a> {
    Live {
        ws: &'a WsConnection,
        ledger: &'a UiProtocolLedger,
    },
    LedgerOnly {
        ledger: &'a UiProtocolLedger,
    },
}

/// task-return-unconsumed-steer-inputs: drain whatever `turn/steer` inputs the
/// server accepted (`steered:true`) but never fed to the loop, and return
/// them to the client as ONE `turn/steer_dropped` notification (buffer
/// order preserved). Sent durable — the payload is user text, so it must not
/// be lost to backpressure — and appended to the ledger so a reconnecting
/// client replays it. Emits nothing for an empty buffer. Returns the number
/// of inputs returned.
///
/// ORDER CONTRACT (`event.turn_steer_dropped.v1`): this runs after the turn
/// state flipped to Terminal (no more steers can be accepted) and BEFORE the
/// terminal frame — see `transition_to_terminal_settling_steers`, the single
/// gate every terminal outlet goes through. A client that sees the terminal
/// without a preceding `turn/steer_dropped` naming its steer may conclude the
/// steer was consumed.
pub(crate) fn settle_leftover_steers(
    buffer: &octos_agent::SharedSteerBuffer,
    sink: SteerReturnSink<'_>,
    session_id: &SessionKey,
    turn_id: &TurnId,
    interrupt_observed: bool,
) -> usize {
    let leftovers = buffer.drain();
    let count = leftovers.len();
    let Some(notification) = crate::steer_return::leftover_steer_notification(
        session_id,
        turn_id,
        leftovers,
        interrupt_observed,
    ) else {
        return 0;
    };
    warn!(
        session = %session_id.0,
        turn = %turn_id.0,
        dropped = count,
        interrupted = interrupt_observed,
        "turn/steer input(s) were still pending when the turn ended; returning them \
         to the client as turn/steer_dropped ahead of the terminal"
    );
    match sink {
        SteerReturnSink::Live { ws, ledger } => {
            let _ = send_notification_durable(ws, ledger, notification);
        }
        SteerReturnSink::LedgerOnly { ledger } => {
            let _ = ledger.append_notification(notification);
        }
    }
    count
}

/// task-return-unconsumed-steer-inputs: THE terminal gate. Every terminal
/// outlet (live `try_emit_terminal`, the connection-close abort path, the
/// early `turn/started`-failed bail-outs) flips the state through here so the
/// `event.turn_steer_dropped.v1` order — state Terminal → steers settled →
/// terminal frame — cannot be bypassed. Returns the transition for the caller
/// to emit its terminal frame, or `None` if the turn was already terminal.
async fn transition_to_terminal_settling_steers(
    turn_state: &TokioMutex<TurnState>,
    expected_reason: TerminalReason,
    steer_buffer: Option<&octos_agent::SharedSteerBuffer>,
    sink: SteerReturnSink<'_>,
    session_id: &SessionKey,
    turn_id: &TurnId,
) -> Option<TerminalTransition> {
    let transition = transition_to_terminal(turn_state, expected_reason).await?;
    if let Some(buffer) = steer_buffer {
        settle_leftover_steers(
            buffer,
            sink,
            session_id,
            turn_id,
            matches!(transition.reason, TerminalReason::Interrupted),
        );
    }
    Some(transition)
}

fn send_turn_error(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    turn_id: &TurnId,
    code: impl Into<String>,
    message: impl Into<String>,
) -> Result<(), SendError> {
    send_turn_error_with_details(ws, ledger, session_id, turn_id, code, message, None)
}

fn send_turn_error_with_details(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    turn_id: &TurnId,
    code: impl Into<String>,
    message: impl Into<String>,
    details: Option<TurnCompletionDetails>,
) -> Result<(), SendError> {
    let details = details.unwrap_or_default();
    send_notification_lifecycle(
        ws,
        ledger,
        UiNotification::TurnError(TurnErrorEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            code: code.into(),
            message: message.into(),
            token_usage: details.token_usage,
            partial_result: details.partial_result,
        }),
    )
}

/// Short STATIC message used for the minimal same-id error reply emitted when a
/// request RESPONSE (result or error) is too large to deliver even after the
/// oversized-frame preview pass. It carries NO echoed payload — only enough for
/// the client to fail the awaiting request id cleanly.
const RPC_RESPONSE_TOO_LARGE_MESSAGE: &str = "response payload too large to deliver";

/// Build a MINIMAL JSON-RPC error response frame for `id` carrying only a short
/// static `message` (code `INTERNAL_ERROR` / -32603, NO `data`). It is tiny by
/// construction, so it fits under [`MAX_TEXT_FRAME_BYTES`] for every realistic
/// `id`. Returns `None` ONLY if even this minimal envelope is over cap — which
/// can happen only when the request `id` itself is pathologically huge (inbound
/// parsing caps the whole request frame at 1 MiB but NOT the id alone, so a
/// near-cap string id is acceptable). Callers treat that residual `None` as the
/// absolute last resort (latch the connection failed / close).
///
/// CRITICAL (codex DO-NOT-SHIP): this MUST serialize and size-check the envelope
/// DIRECTLY — it must NOT route through [`frame_for`]/[`preview_oversized_frame`].
/// The preview pass truncates the largest string field; for a near-cap `id` that
/// is the `id` itself, which would emit a valid under-cap error under a MODIFIED
/// id the client never awaited, leaving the original request stranded. The `id`
/// must be preserved EXACTLY: either the exact-id envelope already fits and we
/// send it, or it does not and we return `None` (caller fail-closes) — never a
/// wrong-id reply.
fn minimal_rpc_error_frame(id: Option<String>, message: &'static str) -> Option<WsMessage> {
    let error = RpcError::new(
        octos_core::ui_protocol::rpc_error_codes::INTERNAL_ERROR,
        message,
    );
    match app_ui_codec::to_compact_json(&RpcErrorResponse::new(id, error)) {
        Ok(text) if text.len() <= MAX_TEXT_FRAME_BYTES => Some(WsMessage::text(text)),
        // Only reachable when the request `id` itself is pathologically huge.
        // Do NOT truncate to fit — that would corrupt the id. Return `None` so
        // the caller fail-closes rather than sending a wrong-id reply.
        Ok(_) => None,
        Err(error) => {
            metrics::counter!("ws.send.error.lifecycle").increment(1);
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                error = %error,
                "failed to serialize minimal rpc error frame"
            );
            None
        }
    }
}

/// Send a JSON-RPC RESPONSE that, when over-cap and un-truncatable, falls back to
/// a minimal same-id error instead of stranding the client request.
///
/// DO-NOT-SHIP fix: `frame_for` returns `None` when the primary frame is still
/// over [`MAX_TEXT_FRAME_BYTES`] after the oversized-frame preview pass
/// (pathological / structural). For a request RESPONSE the request handlers
/// IGNORE the returned `Result` (the call sites `let _ =` it), so a silent
/// `None` would leave the client waiting forever on its request id with NO reply
/// and NO connection close. Instead we synthesize a guaranteed-tiny same-id
/// JSON-RPC error reply (no echoed large payload) and send THAT, so the client
/// always gets a reply to its id. The over-cap `result` is NOT echoed.
fn send_rpc_result(ws: &WsConnection, id: String, result: Value) -> Result<(), SendError> {
    match frame_for(&RpcResponse::success(id.clone(), result)) {
        Some(frame) => ws.send_lifecycle(frame),
        None => send_minimal_rpc_error_fallback(ws, Some(id)),
    }
}

fn send_ui_rpc_result(ws: &WsConnection, id: String, result: UiRpcResult) -> Result<(), SendError> {
    let value = result
        .into_result_value()
        .map_err(|_| SendError::LifecycleFailure("typed rpc result serialization".into()))?;
    send_rpc_result(ws, id, value)
}

/// Send a JSON-RPC ERROR response, falling back to a minimal same-id error when
/// the (possibly large) error payload is itself over-cap and un-deliverable.
///
/// DO-NOT-SHIP fix: same rationale as [`send_rpc_result`]. If `error.data` /
/// `error.message` is so large that `frame_for` returns `None`, we drop the
/// oversized data and emit a minimal same-id error with a short static message,
/// so the client still gets a reply to its id rather than waiting forever.
fn send_rpc_error(ws: &WsConnection, id: Option<String>, error: RpcError) -> Result<(), SendError> {
    match frame_for(&RpcErrorResponse::new(id.clone(), error)) {
        Some(frame) => ws.send_lifecycle(frame),
        None => send_minimal_rpc_error_fallback(ws, id),
    }
}

/// Emit the minimal same-id error reply for an over-cap RPC RESPONSE. Distinct
/// counter + warn (vs. the notification-drop `over_cap_dropped` path) so the
/// minimized-RPC path is observable. Absolute last resort: if even the minimal
/// same-id error frame is over cap (only reachable when the request `id` is
/// pathologically huge — practically unreachable), latch the connection failed
/// and surface `LifecycleFailure` so the read loop tears down (rather than
/// looping or panicking).
fn send_minimal_rpc_error_fallback(ws: &WsConnection, id: Option<String>) -> Result<(), SendError> {
    metrics::counter!("ws.send.rpc.over_cap_minimized").increment(1);
    tracing::warn!(
        target: "octos::ui_protocol::ws",
        has_id = id.is_some(),
        "rpc response over cap after preview; replacing with minimal same-id error reply"
    );
    match minimal_rpc_error_frame(id, RPC_RESPONSE_TOO_LARGE_MESSAGE) {
        Some(frame) => ws.send_lifecycle(frame),
        None => {
            // Unreachable in practice: the only way a minimal error envelope
            // (jsonrpc + short static message + id) exceeds the cap is a
            // pathologically huge request `id`. Latch the connection failed and
            // close it — do NOT loop or panic.
            metrics::counter!("ws.send.rpc.minimal_error_over_cap").increment(1);
            tracing::error!(
                target: "octos::ui_protocol::ws",
                "minimal rpc error frame itself over cap (huge request id?); closing connection"
            );
            // Enqueue the close BEFORE latching failed: `send_lifecycle` is
            // rejected (FatalClosed) once `mark_failed` sets the latch, which
            // would silently drop the 1011 close frame. Close first so the
            // client actually receives the code, THEN latch failed to tear down
            // the read loop.
            let _ = close_ws_with_code(ws, 1011, "rpc_response_undeliverable");
            ws.mark_failed();
            Err(SendError::LifecycleFailure(
                "minimal rpc error frame over cap".into(),
            ))
        }
    }
}

/// Push a WebSocket close frame with an explicit status code and reason. The
/// `writer_loop` forwards the close to the peer and then drains; callers
/// should `return` immediately after this call.
///
/// Used to signal durable auth failure (code 1008 / "auth_expired"). The SPA
/// bridge (Web PR #114, `auth-context.tsx`) subscribes to close-code 1008 to
/// invoke `crew:auth_expired`, which clears the cached token and routes to
/// /login. Callers MUST enqueue this close frame BEFORE any accompanying
/// `RpcError` envelope: under writer-channel backpressure (capacity-1 with one
/// slot used), only the first try_send survives, and the close is the
/// load-bearing signal the SPA listens for (codex BLOCK 2026-05-13).
fn close_ws_with_code(ws: &WsConnection, code: u16, reason: &str) -> Result<(), SendError> {
    let frame = WsMessage::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }));
    ws.send_lifecycle(frame)
}

/// Send a scope-validation error back to the caller and, when the error came
/// from `validate_authenticated_session_scope` (i.e. the connection IS
/// authenticated and the requested scope doesn't match), accompany it with a
/// close-code 1008 frame so the SPA `crew:auth_expired` listener fires.
/// Non-auth scope errors (malformed input, etc.) leave the socket open, and
/// stdio connections never get the close: they carry no auth identity, and a
/// Close frame would end the stdio writer loop (#2040).
fn send_scope_error(ws: &WsConnection, id: String, error: RpcError) {
    let auth_violation = is_auth_scope_violation(&error);
    // Codex BLOCK (2026-05-13): when the writer channel has just one free
    // slot, the close-code is the load-bearing signal — the SPA uses it to
    // detect auth-expiry and clear its token. Enqueue the close FIRST so it
    // survives backpressure even if the courtesy error envelope is dropped.
    //
    // #2040: the close is a WebSocket-only signal. A stdio connection has no
    // auth identity (its scope comes from the session/open candidate, not a
    // token), and a Close frame ends the stdio writer loop — dropping the
    // error envelope queued behind it and tearing down the transport with
    // the request unanswered. Stdio gets the error reply only.
    if auth_violation && !ws.is_stdio() {
        let _ = close_ws_with_code(ws, 1008, "auth_expired");
    }
    let _ = send_rpc_error(ws, Some(id), error);
}

/// Codex #1336 round-2 BLOCKER 1: per-connection capability filter
/// applied at the DIRECT-SEND boundary. Mirrors what
/// [`live_event_passes_capability_filter`] does for the broadcast
/// forwarder so the per-connection mutual exclusion contract holds
/// for the originating connection too — not just for other
/// connections receiving the same event via fan-out.
///
/// Returns `true` when the wire frame should be sent on this
/// connection; `false` when the connection's negotiated feature set
/// filters it out (e.g. a `projection.envelope.v1` client that must
/// not receive a legacy `MessageDelta` direct-send).
fn direct_send_passes_capability_filter(ws: &WsConnection, event: &UiProtocolLedgerEvent) -> bool {
    let features = ws.snapshot_live_features();
    live_event_passes_capability_filter(event, features)
}

fn send_notification_lifecycle(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    notification: UiNotification,
) -> Result<(), SendError> {
    let features = ws.snapshot_live_features();
    if features.projection_envelope_v2
        && matches!(
            &notification,
            UiNotification::TurnCompleted(_) | UiNotification::TurnError(_)
        )
    {
        // A canonical v2 persisted row is produced by the commit observer and
        // reaches this connection through the ordered ledger forwarder. If we
        // direct-send the terminal here, it can overtake that forwarder even
        // though the persisted row has the smaller durable cursor. The client
        // then finalizes an empty/partial turn and rejects the late canonical
        // row as post-terminal — the real first-turn soak failure.
        //
        // Put v2 terminals onto the same broadcast lane instead. Appending
        // without the originating-connection suppression tag makes this
        // connection's forwarder deliver it after every earlier ledger row.
        // The v2 stream is cursor-replayable, so a writer failure is recovered
        // by session hydration rather than by letting a lifecycle-priority
        // frame violate the projection's ordering contract. Legacy/v1 clients
        // retain the direct lifecycle path below unchanged.
        ledger.append_notification(notification);
        return Ok(());
    }

    // Tag the broadcast with the originating connection so this
    // connection's own live forwarder skips the duplicate copy.
    let event = ledger.append_notification_from(notification, ws.connection_id);
    let cursor = event.cursor.clone();
    // Codex #1336 round-2 BLOCKER 1: apply the per-connection
    // capability filter at the direct-send boundary. Without this, a
    // connection that negotiated `projection.envelope.v1` would still
    // receive direct-sent legacy `TurnCompleted` lifecycle frames,
    // violating the γ cutover gate's mutual exclusion contract. The
    // ledger append above still happens so the canonical envelope
    // emit (via `ledger.emit_envelope` on the same handler path)
    // delivers via the broadcast forwarder.
    let projected = features
        .projection_envelope_v2
        .then(|| project_v2_ledger_event(ledger, &event.event, &event.cursor))
        .flatten();
    let event_for_wire = context_event_for_features(projected.unwrap_or(event.event), features);
    let delivery_metric = ui_protocol_delivery_metric(&event_for_wire);
    let method = ledger_event_method(&event_for_wire).to_string();
    if !live_event_passes_capability_filter(&event_for_wire, features) {
        return Ok(());
    }
    let frame = frame_from_ledger(event_for_wire)
        .ok_or_else(|| SendError::LifecycleFailure(format!("serialize {method}")))?;
    match ws.send_lifecycle(frame) {
        Ok(()) => {
            record_ui_protocol_delivery_metric(delivery_metric);
            ws.metrics.record_durable_cursor(&cursor);
            Ok(())
        }
        Err(SendError::LifecycleFailure(reason)) => {
            // The ledger entry stays — the spec calls this `delivery_failed`
            // from the caller's perspective (turn aborts cleanly).
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                method = %method,
                reason = %reason,
                "lifecycle notification not delivered; entry remains in ledger as delivery_failed"
            );
            Err(SendError::LifecycleFailure(reason))
        }
        Err(other) => Err(other),
    }
}

fn send_notification_durable(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    notification: UiNotification,
) -> Result<(), SendError> {
    let event = ledger.append_notification_from(notification, ws.connection_id);
    let cursor = event.cursor.clone();
    // Codex #1336 round-2 BLOCKER 1: apply the per-connection
    // capability filter at the direct-send boundary. A connection
    // that negotiated `projection.envelope.v1` must not receive
    // direct-sent legacy notifications (for example,
    // `ToolStarted` / `ToolProgress` / `ToolCompleted`,
    // `FileAttached`, etc.) — the canonical envelopes emitted by
    // `ledger.emit_envelope` reach the same connection via the live
    // forwarder. Ledger append still occurs above so OTHER
    // connections (without the feature) receive the legacy shape via
    // their own forwarders.
    let features = ws.snapshot_live_features();
    let projected = features
        .projection_envelope_v2
        .then(|| project_v2_ledger_event(ledger, &event.event, &event.cursor))
        .flatten();
    let event_for_wire = context_event_for_features(projected.unwrap_or(event.event), features);
    let delivery_metric = ui_protocol_delivery_metric(&event_for_wire);
    let method = ledger_event_method(&event_for_wire).to_string();
    if !live_event_passes_capability_filter(&event_for_wire, features) {
        return Ok(());
    }
    let frame = match frame_from_ledger(event_for_wire) {
        Some(frame) => frame,
        None => {
            return Err(SendError::BackpressureDrop);
        }
    };
    match ws.send_durable(frame, &method) {
        Ok(()) => {
            record_ui_protocol_delivery_metric(delivery_metric);
            ws.metrics.record_durable_cursor(&cursor);
            Ok(())
        }
        Err(SendError::BackpressureDrop) => {
            // Best-effort: try to tell the client right away. If even the
            // lossy frame cannot enqueue, accumulate and flush later.
            emit_replay_lossy_opportunistic(ws, ledger, &cursor.stream);
            Err(SendError::BackpressureDrop)
        }
        Err(other) => Err(other),
    }
}

fn send_notification_ephemeral(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    notification: UiNotification,
) -> Result<(), SendError> {
    // Ephemeral frames are NOT appended to the ledger — they are explicitly
    // non-durable per spec § 9. Drops never need a `replay_lossy` summary.
    // Every legacy `message/delta` send funnels through here exactly once
    // (the producing turn task's direct send) — feed the `session/btw`
    // live-draft tail BEFORE the capability filter so envelope-negotiated
    // connections still record it. TurnId keying isolates streams.
    if let UiNotification::MessageDelta(delta) = &notification {
        btw_live_draft_append(&delta.session_id, &delta.turn_id, &delta.text);
    }
    let method = notification.method().to_string();
    // Codex #1336 round-2 BLOCKER 1: apply the per-connection
    // capability filter to ephemeral direct sends too. `MessageDelta`
    // is the load-bearing case — every keystroke from the LLM is an
    // ephemeral direct send, and a `projection.envelope.v1` client
    // expects the canonical envelope shape exclusively.
    //
    // Ephemerals are NOT in the ledger so we wrap the notification in
    // a synthetic `Notification` ledger event for the filter check
    // only (it never reaches disk). This keeps the filter helper
    // signature uniform across direct-send paths.
    let filter_event = UiProtocolLedgerEvent::Notification(notification.clone());
    let delivery_metric = ui_protocol_delivery_metric(&filter_event);
    if !direct_send_passes_capability_filter(ws, &filter_event) {
        return Ok(());
    }
    let rpc = match notification.into_rpc_notification() {
        Ok(rpc) => rpc,
        Err(error) => {
            tracing::debug!(
                target: "octos::ui_protocol::ws",
                method = %method,
                error = %error,
                "failed to serialize ephemeral notification"
            );
            return Err(SendError::BackpressureDrop);
        }
    };
    let frame = frame_for(&rpc).ok_or(SendError::BackpressureDrop)?;
    let _ = ledger; // unused for ephemeral, kept for symmetry with durable
    match ws.send_ephemeral(frame, &method) {
        Ok(()) => {
            record_ui_protocol_delivery_metric(delivery_metric);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn send_ledger_event_durable(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    event: UiProtocolLedgerEvent,
) -> Result<(), SendError> {
    let method = ledger_event_method(&event).to_string();
    let delivery_metric = ui_protocol_delivery_metric(&event);
    // `event` already carries its cursor (set by the ledger before storage)
    // — pull a copy out before consuming the event into a frame.
    let cursor = ledger_event_cursor(&event);
    // Codex #1336 round-2 BLOCKER 1: this helper is called from THREE
    // paths — (a) the live forwarder, which already filters via
    // `live_event_passes_capability_filter(features)` BEFORE invoking;
    // (b) the session/open replay loop, which ALSO already filters
    // before invoking; (c) handler-direct paths that flush a ledger
    // event they just appended (progress status, opened-event,
    // synthetic emits) — these always send shapes that aren't gated
    // by `projection_envelope` (Progress, SessionOpened, etc.).
    // Adding a second filter pass here would double up with (a) +
    // (b) — and in tests where the forwarder's `features` arg is set
    // explicitly without syncing `WsConnection::live_features`, the
    // second pass would drop events the forwarder approved. The
    // `send_notification_*` family is where γ-cutover direct sends
    // arrive and where the per-connection filter is applied.
    let frame = match frame_from_ledger(event) {
        Some(frame) => frame,
        None => return Err(SendError::BackpressureDrop),
    };
    match ws.send_durable(frame, &method) {
        Ok(()) => {
            record_ui_protocol_delivery_metric(delivery_metric);
            if let Some(cursor) = cursor {
                ws.metrics.record_durable_cursor(&cursor);
            }
            Ok(())
        }
        Err(SendError::BackpressureDrop) => {
            if let Some(cursor) = cursor.as_ref() {
                emit_replay_lossy_opportunistic(ws, ledger, &cursor.stream);
            }
            Err(SendError::BackpressureDrop)
        }
        Err(other) => Err(other),
    }
}

/// Async twin of [`send_ledger_event_durable`] for the live-forwarder task
/// (#2065): identical prep, metrics, and
/// `replay_lossy` semantics — keep the two in lockstep — but the enqueue
/// goes through [`WsConnection::send_durable_offloaded`], whose stdio lane
/// parks THIS task cooperatively (non-blocking probe + async sleep) instead
/// of a blocking `SyncSender::send` — never an executor-worker stall, and
/// cancellable with an atomic enqueue so abort+join leaves nothing
/// detached in flight.
async fn send_ledger_event_durable_offloaded(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    event: UiProtocolLedgerEvent,
) -> Result<(), SendError> {
    let method = ledger_event_method(&event).to_string();
    let delivery_metric = ui_protocol_delivery_metric(&event);
    let cursor = ledger_event_cursor(&event);
    let frame = match frame_from_ledger(event) {
        Some(frame) => frame,
        None => return Err(SendError::BackpressureDrop),
    };
    match ws.send_durable_offloaded(frame, &method).await {
        Ok(()) => {
            record_ui_protocol_delivery_metric(delivery_metric);
            if let Some(cursor) = cursor {
                ws.metrics.record_durable_cursor(&cursor);
            }
            Ok(())
        }
        Err(SendError::BackpressureDrop) => {
            if let Some(cursor) = cursor.as_ref() {
                emit_replay_lossy_opportunistic(ws, ledger, &cursor.stream);
            }
            Err(SendError::BackpressureDrop)
        }
        Err(other) => Err(other),
    }
}

fn frame_from_ledger(event: UiProtocolLedgerEvent) -> Option<WsMessage> {
    let notification = match event.into_rpc_notification() {
        Ok(rpc) => rpc,
        Err(error) => {
            tracing::warn!(
                target: "octos::ui_protocol::ws",
                error = %error,
                "ledger event failed to serialize"
            );
            return None;
        }
    };
    frame_for(&notification)
}

fn ledger_event_method(event: &UiProtocolLedgerEvent) -> &'static str {
    match event {
        UiProtocolLedgerEvent::Notification(n) => n.method(),
        UiProtocolLedgerEvent::Progress(_) => octos_core::ui_protocol::methods::PROGRESS_UPDATED,
    }
}

fn ledger_event_cursor(event: &UiProtocolLedgerEvent) -> Option<UiCursor> {
    // #924 NIT 7: exhaustive on both `UiProtocolLedgerEvent` AND the
    // inner `UiNotification`. A `_ => None` catchall would let a
    // future cursor-bearing variant compile cleanly while silently
    // being skipped for replay-lossy cursor extraction (#921 was
    // exactly that bug for cursor-bearing notification variants).
    // The rule for new variants: if you add a `cursor: UiCursor` or
    // `cursor: Option<UiCursor>` field, add it here too. Variants
    // whose "cursor" is an `OutputCursor` (task output stream) are
    // explicitly NOT surfaced here — that's a separate replay channel.
    match event {
        UiProtocolLedgerEvent::Notification(notification) => match notification {
            UiNotification::SessionOpened(SessionOpened { cursor, .. }) => cursor.clone(),
            UiNotification::TurnCompleted(TurnCompletedEvent { cursor, .. }) => cursor.clone(),
            UiNotification::TurnSpawnComplete(spawn) => Some(spawn.cursor.clone()),
            UiNotification::EnvelopeV2(envelope) => envelope.envelope.cursor.clone(),
            // Non-cursor-bearing variants — exhaustively enumerated so a
            // future addition forces an explicit decision here.
            UiNotification::TurnStarted(_)
            | UiNotification::MessageDelta(_)
            | UiNotification::ReasoningDelta(_)
            | UiNotification::ToolStarted(_)
            | UiNotification::ToolProgress(_)
            | UiNotification::ToolCompleted(_)
            | UiNotification::ApprovalRequested(_)
            | UiNotification::ApprovalAutoResolved(_)
            | UiNotification::ApprovalDecided(_)
            | UiNotification::ApprovalCancelled(_)
            // UPCR-2026-023: structured user-questions are non-cursor-bearing
            // (like approval/requested); the durable ledger cursor on the
            // surrounding LedgeredUiProtocolEvent is authoritative for replay.
            | UiNotification::UserQuestionRequested(_)
            | UiNotification::TaskUpdated(_)
            // Plan snapshots are non-cursor-bearing; the surrounding
            // LedgeredUiProtocolEvent cursor is authoritative for replay.
            | UiNotification::PlanUpdated(_)
            // TaskOutputDelta carries an `OutputCursor`, not a `UiCursor`.
            | UiNotification::TaskOutputDelta(_)
            | UiNotification::ProgressUpdated(_)
            | UiNotification::Warning(_)
            | UiNotification::TurnError(_)
            | UiNotification::TurnSteerDropped(_)
            // ReplayLossy references a `last_durable_cursor` belonging to
            // the events it summarises, not its own — surfacing it here
            // would re-loop the replay flag onto itself.
            | UiNotification::ReplayLossy(_)
            // Peer staging/closing carry no cursor (files are the durable
            // record); kept exhaustive while the variants exist.

            | UiNotification::FileAttached(_)
            // M16 context lifecycle notifications carry context generation
            // hashes, not replay cursors. The durable ledger cursor is on
            // the surrounding LedgeredUiProtocolEvent.
            | UiNotification::ContextCompactionCompleted(_)
            | UiNotification::ContextCompactionStarted(_)
            | UiNotification::ContextNormalizationReported(_)
            // Whole-job orchestration status is a stateless lifecycle push
            // (no durable cursor of its own).
            | UiNotification::SessionOrchestration(_)
            // #2019: the human sink carries an origin + text + timestamp, not
            // a replay cursor; the surrounding ledger event's cursor is what
            // a reconnecting client resumes from.
            | UiNotification::BackgroundActivity(_)
            // UPCR-2026-014 M9-γ: envelopes carry their OWN per-thread
            // `seq` allocated by `ThreadSeqAllocator`, not the per-session
            // `UiCursor` the legacy ledger replay uses. The durable
            // ledger cursor on the surrounding `LedgeredUiProtocolEvent`
            // is still authoritative for replay; envelopes don't
            // contribute their per-thread seq into the cursor stream
            // (which would mix two non-comparable scales).
            | UiNotification::Envelope(_) => None,
        },
        UiProtocolLedgerEvent::Progress(_) => None,
    }
}

/// Best-effort: append a `protocol/replay_lossy` summary to the ledger and
/// try to enqueue it. Failures here are logged and discarded — the next
/// successful send will retry via `flush_replay_lossy`.
fn emit_replay_lossy_opportunistic(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_stream: &str,
) {
    let session_id = SessionKey(session_stream.to_string());
    let dropped = ws.metrics.dropped_count.swap(0, Ordering::Relaxed);
    if dropped == 0 {
        return;
    }
    let last_cursor = ws.metrics.snapshot_last_cursor();
    let lossy = UiNotification::ReplayLossy(ReplayLossyEvent {
        session_id,
        dropped_count: dropped,
        last_durable_cursor: last_cursor,
    });
    let event = ledger.append_notification_from(lossy, ws.connection_id);
    let method = octos_core::ui_protocol::methods::REPLAY_LOSSY.to_string();
    let frame = match frame_from_ledger(event.event) {
        Some(frame) => frame,
        None => return,
    };
    if ws.try_enqueue(frame).is_err() {
        // Channel is still full or closed. Push the count back and let the
        // next successful send opportunity flush it.
        ws.metrics
            .dropped_count
            .fetch_add(dropped, Ordering::Relaxed);
        tracing::warn!(
            target: "octos::ui_protocol::ws",
            method = %method,
            "replay_lossy could not be queued; will retry on next send"
        );
    }
}

/// Drain any accumulated drops as a final `protocol/replay_lossy` before a
/// turn boundary. Intended to be called just before `turn/completed` or
/// `turn/error` so the client knows the cursor is incomplete.
fn flush_replay_lossy(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    progress_dropped: &Arc<AtomicU64>,
) {
    let progress_drops = progress_dropped.swap(0, Ordering::Relaxed);
    if progress_drops > 0 {
        ws.metrics
            .dropped_count
            .fetch_add(progress_drops, Ordering::Relaxed);
    }
    if ws.metrics.dropped_count.load(Ordering::Relaxed) == 0 {
        return;
    }
    emit_replay_lossy_opportunistic(ws, ledger, &session_id.0);
}

#[cfg(test)]
#[path = "ui_protocol_tests.rs"]
mod tests;

// ── Test-support helpers (callers live in `ui_protocol_tests.rs`) ──
