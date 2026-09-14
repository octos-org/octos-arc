//! REST + WebSocket API surface for octos.
//!
//! Feature-gated behind `api`. Start with `octos serve [--port 50080]`.
//!
//! M9-α-5/α-6 (ADR PR #830 / audit issue #845): the chat SSE transport
//! has been deleted — every chat client now talks to `/api/ui-protocol/ws`
//! exclusively. The harness/admin and swarm event surfaces still use a
//! process-wide [`EventBroadcaster`] over SSE (admin-only).

pub(crate) mod coding_tool_contract;
mod cron_panel;
mod events;
mod handlers;
pub mod metrics;
pub mod profile_scope;
mod router;
pub(crate) mod session_ingress;
mod ui_protocol_alpha2_bridge;
mod ui_protocol_alpha9_bridge;
// Relocated to crate::contracts (Phase 3 of goal-in-chat) so `octos chat
// --peers` can share the SAME process-global pending-prompt registry the WS
// path uses without the `api` feature; re-exported here so api-internal paths
// keep working unchanged.
pub(crate) use crate::contracts::approvals as ui_protocol_approvals;
pub(crate) mod ui_protocol_transport;
// Relocated to crate::approvals_audit (Phase 4, ROBRIX-PHASE4 ADR) so the
// gateway approval path can write the same audit log without the `api`
// feature; re-exported here so api-internal paths keep working.
pub(crate) use crate::approvals_audit as ui_protocol_audit;
mod ui_protocol_ledger;
pub(crate) mod ui_protocol_progress;
mod ui_protocol_reasoning_effort;
pub(crate) use crate::contracts::sanitize as ui_protocol_sanitize;
pub(crate) use crate::contracts::scope as ui_protocol_scope;
mod ui_protocol_task_output;
pub mod ws_slash;

pub use metrics::init_metrics;
pub(crate) use router::resolve_appui_allowed_origins;
pub use router::{build_router, default_cors_allowlist};

/// Test-only re-exports for the build_output_dir validation suite.
/// Not part of the public API — used by
/// `crates/octos-cli/tests/build_output_dir_validation.rs` to assert
/// the handler-layer HTTP status mapping without spinning up the
/// full Axum router. Codex round-2 follow-up to issue #996.
#[doc(hidden)]
// #995 follow-up round 3 — Integration tests in
// `crates/octos-cli/tests/x_profile_id_strip.rs` need to drive
// `handlers::session_messages` directly: the REST route
// `GET /api/sessions/{id}/messages` was retired in M12 Phase D-5, so
// there's no `build_router` path to hit the bypass shape codex flagged.
// The function (already `pub`) and its query params type are exposed
// here for that purpose, plus `AuthIdentity` so tests can construct
// non-admin and admin identities directly without booting a real auth
// middleware stack.
//
// Issue #999 — `session_files` and `session_workspace_contract` are
// exposed via the same harness for the same reason: the legacy REST
// routes `GET /api/sessions/{id}/files` and
// `GET /api/sessions/{id}/workspace-contract` were retired, so the
// only way to exercise the gateway-mode tenant-leak bypass shape end
// -to-end is to call the WS handlers directly.
#[doc(hidden)]
pub use handlers::{
    PaginationParams as TestSessionMessagesPaginationParams, session_files as test_session_files,
    session_messages as test_session_messages,
    session_workspace_contract as test_session_workspace_contract,
};
#[doc(hidden)]
pub use router::AuthIdentity as TestAuthIdentity;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::profiles::ProfileStore;
use crate::runtime::{ProfileRuntime, SessionRuntimeCache};

/// Serializes skill filesystem mutation and runtime publication per profile.
///
/// Entries are removed when the final holder exits, so profiles that are no
/// longer mutated do not accumulate lock records for the life of the server.
pub struct ProfileSkillMutationLocks {
    entries: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ProfileSkillMutationLocks {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub async fn lock(self: &Arc<Self>, profile_id: &str) -> ProfileSkillMutationGuard {
        let lock = {
            let mut entries = self.entries.lock().unwrap();
            entries
                .entry(profile_id.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let guard = lock.clone().lock_owned().await;
        ProfileSkillMutationGuard {
            locks: Arc::clone(self),
            profile_id: profile_id.to_owned(),
            lock,
            guard: Some(guard),
        }
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    #[cfg(test)]
    fn lock_handle_count(&self, profile_id: &str) -> usize {
        self.entries
            .lock()
            .unwrap()
            .get(profile_id)
            .map(Arc::strong_count)
            .unwrap_or_default()
    }
}

impl Default for ProfileSkillMutationLocks {
    fn default() -> Self {
        Self::new()
    }
}

/// Owned async guard that prunes its profile lock once no waiter remains.
pub struct ProfileSkillMutationGuard {
    locks: Arc<ProfileSkillMutationLocks>,
    profile_id: String,
    lock: Arc<tokio::sync::Mutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for ProfileSkillMutationGuard {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut entries = self.locks.entries.lock().unwrap();
        if Arc::strong_count(&self.lock) == 2
            && entries
                .get(&self.profile_id)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.lock))
        {
            entries.remove(&self.profile_id);
        }
    }
}

/// Opaque, per-application OUP persistence resources. Connections share them;
/// independent in-process applications do not.
#[derive(Default)]
pub struct UiProtocolRuntimeResources {
    ledger: std::sync::OnceLock<Arc<ui_protocol_ledger::UiProtocolLedger>>,
    commit_observer: std::sync::OnceLock<octos_bus::MessageCommitObserver>,
}

pub struct AppState {
    /// Shared HTTP client for webhook proxying.
    /// Bootstrap admin auth token from config/env (used only until the
    /// hashed admin-token file is created via dashboard rotation).
    pub auth_token: Option<String>,

    /// Prometheus metrics handle.
    pub metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,

    /// Per-profile guard for AppUI skill install/remove plus runtime reload.
    pub profile_skill_mutation_locks: Arc<ProfileSkillMutationLocks>,

    /// Profile store for admin dashboard.
    pub profile_store: Option<Arc<ProfileStore>>,

    /// Per-profile runtime catalog. Built at startup from
    /// `ProfileStore::list()` — one [`ProfileRuntime`] per enabled
    /// profile with an active primary LLM. The `/api/chat` handler
    /// and UI Protocol dispatcher resolve the request's profile here,
    /// then ask [`Self::session_cache`] to materialize the matching
    /// `SessionRuntime` on demand.
    ///
    /// An unregistered profile is a configuration bug (M11-F deleted
    /// the legacy server-wide `agent` fallback); handlers fail closed
    /// with 503 when a request routes to a missing profile.
    pub profiles: HashMap<String, Arc<ProfileRuntime>>,

    /// TTL/LRU cache of per-session runtimes keyed by
    /// `(profile_id, session_key)`. Built once at startup;
    /// `/api/chat` and other dispatchers call `get_or_init` to
    /// materialize an `Arc<SessionRuntime>` per turn.
    pub session_cache: Arc<SessionRuntimeCache>,

    /// Process-wide [`octos_bus::SessionManager`] backed by
    /// `<data_dir>/sessions/`. Used by REST endpoints that browse and
    /// edit on-disk session history (`/api/sessions`, `/api/sessions/:id/messages`,
    /// `/api/sessions/:id/title`, …) and by the UI Protocol audit
    /// writer to resolve the canonical data_dir. `/api/chat` and the
    /// WS turn dispatcher route through the per-session
    /// `SessionRuntime.sessions` instead — the field stays here so
    /// the listing / metadata endpoints have a single shared handle.
    /// `None` in tests / setup-wizard deployments that haven't opened
    /// a SessionManager yet.
    pub sessions: Option<Arc<tokio::sync::Mutex<octos_bus::SessionManager>>>,

    /// Server start time.
    pub started_at: chrono::DateTime<chrono::Utc>,

    pub ui_protocol: UiProtocolRuntimeResources,

    pub http_client: reqwest::Client,
    /// Path to the global config.json file (for admin bot config editing).
    pub config_path: Option<PathBuf>,
    /// Persistent sysinfo instance for accurate CPU metrics across polls.
    pub sysinfo: tokio::sync::Mutex<sysinfo::System>,
    /// Tenant store for tunnel management.
    /// Cache of frps run_id → tenant_id from Login verification.
    /// Startup-normalized exact origins from `appui.allowed_origins` (or its
    /// non-empty environment override), plus loopback origins for the active
    /// serve port. CORS and both browser WebSocket gates consume this same
    /// list; authentication and work-secret validation remain separate.
    pub appui_allowed_origins: Vec<String>,
    /// Opt-in for the no-password "solo" REST login (`/api/auth/solo*`).
    /// OFF by default; set by `octos serve --solo` / `OCTOS_SOLO_LOGIN=1`.
    ///
    /// SECURITY: this flag alone is NOT sufficient to enable solo
    /// login. A hosted fleet daemon runs Local mode behind a Caddy reverse
    /// proxy, so every request reaches the daemon over loopback and would
    /// otherwise pass the loopback guard. This explicit opt-in (which fleet
    /// configs never set) is the primary defence; the handlers additionally
    /// reject any request carrying proxy-forwarding headers.
    pub solo_login_enabled: bool,
    /// `--danger-full-access`: sessions with NO explicit `/permissions`
    /// selection default to the dangerous full-access profile (sandbox off,
    /// network allowed, approvals never) instead of the gated
    /// workspace-write default — octos' analogue of Claude Code's
    /// `--dangerously-skip-permissions`. Solo-gated at serve startup (the
    /// same keystone that gates selecting the profile from the menu); an
    /// explicit per-session `/permissions` choice always overrides it.
    pub dangerous_default_permissions: bool,
    /// `--no-network` opt-OUT of the network-on default. By default a fresh
    /// solo/Local session with NO explicit `/permissions` selection resolves to
    /// Workspace-Write **with network ALLOWED** (filesystem still sandboxed) so
    /// the common dev workflow — `npm install`, git, fetch — works out of the
    /// box. Setting this (via `--no-network` / `OCTOS_NO_NETWORK=1`) reverts the
    /// default to Workspace-Write with network DENIED. Cloud/tenant deployments
    /// are unaffected (they always default to network-denied). An explicit
    /// per-session `/permissions` choice always overrides either default.
    pub default_network_denied: bool,
    /// `--llm-compaction`: AppUI context compaction asks an LLM for a
    /// higher-quality handoff summary (a real model call — slower, seconds)
    /// instead of the instant deterministic heuristic. Always falls back to the
    /// heuristic on any error/timeout/unsupported-runtime, so it can never
    /// break a turn. Default off.
    pub llm_compaction: bool,
    /// Resolved HOST-level memory policy (top-level config). Threaded into
    /// lazily-bootstrapped profile runtimes so a host opt-out of memory
    /// refresh (DEFAULT-ON) also binds profiles created after startup.
    pub host_memory: Option<crate::config::MemoryConfig>,
    /// Solo-profile id/email ledger (multi-user accounts were removed with
    /// the dashboard; the store remains for `profile/local/create`).
    /// Optional path to the JSONL harness-event sink. When `Some`,
    /// typed harness events (e.g. `SwarmReviewDecision`) are appended
    /// to the file in addition to being broadcast live to harness
    /// SSE subscribers. When `None`, events are broadcast-only — so a
    /// decision made while no subscriber is connected is lost. Wired
    /// by `octos serve` from the `OCTOS_HARNESS_EVENT_SINK` env var.
    pub harness_event_sink_path: Option<String>,
    /// Credential pool (M6.5, F-005). Initialised at startup from
    /// `config.credential_pool` when present; `None` falls back to the
    /// legacy single-credential flow. Shared with session actors so
    /// per-LLM-call `acquire`/`mark_*` operations see a consistent view.
    pub credential_pool: Option<Arc<octos_llm::PersistentCredentialPool>>,
    /// M7.9 / W2: shared session-task supervisor lookup. Used by the
    /// `POST /api/tasks/{task_id}/cancel` and
    /// `POST /api/tasks/{task_id}/restart-from-node` endpoints to
    /// forward to the matching `TaskSupervisor`. `None` keeps the
    /// pre-W2 behaviour — both endpoints return `503 Service
    /// Unavailable` so they fail closed instead of pretending a task
    /// was cancelled.
    pub task_query_store: Option<crate::session_actor::SessionTaskQueryStore>,
    /// Operator-configured default session cwd (`config.appui.default_session_cwd`).
    /// Mirrored into `AppState` so the per-session tool registry can tell
    /// the difference between "operator approved this directory as the
    /// session cwd" (Tier-2: respect it for plugin work_dirs too) and the
    /// boot-time `with_builtins_and_sandbox(serve_cwd)` fallback (Tier-3:
    /// route plugin output to `<data_dir>/skill-output` instead, since the
    /// serve cwd under launchd is `~`, outside the profile root, and
    /// `/api/files` would 403 anything written there).
    pub appui_default_session_cwd: Option<PathBuf>,
    /// Persistent session-ingress grant store for external CLI agents.
    ///
    /// `octos auth issue-work-secret` writes short-lived grants here and
    /// `/v1/session_ingress/ws/{session_id}` revalidates the token on
    /// every frame, so revocation applies to already-open sockets without
    /// requiring a daemon restart.
    pub work_secret_store: Arc<octos_agent::bridge::work_secret::WorkSecretGrantStore>,
}

impl AppState {
    /// Empty `AppState` for unit and integration tests — every
    /// store/service is `None`.
    ///
    /// Override individual fields with struct-update syntax:
    ///
    /// ```ignore
    /// let state = AppState {
    ///     profile_store: Some(profile_store),
    ///     ..AppState::empty_for_tests()
    /// };
    /// ```
    pub fn empty_for_tests() -> Self {
        let tmp =
            std::env::temp_dir().join(format!("octos-test-admin-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).ok();
        Self::without_services(&tmp)
    }

    /// State for an embedded local OUP transport. No HTTP listener, account
    /// services, login routes or gateway processes are started. Callers must
    /// install a profile runtime before accepting session/open.
    pub(crate) fn without_services(data_dir: &std::path::Path) -> Self {
        Self {
            ui_protocol: UiProtocolRuntimeResources::default(),
            profiles: HashMap::new(),
            session_cache: Arc::new(SessionRuntimeCache::new(
                64,
                std::time::Duration::from_secs(1800),
            )),
            profile_skill_mutation_locks: Arc::new(ProfileSkillMutationLocks::new()),
            sessions: None,
            started_at: chrono::Utc::now(),
            auth_token: None,
            metrics_handle: None,
            profile_store: None,
            http_client: reqwest::Client::new(),
            config_path: None,
            sysinfo: tokio::sync::Mutex::new(crate::sysinfo_budget::new_metrics_system()),
            appui_allowed_origins: Vec::new(),
            solo_login_enabled: false,
            dangerous_default_permissions: false,
            default_network_denied: false,
            llm_compaction: false,
            host_memory: None,
            harness_event_sink_path: None,
            credential_pool: None,
            task_query_store: None,
            appui_default_session_cwd: None,
            work_secret_store: Arc::new(
                octos_agent::bridge::work_secret::WorkSecretGrantStore::new(data_dir),
            ),
            // Tests don't spawn the sweeper. Tests that exercise the
            // sweeper either drive `sweep_expired_all` directly or
            // build their own `PreviewSweeperHandle::spawn(...)`.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn profile_skill_mutation_locks_publish_newer_mutation_last_and_prune_idle_entry() {
        let locks = Arc::new(ProfileSkillMutationLocks::new());
        let published = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let first = locks.lock("profile-a").await;
        let (newer_ready_tx, newer_ready_rx) = tokio::sync::oneshot::channel();
        let second_locks = Arc::clone(&locks);
        let second_published = Arc::clone(&published);
        let second = tokio::spawn(async move {
            let _ = newer_ready_tx.send(());
            let _guard = second_locks.lock("profile-a").await;
            second_published.lock().await.push("newer");
        });

        newer_ready_rx.await.unwrap();
        for _ in 0..100 {
            if locks.lock_handle_count("profile-a") >= 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            locks.lock_handle_count("profile-a") >= 4,
            "the newer mutation must be waiting behind the older mutation"
        );
        // The newer mutation has reached its completion barrier first. The
        // per-profile lock must still make the older publication win because
        // filesystem mutation and publication share the same critical section.
        published.lock().await.push("older");
        drop(first);
        second.await.unwrap();

        assert_eq!(*published.lock().await, vec!["older", "newer"]);
        assert_eq!(locks.entry_count(), 0);
    }

    #[tokio::test]
    async fn profile_skill_mutation_locks_prune_when_a_waiter_is_cancelled() {
        let locks = Arc::new(ProfileSkillMutationLocks::new());
        let first = locks.lock("profile-a").await;
        let waiter = {
            let locks = Arc::clone(&locks);
            tokio::spawn(async move {
                let _guard = locks.lock("profile-a").await;
                std::future::pending::<()>().await;
            })
        };

        for _ in 0..100 {
            if locks.lock_handle_count("profile-a") >= 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(locks.lock_handle_count("profile-a") >= 4);

        waiter.abort();
        waiter.await.unwrap_err();
        drop(first);

        assert_eq!(locks.entry_count(), 0);
    }

    #[tokio::test]
    async fn profile_skill_mutation_locks_allow_different_profiles_to_progress_independently() {
        let locks = Arc::new(ProfileSkillMutationLocks::new());
        let first = locks.lock("profile-a").await;
        let other_locks = Arc::clone(&locks);
        let other_profile = tokio::spawn(async move {
            let _guard = other_locks.lock("profile-b").await;
            "profile-b"
        });

        let completed = tokio::time::timeout(std::time::Duration::from_secs(1), other_profile)
            .await
            .expect("profile-b must not wait for profile-a")
            .unwrap();
        assert_eq!(completed, "profile-b");
        drop(first);
        assert_eq!(locks.entry_count(), 0);
    }
}
