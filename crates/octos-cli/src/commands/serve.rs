//! Serve command: speak the UI Protocol JSON-RPC over stdin/stdout.
//!
//! slim5-batch4: serve is stdio-only. The REST router, HTTP listener,
//! metrics exporter and browser-origin policy were removed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use eyre::{Result, WrapErr};
use octos_bus::SessionManager;

use super::Executable;
use crate::api::AppState;
use crate::config::Config;

/// Start the stdio AppUI JSON-RPC server.
///
/// `Serialize`/`Deserialize` back the layered startup config: the resolved
/// struct is serialized, non-explicit fields are overlaid from
/// `config.cli.serve`, then deserialized back (see [`crate::config_layer`]).
#[derive(Debug, Args, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ServeCommand {
    /// Accepted for compatibility: serve always runs the stdio AppUI
    /// JSON-RPC transport (the HTTP listener was removed in slim5-batch4).
    #[arg(long)]
    pub stdio: bool,

    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Per-instance runtime data dir (redb stores, sessions, goals, serve lock,
    /// per-profile data). When set, the profile REGISTRY + model catalog still
    /// resolve from the shared state home (the normal
    /// `--data-dir`/`OCTOS_HOME`/`~/.octos`), so many stdio instances share one
    /// config/profile while each owns private runtime state. Unset ⇒ identical
    /// to today (runtime == state home).
    ///
    /// Also settable via `OCTOS_INSTANCE_DATA_DIR` (the flag wins; an empty env
    /// value is treated as unset). Env is resolved in `run_async` because the
    /// workspace `clap` build does not enable the `env` feature.
    #[arg(long)]
    pub instance_data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// LLM provider to use (overrides config).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model to use (overrides config).
    #[arg(long)]
    pub model: Option<String>,

    /// Enable the no-password "solo" login (`POST /api/auth/solo*`) for a
    /// local single-user install. OFF by default. Only honoured for direct
    /// loopback requests on a Local-mode host with profile/user stores, and
    /// never when the request carries reverse-proxy headers. Also settable
    /// via `OCTOS_SOLO_LOGIN=1`. Do NOT set on a host fronted by a reverse
    /// proxy (e.g. the Caddy-fronted fleet) — see `api::solo_auth`.
    #[arg(long)]
    pub solo: bool,

    /// Default every session to the dangerous FULL-ACCESS permission
    /// profile: sandbox disabled, network allowed, approvals never —
    /// octos' analogue of Claude Code's `--dangerously-skip-permissions`.
    /// Requires `--solo` (the same local-single-user keystone that gates
    /// selecting Full Access from the `/permissions` menu). A session's
    /// explicit `/permissions` choice still overrides the default. Also
    /// settable via `OCTOS_DANGER_FULL_ACCESS=1`.
    #[arg(long)]
    pub danger_full_access: bool,

    /// Opt OUT of the network-on default. By default a fresh Local session with
    /// no explicit `/permissions` choice runs Workspace-Write with network
    /// ALLOWED (filesystem still sandboxed) so `npm install` / git / fetch work
    /// out of the box. Pass `--no-network` (or `OCTOS_NO_NETWORK=1`) to revert
    /// the default to network DENIED. Cloud/tenant deployments always default to
    /// network-denied regardless. An explicit `/permissions` choice still wins.
    #[arg(long)]
    pub no_network: bool,

    /// Use LLM-summarization for AppUI context compaction: when a session's
    /// context fills, ask the model for a high-quality handoff summary (a real
    /// model call — slower, a few seconds) instead of the instant deterministic
    /// heuristic. Falls back to the heuristic on any error/timeout, so it never
    /// breaks a turn. Off by default.
    #[arg(long)]
    pub llm_compaction: bool,

    /// Disable automatic retry on transient errors.
    #[arg(long)]
    pub no_retry: bool,
}

/// Stable, machine-greppable marker embedded in the "data directory is already
/// owned by another serve" error. octoscode (a separate repo) spawns
/// `octos serve --stdio` as a child and greps its stderr for this exact token
/// on child-exit to recognize the single-writer conflict and STOP relaunching —
/// instead of the silent ~5s crash-loop it used to hit when the second serve
/// died mid-startup opening `admin_audit.redb`. MUST stay byte-stable: the
/// client matches it verbatim (octoscode `transport.rs` DATA_DIR_LOCKED_MARKER).
pub(crate) const DATA_DIR_LOCKED_MARKER: &str = "OCTOS_DATA_DIR_LOCKED";

/// Held for the serve process's whole lifetime: an exclusive OS advisory lock
/// (flock / LockFileEx via `fs2`) on `<data_dir>/.octos-serve.lock`. redb is
/// single-writer-single-process, so two `octos serve` against one data dir can
/// never coexist — the second used to crash mid-startup opening the first
/// data-dir-level redb store (`admin_audit.redb`) with `DatabaseAlreadyOpen`,
/// which a stdio client silently respawned in a loop. Taking this lock BEFORE
/// any store open turns that into one clean, greppable refusal. The lock is
/// released explicitly when the guard drops, so a forked child's temporary
/// duplicate descriptor cannot prolong ownership after normal serve shutdown.
/// Closing the final descriptor also releases it on abrupt process exit.
struct ServeDataDirLock {
    _file: std::fs::File,
}

impl Drop for ServeDataDirLock {
    fn drop(&mut self) {
        // Unix flock follows the shared open file description, not this one
        // descriptor. Close-on-exec still leaves a fork-to-exec window where a
        // child holds a reference. The guard owns the lock lifetime; explicitly
        // end it before File::drop closes our descriptor. This guard is never
        // cloned, and remains alive until the serve's stores have shut down.
        if let Err(error) = fs2::FileExt::unlock(&self._file) {
            tracing::warn!(%error, "failed to release serve single-writer lock");
        }
    }
}

/// Acquire the serve single-writer lock for `data_dir`, or return a clear error
/// carrying [`DATA_DIR_LOCKED_MARKER`] when another serve already holds it.
/// Contention is detected structurally via the platform's canonical
/// lock-contended errno (`fs2::lock_contended_error`), never string matching.
/// Fully-qualified `fs2::FileExt` calls: std 1.89 grew inherent methods of the
/// same names and the workspace MSRV is 1.85.
fn acquire_serve_data_dir_lock(data_dir: &std::path::Path) -> Result<ServeDataDirLock> {
    std::fs::create_dir_all(data_dir)
        .wrap_err_with(|| format!("failed to create data dir: {}", data_dir.display()))?;
    let lock_path = data_dir.join(".octos-serve.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .wrap_err_with(|| format!("failed to open serve lockfile: {}", lock_path.display()))?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(ServeDataDirLock { _file: file }),
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Err(eyre::eyre!(
                "{DATA_DIR_LOCKED_MARKER}: another octos server is already running for this data \
                 directory ({}). Close the other octoscode (or `octos serve`), or start this one \
                 against a different --data-dir.",
                data_dir.display()
            ))
        }
        Err(error) => Err(eyre::Report::new(error).wrap_err(format!(
            "failed to acquire serve single-writer lock: {}",
            lock_path.display()
        ))),
    }
}

impl Executable for ServeCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

impl ServeCommand {
    async fn run_async(self) -> Result<()> {
        let cwd = match &self.cwd {
            Some(p) => p.clone(),
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };
        // Resolve the canonical config context once (data_dir for runtime
        // state; config_home/is_default for config; auth_home for global auth)
        // and run migrations.
        let ctx = super::resolve_command_context(self.data_dir.clone())?;
        let data_dir = ctx.data_dir.clone();

        // Multi-instance stdio split. `state_home` is the SHARED, config-like
        // root that holds the profile REGISTRY and the model catalog — always
        // the normal resolution (`--data-dir`/`OCTOS_HOME`/`~/.octos`), never
        // the per-instance dir. The `data_dir` used from here on is the
        // per-instance RUNTIME root (redb stores, sessions, goals, serve lock,
        // per-profile data): the private per-instance dir when set, else the
        // state home (byte-identical to today for default installs and for
        // gateways, which never set a per-instance dir).
        //
        // The per-instance dir comes from `--instance-data-dir` (flag wins) or
        // `OCTOS_INSTANCE_DATA_DIR` (empty ⇒ unset). Env is read here because
        // the workspace `clap` build omits the `env` feature.
        let state_home = data_dir.clone();
        let instance_data_dir = self.instance_data_dir.clone().or_else(|| {
            std::env::var("OCTOS_INSTANCE_DATA_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from)
        });
        let data_dir = instance_data_dir
            .clone()
            .unwrap_or_else(|| state_home.clone());
        if instance_data_dir.is_some() {
            // A fresh per-instance dir must exist before the serve lock and
            // redb stores open under it.
            std::fs::create_dir_all(&data_dir).wrap_err_with(|| {
                format!(
                    "failed to create per-instance data dir: {}",
                    data_dir.display()
                )
            })?;
        }

        let (config, _) = if let Some(config_path) = &self.config {
            tracing::info!(path = %config_path.display(), "loading config (--config)");
            (Config::from_file(config_path)?, Some(config_path.clone()))
        } else {
            Config::load_with_context_path(&cwd, &ctx)?
        };
        // ARC-Bench drives the local single-user stdio transport. Keep that
        // transport aligned with `octos chat --profile coding`: its default
        // tool envelope is intentionally lean and bundled app/platform skills
        // are opt-in, rather than model-visible startup baggage.
        if self.stdio && self.solo {
            crate::runtime::profile::enable_stdio_solo_lean_defaults();
            tracing::info!(
                "stdio/solo defaulting to the coding tool profile and skipping bundled skills"
            );
        }
        tracing::info!(data_dir = %data_dir.display(), "data directory resolved");

        // Single-writer guard: redb is single-process, so a second `octos serve`
        // on this data dir can't coexist. Fail FAST here with one clean,
        // client-greppable refusal instead of crashing mid-startup opening
        // `admin_audit.redb` (which a stdio client silently respawned in a
        // ~5s loop). Held for the whole process via `_data_dir_lock`; released
        // on exit so a relaunch after the prior serve quits still starts.
        let _data_dir_lock = match acquire_serve_data_dir_lock(&data_dir) {
            Ok(guard) => guard,
            Err(error) => {
                if error.to_string().contains(DATA_DIR_LOCKED_MARKER) {
                    // A guaranteed clean, un-colored stderr line the octoscode
                    // client greps on child-exit (color-eyre's rendering of the
                    // returned error may interleave ANSI, so don't rely on it).
                    let _ = super::serve_console::print_stderr(&format!(
                        "{DATA_DIR_LOCKED_MARKER}: another octos server already owns data \
                         directory {}",
                        data_dir.display()
                    ));
                }
                return Err(error);
            }
        };

        // M11-F: per-profile LLM, credentials, tool registry, plugins,
        // MCP, and memory are built once per profile below via
        // `ProfileRuntime::bootstrap`. There is no longer a
        // server-wide agent; an unregistered profile returns 503 at
        // the handler.
        //
        // We still open a process-wide `SessionManager` against the
        // top-level data dir so the read-only REST endpoints
        // (`/api/sessions`, `/api/sessions/:id/messages`, …) and the UI
        // Protocol audit writer have a single shared handle for the
        // canonical JSONL store.
        let sessions: Option<Arc<tokio::sync::Mutex<SessionManager>>> =
            match SessionManager::open(&data_dir) {
                Ok(mgr) => Some(Arc::new(tokio::sync::Mutex::new(mgr))),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "failed to open process-wide SessionManager; \
                         REST session listing endpoints will return empty"
                    );
                    None
                }
            };
        // Initialize profile store and process manager for admin dashboard.
        // Registry (`<id>.json`) resolves from the SHARED `state_home`; the
        // per-profile `<id>/data` runtime tree roots under the per-instance
        // `data_dir`. With no `--instance-data-dir`, `state_home == data_dir`,
        // so this is byte-identical to `open_unified(&data_dir)`.
        tracing::info!("initializing profile store and process manager");
        let profile_store = Arc::new(
            crate::profiles::ProfileStore::open(&state_home, &data_dir)
                .wrap_err("failed to open profile store")?,
        );

        // orchestrator. The data-dir serve lock tells the CLI whether this
        // endpoint is mandatory; a missing endpoint while the lock is held is
        // therefore a fail-closed old-version/startup condition, never an
        // excuse to append an offline snapshot behind the live cache.

        // M11-D — build the per-profile runtime catalog. For every
        // enabled profile that has an active primary LLM selection,
        // call `ProfileRuntime::bootstrap` and stash the resulting
        // `Arc<ProfileRuntime>` under its profile id. Failures are
        // logged and skipped so a single bad profile cannot 503 the
        // whole server.
        //
        // `ProfileRuntime::bootstrap` opens a per-profile
        // `EpisodeStore` / `MemoryStore` against
        // the profile's data dir. M11-F removed the legacy
        // server-wide `Agent`, so these are now the only redb opens
        // against the profile data dir from `octos serve` — no lock
        // contention.
        let mut profile_runtimes: HashMap<String, Arc<crate::runtime::ProfileRuntime>> =
            HashMap::new();
        let all_profiles = profile_store.list().unwrap_or_default();
        for profile in &all_profiles {
            if !profile.enabled || profile.parent_id.is_some() {
                continue;
            }
            if !profile.config.has_llm_selection() {
                tracing::debug!(
                    profile_id = %profile.id,
                    "skipping ProfileRuntime bootstrap: no LLM selection",
                );
                continue;
            }
            let profile_data_dir = profile_store.resolve_data_dir(profile);
            // Resolve the full runtime profile through the single shared
            // resolver: parent/sub-account inheritance THEN the store's global
            // `profile-defaults.json` base, so inherited hooks / plugins /
            // sandbox / memory settings reach the per-profile bootstrap. Absent
            // parent + defaults ⇒ effective config == `profile.config` (no
            // behavior change).
            let profile = profile_store.resolve_runtime_profile(profile);
            let profile = &profile;
            match crate::runtime::ProfileRuntime::bootstrap_with_host_memory(
                profile,
                &profile_data_dir,
                Some(&data_dir),
                crate::runtime::BootstrapRole::Serve,
                config.memory.as_ref(),
            )
            .await
            {
                Ok(rt) => {
                    tracing::info!(
                        profile_id = %profile.id,
                        provider = %rt.provider_name,
                        model = %rt.primary_model_id,
                        tools = rt.tool_specs.specs().len(),
                        "ProfileRuntime bootstrapped for /api/chat",
                    );
                    // Outer-loop #4 (§4.1/§7.2): install this profile's
                    profile_runtimes.insert(profile.id.clone(), rt);
                }
                Err(error) => {
                    tracing::warn!(
                        profile_id = %profile.id,
                        %error,
                        "ProfileRuntime bootstrap failed — /api/chat will return 503 for this profile",
                    );
                }
            }
        }

        let session_cache = Arc::new(
            crate::runtime::SessionRuntimeCache::new(64, std::time::Duration::from_secs(1800))
                // Per-project session storage (opt-in, default off). When set,
                // a cwd-hinted AppUi session's transcript store relocates to
                // `<cwd>/.octos`; no-hint/gateway sessions are unaffected.
                .with_sessions_in_cwd(config.appui.sessions_in_cwd),
        );

        // Spawn auth cleanup task if auth manager is active

        // Issue #1001 follow-up: in-memory signed-preview token cache.
        // Issue #1009: construct the cache first so we can spawn the
        // background sweeper and own the resulting handle inside
        // `AppState` — when the last `Arc<AppState>` is dropped the
        // wrapper aborts the task instead of leaking it (the previous
        // local-binding pattern relied on `process::exit(0)` and would
        // strand the sweeper on any error-path drop).
        let solo_login_enabled_flag = self.solo
            || std::env::var("OCTOS_SOLO_LOGIN")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
        let dangerous_default_permissions_flag = self.danger_full_access
            || std::env::var("OCTOS_DANGER_FULL_ACCESS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
        let default_network_denied_flag = self.no_network
            || std::env::var("OCTOS_NO_NETWORK")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
        // SECURITY KEYSTONE: the dangerous default rides the SAME solo
        // opt-in that gates selecting Full Access from the menu — a fleet
        // config that never sets --solo can reach neither surface.
        if dangerous_default_permissions_flag && !solo_login_enabled_flag {
            eyre::bail!(
                "--danger-full-access requires --solo (local single-user opt-in); \
                 refusing to default sessions to the dangerous profile on a \
                 potentially shared host"
            );
        }
        let state = Arc::new(AppState {
            ui_protocol: crate::api::UiProtocolRuntimeResources::default(),
            profiles: profile_runtimes,
            session_cache,
            profile_skill_mutation_locks: Arc::new(crate::api::ProfileSkillMutationLocks::new()),
            sessions,
            started_at: chrono::Utc::now(),
            profile_store: Some(profile_store.clone()),
            host_memory: config.memory.clone(),
            solo_login_enabled: solo_login_enabled_flag,
            dangerous_default_permissions: dangerous_default_permissions_flag,
            default_network_denied: default_network_denied_flag,
            llm_compaction: self.llm_compaction,
            // stdio-only serve: session actors run in-process with no
            // gateway, so the per-turn supervisor self-registers into an
            // empty store — letting AppUI `task/cancel` reach live
            // `spawn_only` background tasks.
            task_query_store: Some(crate::session_actor::SessionTaskQueryStore::default()),
            // Mirror the operator-configured Tier-2 default cwd so
            // `session_tool_registry` can distinguish "operator chose this
            // dir for sessions" from the boot fallback baked in by
            // `with_builtins_and_sandbox(serve_cwd)`. See
            // `api/ui_protocol.rs::session_tool_registry`.
            appui_default_session_cwd: config.appui.default_session_cwd.clone(),
        });

        // mini5 soak gap #1 / #1973 fix E: drain queued master continuations
        // (ChildCompleted / ScatterJoinComplete / GoalContinue / LoopFire)
        // even when NO ws/stdio client is connected. The per-connection
        // `appui_continuation_tick` only runs inside a live handler loop, so a
        // sub-agent finishing while the TUI is disconnected (or a continuation
        // re-loaded after a serve restart) would otherwise sit undrained until
        // a client reconnects. Shares the process-global active-turns registry
        // with the per-connection ticks, so there is no double-run.
        //
        // #1973 fix E — spawned BEFORE the stdio early-return below, so
        // `serve --stdio` (headless stdio deployments) gets the same
        // continuation safety net and escalation-timeout sweep the HTTP serve
        // always had. This is a deliberate behavior change for stdio serves:
        // restored goal/loop continuations now drain even while the stdio
        // client is idle or detached, instead of waiting for connection ticks.
        // Everything the drain needs (the full AppState) is constructed above.

        // #2019 — install the HUMAN sink over background events that today
        // only wake the model (monitor event lines, claimed fleet outbox
        // events). Spawned here, next to (and before) the global drain, so
        // both `serve --stdio` and the HTTP serve get it: the producers are
        // the connection-independent watcher tasks and the outbox consumer,
        // so the sink must not be per-connection either. Purely additive —
        // it changes nothing about how or when the model is woken.
        crate::api::ui_protocol_transport::spawn_background_activity_sink(state.clone());

        crate::api::ui_protocol_transport::stdio_connection(state).await?;

        // Force exit — background tokio tasks (profile watcher, auth cleanup,
        // admin bot) have no shutdown signal and would hang indefinitely.
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two `octos serve` against one data dir can't coexist (redb is
    /// single-process). The second must be refused FAST with a stable,
    /// client-greppable marker — not crash mid-startup opening `admin_audit.redb`
    /// (which a stdio client respawned in a silent ~5s loop). Releasing the first
    /// (process exit) must free the lock so a legitimate relaunch still starts.
    #[test]
    fn second_serve_on_same_data_dir_is_refused_with_a_greppable_marker() {
        let dir = tempfile::tempdir().unwrap();

        let first = acquire_serve_data_dir_lock(dir.path()).expect("first serve acquires the lock");

        let err = acquire_serve_data_dir_lock(dir.path())
            .err()
            .expect("a second serve must be refused while the first holds the lock");
        assert!(
            err.to_string().contains(DATA_DIR_LOCKED_MARKER),
            "refusal must carry the stable client-greppable marker; got: {err}"
        );
        assert!(
            err.to_string().contains(&dir.path().display().to_string()),
            "refusal must name the contended data dir; got: {err}"
        );

        // Prior serve exits → lock released → a fresh relaunch acquires it. This
        // pins that the guard never false-positives a normal client relaunch.
        drop(first);
        let _relaunch = acquire_serve_data_dir_lock(dir.path())
            .expect("after the holder exits, a fresh serve acquires the lock");
    }

    /// Unix flock ownership follows the open file description, so a forked
    /// child can briefly retain it even when its descriptor is close-on-exec.
    /// `try_clone` reproduces that shared-description lifetime deterministically,
    /// without needing another process or depending on scheduler timing.
    // fs2's Solaris backend emulates flock with process-owned fcntl locks.
    #[cfg(all(unix, not(target_os = "solaris")))]
    #[test]
    fn should_release_serve_data_dir_lock_when_duplicate_descriptor_outlives_guard() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_serve_data_dir_lock(dir.path()).expect("first serve acquires lock");
        let inherited = first._file.try_clone().expect("duplicate lock descriptor");
        assert!(
            acquire_serve_data_dir_lock(dir.path()).is_err(),
            "duplicating a descriptor must not release the live guard's lock"
        );

        drop(first);
        let relaunch = acquire_serve_data_dir_lock(dir.path())
            .expect("guard drop must unlock even while a duplicate descriptor remains open");
        drop(inherited);
        assert!(
            acquire_serve_data_dir_lock(dir.path()).is_err(),
            "closing the previous holder's duplicate must not unlock the new guard"
        );

        drop(relaunch);
        let _next = acquire_serve_data_dir_lock(dir.path())
            .expect("the new guard still releases its own lock on drop");
    }
}
