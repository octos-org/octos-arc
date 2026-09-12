//! Serve command: start the REST API server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use colored::Colorize;
use eyre::{Result, WrapErr};
use octos_bus::SessionManager;

use super::Executable;
use crate::api::{
    AppState, EventBroadcaster, build_router, init_metrics, resolve_appui_allowed_origins,
};
use crate::config::Config;

// #1857 PR 5a — fleet worker pool defaults (serve boot). Conservative single-
// host values; not yet operator-configurable (5a is the dispatch backbone, not
// the tuning surface).
/// Max fleet attempts running across ALL fleets at once.
const FLEET_POOL_GLOBAL_CONCURRENCY: usize = 4;
/// Max fleet attempts running per single fleet at once.
const FLEET_POOL_PER_FLEET_CONCURRENCY: usize = 2;
/// Hard wall-clock ceiling for one attempt's agent run.
const FLEET_POOL_ATTEMPT_DEADLINE_SECS: u64 = 600;
/// Lease TTL stamped at launch. Chosen > the attempt deadline so boot
/// reconciliation reclaims only genuinely-abandoned (crashed-owner) leases, not
/// a healthy in-flight attempt.
const FLEET_POOL_LEASE_TTL_MS: u64 = 900_000;
/// Tokens reserved on the fleet budget at launch (soft admission). #1857 PR 5a
/// fix (MEDIUM): reduced from 50k — that is a whole small goal's budget, so a
/// modestly-budgeted goal would have EVERY task rejected. This is a per-attempt

/// #1857 PR 5a fix (HIGH 1) — a fleet worker's shell reach is bounded ONLY by
/// the sandbox (the closed worker tool set is a denylist, not a boundary). Fail
/// closed: the pool must be installed ONLY when the configured sandbox
/// constructs a REAL isolating backend — never [`NoSandbox`], which a disabled
/// sandbox (or `Auto` finding no backend on this host) yields and which would
/// give the worker unbounded network/host reach. Returns whether `sandbox_cfg`
/// yields real isolation.
///
/// [`NoSandbox`]: octos_agent::sandbox::NoSandbox
fn fleet_sandbox_is_isolating(sandbox_cfg: &octos_agent::sandbox::SandboxConfig) -> bool {
    let sandbox = octos_agent::sandbox::create_sandbox(sandbox_cfg);
    // A refusing resolution (explicit mode unhonorable on this host, or
    // sandbox.fail_closed with no backend) is fail-closed but useless to a
    // pool: every worker command would refuse. Treat it like a missing
    // backend — the pool is not installed — matching the pre-refusal
    // behaviour where these configs resolved to `NoSandbox` and were caught
    // by `is_noop()`.
    !sandbox.is_noop() && sandbox.refusal().is_none()
}

/// Whether the resolved sandbox backend can grant a `FsGrant::Host` worker FULL
/// daemon-user FS write together with the reads git needs — the third gate
/// condition for the fleet WORKTREE flow (§5). Computed at serve boot (mirrors
/// [`fleet_sandbox_is_isolating`]) from the base sandbox's
/// `supports_repo_git_write()`: `true` for bwrap and unrestricted-read macOS,
/// `false` for docker, restricted-read macOS, Landlock, AppContainer, and no
/// sandbox. When `false`, the pool falls back to a scratch workspace for every
/// task (a worktree worker whose `git commit` can't reach `<repo>/.git` would
/// lose its deliverable when the checkout is removed). Threaded into
/// `PoolConfig.repo_git_write_supported`.
fn fleet_sandbox_supports_repo_git_write(
    sandbox_cfg: &octos_agent::sandbox::SandboxConfig,
) -> bool {
    octos_agent::sandbox::create_sandbox(sandbox_cfg).supports_repo_git_write()
}

fn smtp_email_is_usable(email: &crate::profiles::EmailSettings) -> bool {
    if !email.provider.eq_ignore_ascii_case("smtp") {
        return false;
    }

    let host = email
        .smtp_host
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    let username = email.username.as_deref().map(str::trim).unwrap_or_default();
    let from_address = email
        .from_address
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    !host.is_empty() && !username.is_empty() && !from_address.is_empty()
}

fn profile_dashboard_auth_priority(profile: &crate::profiles::UserProfile) -> (u8, bool, &str) {
    let tier = if profile.id == crate::api::auth_handlers::ADMIN_PROFILE_ID {
        0
    } else if profile.config.admin_mode {
        1
    } else if profile.enabled && profile.parent_id.is_none() {
        2
    } else if profile.enabled {
        3
    } else {
        4
    };
    let usable_email = profile
        .config
        .email
        .as_ref()
        .is_some_and(smtp_email_is_usable);
    (tier, !usable_email, &profile.id)
}

fn preferred_dashboard_auth_profiles(
    profile_store: &crate::profiles::ProfileStore,
) -> Vec<crate::profiles::UserProfile> {
    let mut profiles = profile_store.list().unwrap_or_default();
    profiles.sort_by(|a, b| {
        profile_dashboard_auth_priority(a).cmp(&profile_dashboard_auth_priority(b))
    });
    profiles
}

fn derive_dashboard_auth_from_profile(
    profile: &crate::profiles::UserProfile,
) -> Option<(crate::otp::DashboardAuthConfig, Option<String>)> {
    let email = profile.config.email.as_ref()?;
    if !smtp_email_is_usable(email) {
        return None;
    }

    let host = email.smtp_host.as_ref()?.trim();
    let username = email.username.as_ref()?.trim();
    let from_address = email.from_address.as_ref()?.trim();
    let password = resolve_profile_email_secret(email, &profile.config.env_vars);
    let password_env = email
        .password_env
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("SMTP_PASSWORD")
        .to_string();

    Some((
        crate::otp::DashboardAuthConfig {
            smtp: Some(crate::otp::SmtpConfig {
                host: host.to_string(),
                port: email.smtp_port.unwrap_or(465),
                username: username.to_string(),
                password_env,
                from_address: from_address.to_string(),
            }),
            session_expiry_hours: 24,
            allow_self_registration: false,
            static_tokens: Vec::new(),
        },
        password,
    ))
}

fn derive_dashboard_auth_from_profiles(
    profile_store: &crate::profiles::ProfileStore,
) -> Option<(crate::otp::DashboardAuthConfig, Option<String>)> {
    for profile in preferred_dashboard_auth_profiles(profile_store) {
        if let Some(derived) = derive_dashboard_auth_from_profile(&profile) {
            tracing::info!(profile = %profile.id, "derived dashboard_auth.smtp from profile email tool config");
            return Some(derived);
        }
    }
    None
}

fn resolve_profile_email_secret(
    email: &crate::profiles::EmailSettings,
    env_vars: &std::collections::HashMap<String, String>,
) -> Option<String> {
    if let Some(password) = email.password.as_ref().filter(|value| !value.is_empty()) {
        return Some(password.clone());
    }

    let password_env = email
        .password_env
        .as_ref()
        .filter(|value| !value.is_empty())?;
    let value = env_vars.get(password_env)?;
    if value == crate::auth::keychain::KEYCHAIN_MARKER {
        crate::auth::keychain::get_secret(password_env)
            .ok()
            .flatten()
            .filter(|secret| !secret.is_empty())
    } else if value.is_empty() {
        None
    } else {
        Some(value.clone())
    }
}

fn profile_email_matches_dashboard_smtp(
    email: &crate::profiles::EmailSettings,
    smtp: &crate::otp::SmtpConfig,
) -> bool {
    email.provider.eq_ignore_ascii_case("smtp")
        && email
            .smtp_host
            .as_deref()
            .is_some_and(|host| host == smtp.host)
        && email
            .username
            .as_deref()
            .is_some_and(|username| username == smtp.username)
        && email
            .from_address
            .as_deref()
            .is_some_and(|from_address| from_address == smtp.from_address)
}

fn resolve_dashboard_auth_smtp_password(
    profile_store: &crate::profiles::ProfileStore,
    auth_config: &crate::otp::DashboardAuthConfig,
) -> Option<String> {
    // No SMTP block on disk → nothing to resolve.
    let smtp = auth_config.smtp.as_ref()?;
    if std::env::var(&smtp.password_env).is_ok() {
        return None;
    }

    for profile in preferred_dashboard_auth_profiles(profile_store) {
        if let Some(email) = profile.config.email.as_ref() {
            if profile_email_matches_dashboard_smtp(email, smtp) {
                if let Some(secret) = resolve_profile_email_secret(email, &profile.config.env_vars)
                {
                    tracing::info!(
                        profile = %profile.id,
                        "SMTP password resolved from matching profile email tool config"
                    );
                    return Some(secret);
                }
            }
        }
    }

    let profiles_for_smtp = profile_store.list().unwrap_or_default();
    for profile in &profiles_for_smtp {
        if let Some(password) = profile.config.env_vars.get(&smtp.password_env) {
            if password == crate::auth::keychain::KEYCHAIN_MARKER {
                if let Ok(Some(secret)) = crate::auth::keychain::get_secret(&smtp.password_env) {
                    tracing::info!(
                        var = %smtp.password_env,
                        "SMTP password resolved from keychain"
                    );
                    return Some(secret);
                }
            } else if !password.is_empty() {
                tracing::info!(
                    var = %smtp.password_env,
                    profile = %profile.id,
                    "SMTP password resolved from profile env_vars"
                );
                return Some(password.clone());
            }
        }
    }

    None
}

/// Start the REST API server.
///
/// `Serialize`/`Deserialize` back the layered startup config: the resolved
/// struct is serialized, non-explicit fields are overlaid from
/// `config.cli.serve`, then deserialized back (see [`crate::config_layer`]).
#[derive(Debug, Args, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ServeCommand {
    /// Port to listen on. Default lives in IANA's Dynamic/Private range
    /// (49152–65535) to avoid collisions with `http-alt` services like
    /// Tomcat/Jenkins/ominix-api. See issue #417.
    #[arg(short, long, default_value = "50080")]
    pub port: u16,

    /// Host address to bind to. Defaults to localhost for security.
    /// Use 0.0.0.0 to accept connections from all interfaces.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Run AppUI JSON-RPC over stdin/stdout instead of binding HTTP.
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

    /// Auth token for API access (overrides config).
    #[arg(long)]
    pub auth_token: Option<String>,

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

/// Wire a `task_query_store` for `octos serve --stdio` (the in-process
/// AppUI/TUI deployment); leave it `None` for HTTP/gateway serve.
///
/// `--stdio` runs session turns in *this* process with no gateway to proxy
/// `task/cancel` to. The per-turn `tool_registry.supervisor()` self-registers
/// into this store (see `ui_protocol.rs`, the `store.register(..)` guarded on
/// `task_query_store.is_some()`, holding a `Weak<TaskSupervisor>` so it prunes
/// at end of turn), which lets `handle_task_cancel` reach the live supervisor
/// and actually cancel a running `spawn_only` background task. Without it the
/// AppUI task commands fail `runtime_unavailable` ("task supervisor not wired
/// for AppUI task commands"). HTTP/gateway serve must stay `None` so
/// `handle_task_cancel` keeps proxying to the gateway via `resolve_api_port`.
fn stdio_task_query_store(stdio: bool) -> Option<crate::session_actor::SessionTaskQueryStore> {
    stdio.then(crate::session_actor::SessionTaskQueryStore::default)
}

/// Bind the HTTP listener before constructing `AppState`.
///
/// Port `0` asks the OS for an ephemeral port. Resolving it here ensures every
/// downstream consumer (gateway launch configuration, browser-Origin policy,
/// and user-facing URLs) sees the real port instead of the sentinel `0`.
/// Stdio mode does not bind HTTP and preserves the configured value.
async fn bind_http_listener(
    stdio: bool,
    host: &str,
    requested_port: u16,
) -> Result<(Option<tokio::net::TcpListener>, u16)> {
    if stdio {
        return Ok((None, requested_port));
    }

    let listener = tokio::net::TcpListener::bind((host, requested_port))
        .await
        .wrap_err_with(|| format!("failed to bind octos API server to {host}:{requested_port}"))?;
    let actual_port = listener
        .local_addr()
        .wrap_err("failed to inspect bound octos API listener")?
        .port();
    Ok((Some(listener), actual_port))
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

        let (config, resolved_config_path) = if let Some(config_path) = &self.config {
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

        let broadcaster = Arc::new(EventBroadcaster::new(256));

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
        let metrics_handle = Some(init_metrics());

        // Security: warn if binding to non-localhost without auth token
        // Check CLI arg, then OCTOS_AUTH_TOKEN env var
        let auth_token = if self.auth_token.is_some() {
            self.auth_token
        } else if let Ok(env_token) = std::env::var("OCTOS_AUTH_TOKEN") {
            Some(env_token)
        } else if let Some(ref cfg_token) = config.auth_token {
            if !cfg_token.is_empty() {
                Some(cfg_token.clone())
            } else {
                None
            }
        } else if self.host != "127.0.0.1" && self.host != "localhost" && self.host != "::1" {
            tracing::warn!(
                "Binding to {} without --auth-token is dangerous! \
                 Generating a random token for this session.",
                self.host
            );
            // Generate cryptographically random token
            use rand::Rng;
            let mut rng = rand::thread_rng();
            let a: u64 = rng.r#gen();
            let b: u64 = rng.r#gen();
            let token = format!("{a:016x}{b:016x}");
            println!(
                "{}: {} (auto-generated, pass --auth-token to set your own)",
                "Auth token".yellow(),
                token
            );
            Some(token)
        } else {
            None
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

        // M11-F regression fix REG-4: bootstrap bundled app-skills
        // (`crates/app-skills/`) and platform-skills (`crates/platform-
        // skills/`) into `<octos_home>/{bundled-app-skills,platform-
        // skills}/` so every `ProfileRuntime` we build below can scan
        // them via `Config::plugin_dirs_from_project`. Pre-M11-F
        // `serve.rs::try_create_agent` did this unconditionally per
        // agent build; M11-F deleted the helper and never restored the
        // call, so a clean install of `octos serve` came up with zero
        // bundled skills available to `/api/chat` (weather, time, news,
        // deep-search) and zero platform skills (voice). Doing it once
        // at process startup matches the gateway flow and keeps the
        // per-profile loop free of redundant disk writes.
        if self.stdio && self.solo {
            tracing::info!("stdio/solo: bundled app-skills and platform-skills bootstrap disabled");
        } else {
            octos_agent::bootstrap::bootstrap_bundled_skills(&data_dir);
            octos_agent::bootstrap::bootstrap_platform_skills(&data_dir);
        }
        // Preflight: if the sibling app-skill binaries are missing beside the
        // running `octos` executable, bootstrap silently skipped them and the
        // affected tools (get_weather, etc.) will NOT register. Warn loudly so
        // a bare-binary deploy is diagnosable instead of a silent plugin_count=0.
        let missing = octos_agent::bootstrap::missing_bundled_skill_binaries();
        if !(self.stdio && self.solo) && !missing.is_empty() {
            tracing::warn!(
                missing = ?missing,
                "bundled skill binaries missing beside the octos executable — this looks like a bare-binary install; app-skill tools (get_weather, etc.) will NOT register. Deploy the full bundle (scripts/build-local-bundle.sh / scripts/install.sh), not just the octos binary."
            );
        }
        // Gap 4.1: bundle generic pipelines (deep_research) into the
        // dedicated `<data_dir>/bundled-pipelines` dir so `run_pipeline`
        // always discovers them even when the `mofa-research` skill carrying
        // `deep_research.dot` has drifted off a profile. Per-profile
        // `RunPipelineTool`s register that dir as the LOWEST-precedence
        // search path via `with_octos_home` (bootstrap-dir == search-dir).
        // Installed pipelines of the same name always win (no clobber).
        octos_agent::bootstrap::bootstrap_bundled_pipelines(&data_dir);

        // M11-D — build the per-profile runtime catalog. For every
        // enabled profile that has an active primary LLM selection,
        // call `ProfileRuntime::bootstrap` and stash the resulting
        // `Arc<ProfileRuntime>` under its profile id. Failures are
        // logged and skipped so a single bad profile cannot 503 the
        // whole server.
        //
        // `ProfileRuntime::bootstrap` opens a per-profile
        // `EpisodeStore` / `MemoryStore` / `ToolConfigStore` against
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
            // Section B (codex review round-3): thread the host's
            // strict-signing policy so the per-profile plugin load honors
            // `plugins.require_signed = true` from the top-level config
            // even when individual profile JSONs omit the field.
            match crate::runtime::ProfileRuntime::bootstrap_with_host_plugins(
                profile,
                &profile_data_dir,
                Some(&data_dir),
                crate::runtime::BootstrapRole::Serve,
                Some(&config.plugins),
                config.voice.as_ref(),
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
                    // build-cache pool config so `stage_peer` can acquire a
                    // first-turn slot under `<data_dir>/build-cache/…`.
                    // Absent `[build_cache]` uses pool defaults. Only roots
                    // absent from the process side-table have no pool.
                    // Same key derivation as every release site
                    // (`<data_dir>/peers`), so acquire/release always agree.
                    crate::peers::set_build_cache_config(
                        &rt.data_dir.join("peers"),
                        Some(profile.config.build_cache.clone().unwrap_or_default()),
                    );
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

        let (http_listener, effective_serve_port) =
            bind_http_listener(self.stdio, &self.host, self.port).await?;

        let bridge_js_path = data_dir.join("whatsapp-bridge").join("bridge.js");
        let process_manager = Arc::new(
            crate::process_manager::ProcessManager::new(profile_store.clone())
                .with_bridge_js(bridge_js_path)
                .with_serve_config(effective_serve_port, auth_token.clone())
                // Section B (codex review round-5 P1.2): every spawned
                // gateway inherits the host's strict-signing policy via
                // an env var. `Config::from_file` OR-merges it onto the
                // gateway's effective `plugins.require_signed`.
                .with_host_plugins_require_signed(config.plugins.require_signed)
                .with_host_max_inject_tokens(
                    config.memory.as_ref().and_then(|m| m.max_inject_tokens),
                )
                .with_host_memory_refresh_enabled(crate::config::MemoryConfig::refresh_enabled(
                    config.memory.as_ref(),
                ))
                .with_host_asr_language(
                    config
                        .voice
                        .as_ref()
                        .and_then(|voice| voice.asr_language.clone()),
                ),
        );
        process_manager.set_self_ref();

        // Initialize user store and auth manager for multi-user support
        let user_store = Arc::new(
            crate::user_store::UserStore::open(&data_dir).wrap_err("failed to open user store")?,
        );
        let allowlist_store = Arc::new(
            crate::login_allowlist::LoginAllowlistStore::open(&data_dir)
                .wrap_err("failed to open login allowlist store")?,
        );
        let admin_audit_store = Arc::new(
            crate::admin_audit_store::AdminAuditStore::open(&data_dir)
                .wrap_err("failed to open admin audit store")?,
        );
        let auth_manager = {
            let (auth_config, derived_profile_password) = match config.dashboard_auth.clone() {
                Some(auth) => (Some(auth), None),
                None => {
                    let derived = derive_dashboard_auth_from_profiles(&profile_store);
                    if derived.is_some() {
                        tracing::info!(
                            "derived dashboard_auth.smtp from a profile email tool config"
                        );
                    } else {
                        tracing::warn!(
                            "no dashboard_auth.smtp configured and no usable profile SMTP email tool found — OTP codes will be logged to console only"
                        );
                    }
                    match derived {
                        Some((auth, password)) => (Some(auth), password),
                        None => (None, None),
                    }
                }
            };
            let mut mgr = crate::otp::AuthManager::new(auth_config.clone(), user_store.clone())
                .with_sessions_path(data_dir.join("auth_sessions.json"))
                .with_data_dir(data_dir.clone())
                // Registration id generation must treat an existing
                // PROFILE file as taken, or a generated id claims an
                // admin-created-but-unclaimed profile (codex #1613
                // r6/r8). Policy lives on the store — see
                // id_reserved_for_registration: anonymous claims never
                // pass a file; authorized (allowlist) claims pass only
                // a cleanly-loadable record.
                .with_id_taken_probe({
                    let ps = profile_store.clone();
                    std::sync::Arc::new(move |id: &str, authorized: bool| {
                        ps.id_reserved_for_registration(id, authorized)
                    })
                });

            if let Some(password) = derived_profile_password {
                mgr = mgr.with_smtp_password(password);
            }

            // Resolve SMTP password from profile email config / env_vars as fallback
            // (covers nohup startup where LaunchAgent env vars aren't available)
            if let Some(ref auth_cfg) = auth_config {
                if let Some(password) =
                    resolve_dashboard_auth_smtp_password(&profile_store, auth_cfg)
                {
                    mgr = mgr.with_smtp_password(password);
                }
            }

            Some(Arc::new(mgr))
        };

        // Spawn auth cleanup task if auth manager is active
        if let Some(ref am) = auth_manager {
            let am_clone = am.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
                loop {
                    interval.tick().await;
                    am_clone.cleanup().await;
                }
            });
        }

        // Pre-create watchdog/alerts flags for both Monitor and AppState
        let (watchdog_flag, alerts_flag) = {
            let wf = config
                .monitor
                .as_ref()
                .map(|m| Arc::new(std::sync::atomic::AtomicBool::new(m.watchdog_enabled)));
            let af = config
                .monitor
                .as_ref()
                .map(|m| Arc::new(std::sync::atomic::AtomicBool::new(m.alerts_enabled)));
            (wf, af)
        };

        // F-005: Wire the credential pool at startup. Absent config →
        // stays `None` so the session actor falls back to the legacy
        // single-credential flow.
        let credential_pool_init =
            super::build_credential_pool(config.credential_pool.as_ref(), &data_dir);

        // F-005: Build the content classifier at startup. Absent config
        // or `enabled: false` → stays `None` so routing keeps the
        // pre-M6.6 strong-only default (invariant #3 of issue #493).
        let content_classifier_init: Option<Arc<octos_llm::ContentClassifier>> = config
            .content_routing
            .as_ref()
            .filter(|cfg| cfg.enabled)
            .map(|cfg| Arc::new(octos_llm::ContentClassifier::new(cfg.clone())));

        let harness_sink_init = std::env::var("OCTOS_HARNESS_EVENT_SINK").ok();

        // Issue #1001 follow-up: in-memory signed-preview token cache.
        // Issue #1009: construct the cache first so we can spawn the
        // background sweeper and own the resulting handle inside
        // `AppState` — when the last `Arc<AppState>` is dropped the
        // wrapper aborts the task instead of leaking it (the previous
        // local-binding pattern relied on `process::exit(0)` and would
        // strand the sweeper on any error-path drop).
        let preview_tokens = Arc::new(crate::api::PreviewTokens::new());
        let preview_sweeper = crate::api::PreviewSweeperHandle::spawn(
            preview_tokens.clone(),
            crate::api::DEFAULT_PREVIEW_SWEEP_INTERVAL,
        );

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
        // `effective_permissions_for_session` only grants DangerFullAccess in
        // Local deployment mode; enabling the default under Tenant/Cloud would
        // fail every unselected `session/open` at permission resolution and
        // let `profile/list` advertise a current profile the runtime rejects
        // (codex P2 on #1639). Refuse the misconfiguration at startup.
        if dangerous_default_permissions_flag && config.mode != crate::config::DeploymentMode::Local
        {
            eyre::bail!(
                "--danger-full-access requires Local deployment mode (mode is {:?}); \
                 the full-access profile is not grantable under Tenant/Cloud, so an \
                 unselected session would fail permission resolution",
                config.mode
            );
        }
        // Resolve browser origins once, before any HTTP route is exposed.
        // A malformed explicit origin aborts startup instead of silently
        // weakening CORS/WS behavior. Empty env means "use config"; a
        // non-empty env value replaces the config list for deployments.
        let appui_allowed_origins_env = match std::env::var("OCTOS_APPUI_ALLOWED_ORIGINS") {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                eyre::bail!("OCTOS_APPUI_ALLOWED_ORIGINS must be valid Unicode")
            }
        };
        let appui_allowed_origins = resolve_appui_allowed_origins(
            &config.appui.allowed_origins,
            appui_allowed_origins_env.as_deref(),
            effective_serve_port,
        )
        .wrap_err("invalid AppUI browser-origin configuration")?;

        let state = Arc::new(AppState {
            ui_protocol: crate::api::UiProtocolRuntimeResources::default(),
            profiles: profile_runtimes,
            session_cache,
            profile_skill_mutation_locks: Arc::new(crate::api::ProfileSkillMutationLocks::new()),
            sessions,
            broadcaster,
            started_at: chrono::Utc::now(),
            auth_token,
            admin_token_store: Arc::new(crate::admin_token_store::AdminTokenStore::new(&data_dir)),
            setup_state_store: Arc::new(crate::setup_state_store::SetupStateStore::new(&data_dir)),
            metrics_handle,
            profile_store: Some(profile_store.clone()),
            process_manager: Some(process_manager.clone()),
            user_store: Some(user_store),
            allowlist_store: Some(allowlist_store),
            admin_audit_store: Some(admin_audit_store),
            auth_manager,
            http_client: reqwest::Client::new(),
            // If a config file was loaded, admin edits target that exact file.
            // If none existed at startup, fall back to THIS serve's resolved
            // config_home (which already accounts for the `--data-dir` FLAG —
            // not just env), so admin writes under `serve --data-dir T` land in
            // `T/config.json`, not a recomputed XDG path. (admin_setup's own
            // None branch can't see the CLI flag; this closes that leak.)
            config_path: resolved_config_path.or_else(|| Some(ctx.config_home.join("config.json"))),
            watchdog_enabled: watchdog_flag.clone(),
            alerts_enabled: alerts_flag.clone(),
            // task-sysinfo-proc-stat-fd-budget: no startup process snapshot,
            // no retained /proc handles (see sysinfo_budget).
            sysinfo: tokio::sync::Mutex::new(crate::sysinfo_budget::new_metrics_system()),
            tenant_store: crate::tenant::TenantStore::open(&data_dir)
                .ok()
                .map(Arc::new),
            run_id_cache: Arc::new(crate::api::RunIdCache::new()),
            tunnel_domain: config
                .tunnel_domain
                .clone()
                .or_else(|| std::env::var("TUNNEL_DOMAIN").ok()),
            // `OCTOS_BASE_DOMAIN` (env) takes precedence over config.json so
            // operators can override without touching the file. `None` falls
            // back to `crate::api::DEFAULT_BASE_DOMAIN` at read sites.
            base_domain: std::env::var("OCTOS_BASE_DOMAIN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| config.base_domain.clone().filter(|s| !s.trim().is_empty())),
            appui_allowed_origins,
            frps_server: config
                .frps_server
                .clone()
                .or_else(|| std::env::var("FRPS_SERVER").ok()),
            frps_port: std::env::var("FRPS_PORT").ok().and_then(|p| p.parse().ok()),
            deployment_mode: config.mode.clone(),
            host_memory: config.memory.clone(),
            solo_login_enabled: solo_login_enabled_flag,
            dangerous_default_permissions: dangerous_default_permissions_flag,
            default_network_denied: default_network_denied_flag,
            llm_compaction: self.llm_compaction,
            allow_admin_shell: config.allow_admin_shell,
            content_catalog_mgr: Some(Arc::new(
                crate::content_catalog::ContentCatalogManager::new(profile_store.clone()),
            )),
            // Harness JSONL event sink — wired from the
            // `OCTOS_HARNESS_EVENT_SINK` env var when the caller wants
            // review decisions and swarm dispatch events persisted (see
            // `/api/events/harness`). `None` keeps the pre-M7.6
            // behaviour of broadcast-only.
            harness_event_sink_path: harness_sink_init,
            credential_pool: credential_pool_init,
            content_classifier: content_classifier_init,
            // HTTP/gateway serve: session actors live in gateway
            // processes, so `task_query_store` stays `None` and the
            // cancel/restart handlers proxy via `resolve_api_port` (the
            // gateway runtime sets its own store on the embedded api
            // channel). `--stdio` runs actors in-process with no gateway,
            // so it wires an empty store the per-turn supervisor
            // self-registers into — letting AppUI `task/cancel` reach live
            // `spawn_only` tasks. See `stdio_task_query_store`.
            task_query_store: stdio_task_query_store(self.stdio),
            // Mirror the operator-configured Tier-2 default cwd so
            // `session_tool_registry` can distinguish "operator chose this
            // dir for sessions" from the boot fallback baked in by
            // `with_builtins_and_sandbox(serve_cwd)`. See
            // `api/ui_protocol.rs::session_tool_registry`.
            appui_default_session_cwd: config.appui.default_session_cwd.clone(),
            // Issue #1001 follow-up: in-memory signed-preview token
            // cache backs `POST /api/my/preview/sign` /
            // `GET /api/preview-signed/...` so the SPA iframe can drop
            // the `Authorization: Bearer ...` header that the closed
            // `/api/preview/...` route now requires. Daemon restart
            // invalidates every grant (see
            // `crate::api::preview_tokens` for the design rationale).
            preview_tokens,
            work_secret_store: Arc::new(
                octos_agent::bridge::work_secret::WorkSecretGrantStore::new(&data_dir),
            ),
            // Issue #1009: owning sweeper handle. `Drop` aborts the
            // tokio task when the last `Arc<AppState>` is released,
            // replacing the previous `_preview_sweeper` local that
            // leaked the task on any non-`process::exit(0)` shutdown
            // path.
            preview_sweeper: Some(preview_sweeper),
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

        if self.stdio {
            crate::api::ui_protocol_transport::stdio_connection(state).await?;
            tracing::info!("stopping all gateway child processes");
            let _ = process_manager.stop_all().await;
            return Ok(());
        }

        // Auto-start enabled profiles
        let profiles = profile_store.list().unwrap_or_default();
        let enabled_count = profiles.iter().filter(|p| p.enabled).count();
        tracing::info!(
            total = profiles.len(),
            enabled = enabled_count,
            "loaded profiles"
        );
        if enabled_count > 0 {
            for p in &profiles {
                if p.enabled {
                    if !p.config.has_llm_selection() {
                        tracing::warn!(
                            profile = %p.id,
                            "skipping auto-start: no LLM provider configured"
                        );
                        continue;
                    }
                    tracing::info!(profile = %p.id, "auto-starting gateway");
                    if let Err(e) = process_manager.start(p).await {
                        tracing::warn!(profile = %p.id, error = %e, "failed to auto-start gateway");
                    }
                }
            }
        }

        // Profile file watcher: auto-restart gateways when profile JSON changes.
        {
            let ps = profile_store.clone();
            let pm = process_manager.clone();
            tokio::spawn(async move {
                use crate::profiles::{ProfileChange, UserProfile, diff_profiles};
                use sha2::{Digest, Sha256};
                use std::collections::HashMap;

                // Snapshot of known profile states: (hash, profile)
                let mut known: HashMap<String, ([u8; 32], UserProfile)> = HashMap::new();
                // Seed with current profiles
                if let Ok(list) = ps.list() {
                    for p in list {
                        if let Ok(bytes) = std::fs::read(ps.profile_path(&p.id)) {
                            let hash: [u8; 32] = Sha256::digest(&bytes).into();
                            known.insert(p.id.clone(), (hash, p));
                        }
                    }
                }

                // NOTE(#149): The 5-second poll interval is hardcoded. This could be made
                // configurable (e.g. via a CLI flag or config field) for deployments that
                // need faster detection or want to reduce filesystem polling overhead.
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                loop {
                    interval.tick().await;
                    let current = match ps.list() {
                        Ok(list) => list,
                        Err(_) => continue,
                    };
                    for profile in &current {
                        let bytes = match std::fs::read(ps.profile_path(&profile.id)) {
                            Ok(b) => b,
                            Err(_) => continue,
                        };
                        let hash: [u8; 32] = Sha256::digest(&bytes).into();

                        if let Some((old_hash, old_profile)) = known.get(&profile.id) {
                            if hash == *old_hash {
                                continue; // no change
                            }
                            let status = pm.status(&profile.id).await;

                            // Handle enable/disable transitions
                            if !old_profile.enabled && profile.enabled && !status.running {
                                // disabled → enabled: start gateway
                                tracing::info!(
                                    profile = %profile.id,
                                    "profile enabled, starting gateway"
                                );
                                if let Err(e) = pm.start(profile).await {
                                    tracing::warn!(
                                        profile = %profile.id,
                                        error = %e,
                                        "failed to start gateway after enable"
                                    );
                                }
                            } else if old_profile.enabled && !profile.enabled && status.running {
                                // enabled → disabled: stop gateway
                                tracing::info!(
                                    profile = %profile.id,
                                    "profile disabled, stopping gateway"
                                );
                                if let Err(e) = pm.stop(&profile.id).await {
                                    tracing::warn!(
                                        profile = %profile.id,
                                        error = %e,
                                        "failed to stop gateway after disable"
                                    );
                                }
                            } else if status.running {
                                // Config changed while running — check if restart needed
                                match diff_profiles(old_profile, profile) {
                                    ProfileChange::RestartRequired(fields) => {
                                        tracing::info!(
                                            profile = %profile.id,
                                            fields = ?fields,
                                            "profile changed (restart-required fields), restarting gateway"
                                        );
                                        if let Err(e) = pm.restart(profile).await {
                                            tracing::warn!(
                                                profile = %profile.id,
                                                error = %e,
                                                "failed to restart gateway after profile change"
                                            );
                                        }
                                    }
                                    ProfileChange::HotReloadable => {
                                        tracing::debug!(
                                            profile = %profile.id,
                                            "profile changed (hot-reloadable only), gateway watcher will handle"
                                        );
                                    }
                                    ProfileChange::Unchanged => {}
                                }
                            } else if profile.enabled && !status.running {
                                // Profile changed & enabled but not running — start it
                                tracing::info!(
                                    profile = %profile.id,
                                    "profile changed and enabled but not running, starting gateway"
                                );
                                if let Err(e) = pm.start(profile).await {
                                    tracing::warn!(
                                        profile = %profile.id,
                                        error = %e,
                                        "failed to start gateway"
                                    );
                                }
                            }
                        } else if profile.enabled {
                            // New profile detected — auto-start its gateway
                            tracing::info!(
                                profile = %profile.id,
                                "new profile detected, starting gateway"
                            );
                            if let Err(e) = pm.start(profile).await {
                                tracing::warn!(
                                    profile = %profile.id,
                                    error = %e,
                                    "failed to auto-start gateway for new profile"
                                );
                            }
                        }
                        known.insert(profile.id.clone(), (hash, profile.clone()));
                    }
                }
            });
        }

        // Start monitor (watchdog + health checks + alerts)
        {
            use crate::monitor::{FeishuAlertSender, Monitor, TelegramAlertSender};
            use std::sync::atomic::AtomicBool;
            use std::time::Duration;

            let monitor_cfg = config.monitor.clone();

            if let Some(ref mon_cfg) = monitor_cfg {
                let shutdown = Arc::new(AtomicBool::new(false));
                let (alert_tx, alert_rx) = tokio::sync::mpsc::channel(256);

                // Use shared flags from AppState
                let watchdog_enabled = watchdog_flag
                    .clone()
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(mon_cfg.watchdog_enabled)));
                let alerts_enabled = alerts_flag
                    .clone()
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(mon_cfg.alerts_enabled)));

                // Wire alert sender into process manager
                process_manager.set_alert_sender(alert_tx);

                let mut monitor = Monitor::new(
                    profile_store.clone(),
                    process_manager.clone(),
                    alert_rx,
                    watchdog_enabled.clone(),
                    alerts_enabled.clone(),
                    mon_cfg.max_restart_attempts,
                    Duration::from_secs(mon_cfg.health_check_interval_secs),
                    shutdown,
                );

                // Add Telegram alert sender if configured
                if let Some(ref token_env) = mon_cfg.telegram_token_env {
                    if let Ok(token) = std::env::var(token_env) {
                        if !mon_cfg.telegram_alert_chat_ids.is_empty() {
                            monitor.add_sender(Box::new(TelegramAlertSender::new(
                                token,
                                mon_cfg.telegram_alert_chat_ids.clone(),
                            )));
                        }
                    }
                }

                // Add Feishu alert sender if configured
                if let Some(ref app_id_env) = mon_cfg.feishu_app_id_env {
                    if let Ok(app_id) = std::env::var(app_id_env) {
                        let secret_env = mon_cfg
                            .feishu_app_secret_env
                            .as_deref()
                            .unwrap_or("FEISHU_APP_SECRET");
                        if let Ok(app_secret) = std::env::var(secret_env) {
                            if !mon_cfg.feishu_alert_user_ids.is_empty() {
                                monitor.add_sender(Box::new(FeishuAlertSender::new(
                                    app_id,
                                    app_secret,
                                    mon_cfg.feishu_alert_user_ids.clone(),
                                    "cn",
                                )));
                            }
                        }
                    }
                }

                tokio::spawn(async move { monitor.run().await });
                tracing::info!("monitor started (watchdog + health checks + alerts)");
            }
        }

        // (#1973 fix E — the global master-continuation drain used to be
        // spawned HERE, after the stdio early-return; it now spawns right
        // before that branch so stdio serves share the safety net.)
        let app = build_router(state);
        let listener =
            http_listener.expect("non-stdio serve must bind its HTTP listener before AppState");
        let addr = listener
            .local_addr()
            .wrap_err("failed to inspect bound octos API listener")?
            .to_string();

        tracing::info!(address = %addr, "octos API server starting");
        tracing::info!(app = %format!("http://{}/app/", addr), "web app available");
        tracing::info!(dashboard = %format!("http://{}/admin/", addr), "admin dashboard available");
        if enabled_count > 0 {
            tracing::info!(count = enabled_count, "gateway profiles auto-started");
        }

        use super::serve_console;
        let _ = serve_console::print_stdout(&format!("{}", "octos API server".cyan().bold()));
        let _ = serve_console::print_stdout(&format!("{}: http://{}", "Listening".green(), addr));
        let _ = serve_console::print_stdout(&format!("{}: http://{}/app/", "App".green(), addr));
        let _ = serve_console::print_stdout(&format!(
            "{}: http://{}/admin/",
            "Admin dashboard".green(),
            addr
        ));
        if enabled_count > 0 {
            let _ = serve_console::print_stdout(&format!(
                "{}: {} profiles auto-started",
                "Gateways".green(),
                enabled_count
            ));
        }
        let _ = serve_console::print_stdout("");

        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            let _ = serve_console::print_stdout("");
            let _ = serve_console::print_stdout(&format!("{}", "Shutting down server...".yellow()));
        })
        .await?;

        // Stop all gateway child processes before exiting
        tracing::info!("stopping all gateway child processes");
        let _ = serve_console::print_stdout(&format!("{}", "Stopping gateways...".yellow()));
        let stopped = process_manager.stop_all().await;
        if stopped > 0 {
            tracing::info!(count = stopped, "gateways stopped");
            let _ = serve_console::print_stdout(&format!("  stopped {} gateway(s)", stopped));
        }

        // Force exit — background tokio tasks (profile watcher, auth cleanup,
        // admin bot) have no shutdown signal and would hang indefinitely.
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_serve_wires_task_query_store_for_in_process_cancel() {
        // `--stdio` runs session actors in-process with no gateway to
        // proxy `task/cancel` to, so the store must be present for the
        // per-turn supervisor to self-register into — otherwise AppUI
        // task commands fail `runtime_unavailable` and octoscode Esc/`x`
        // cannot cancel a spawned background task (the reported bug).
        assert!(
            stdio_task_query_store(true).is_some(),
            "stdio serve must wire a task_query_store"
        );
    }

    #[test]
    fn non_stdio_serve_leaves_task_query_store_none_for_gateway_proxy() {
        // HTTP/gateway serve must leave it `None` so `handle_task_cancel`
        // takes the gateway-proxy path; a non-`None` store would skip it.
        assert!(
            stdio_task_query_store(false).is_none(),
            "gateway/http serve must leave task_query_store None"
        );
    }

    #[tokio::test]
    async fn port_zero_resolves_before_origin_configuration() {
        let (listener, effective_port) = bind_http_listener(false, "127.0.0.1", 0)
            .await
            .expect("bind an ephemeral loopback listener");
        let listener = listener.expect("HTTP serve returns a bound listener");

        assert_ne!(effective_port, 0, "the OS-selected port must be concrete");
        assert_eq!(
            listener.local_addr().unwrap().port(),
            effective_port,
            "all downstream configuration must use the bound listener's port"
        );
        let origins = resolve_appui_allowed_origins(&[], None, effective_port).unwrap();
        assert!(origins.contains(&format!("http://127.0.0.1:{effective_port}")));
        assert!(origins.contains(&format!("http://localhost:{effective_port}")));
        assert!(origins.contains(&format!("http://[::1]:{effective_port}")));
    }

    /// #1857 PR 5a fix (HIGH 1) — the fleet worker pool installs ONLY behind a
    /// REAL isolating sandbox. A disabled sandbox (or an explicit `None` mode)
    /// yields `NoSandbox`, which the fail-closed predicate must reject so a fleet
    /// worker never runs unsandboxed with network reach. (The real-backend side
    /// is host-dependent — `Auto` may resolve to `NoSandbox` on a CI host with no
    /// bwrap/docker — so only the fail-closed direction is asserted here.)
    #[test]
    fn fleet_pool_requires_a_real_isolating_sandbox() {
        use octos_agent::sandbox::{SandboxConfig, SandboxMode};
        let disabled = SandboxConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(
            !fleet_sandbox_is_isolating(&disabled),
            "a disabled sandbox must NOT be treated as isolating",
        );
        let none_mode = SandboxConfig {
            enabled: true,
            mode: SandboxMode::None,
            ..Default::default()
        };
        assert!(
            !fleet_sandbox_is_isolating(&none_mode),
            "SandboxMode::None must NOT be treated as isolating",
        );
    }

    /// Fail-closed twin: a config whose resolution REFUSES (an explicit mode
    /// unhonorable on this host) is fail-closed but useless to a pool — every
    /// worker command would refuse — so the boot gate must not install the
    /// pool behind it, matching the old behaviour where the same configs
    /// degraded to `NoSandbox` and were caught by `is_noop()`. The refusing
    /// resolution reports `is_noop() == false`, so without the dedicated
    /// `refusal()` check this would regress to installing a dead pool.
    #[test]
    fn fleet_pool_rejects_a_refusing_sandbox_resolution() {
        use octos_agent::sandbox::{SandboxConfig, SandboxMode};
        // Unhonorable on every host this test runs on: landlock requires
        // Linux (and, on Linux, the octos-sandbox helper, absent in unit-test
        // runners); appcontainer requires Windows.
        let unhonorable = SandboxConfig {
            mode: if cfg!(windows) {
                SandboxMode::Landlock
            } else {
                SandboxMode::AppContainer
            },
            ..Default::default()
        };
        assert!(
            !fleet_sandbox_is_isolating(&unhonorable),
            "a refusing sandbox resolution must NOT install the fleet pool",
        );
    }

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

    fn dashboard_smtp_test_env_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        #[allow(unsafe_code)]
        fn remove(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: dashboard SMTP env tests hold `dashboard_smtp_test_env_lock`,
            // serializing mutation of process-wide environment variables.
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: callers keep the env lock for the full guard lifetime.
            match self.previous.as_ref() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn deployment_mode_is_explicit_and_ignores_tunnel_settings() {
        let config = Config {
            mode: crate::config::DeploymentMode::Local,
            tunnel_domain: Some("octos-cloud.org".to_string()),
            frps_server: Some("127.0.0.1".to_string()),
            ..Default::default()
        };

        assert_eq!(config.mode, crate::config::DeploymentMode::Local);
    }

    #[test]
    fn deployment_mode_preserves_explicit_cloud_mode() {
        let config = Config {
            mode: crate::config::DeploymentMode::Cloud,
            tunnel_domain: None,
            frps_server: None,
            ..Default::default()
        };

        assert_eq!(config.mode, crate::config::DeploymentMode::Cloud);
    }

    #[test]
    fn derives_dashboard_auth_from_admin_profile_email_tool() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::profiles::ProfileStore::open_unified(dir.path()).unwrap();
        store
            .save(&crate::profiles::UserProfile {
                id: crate::api::auth_handlers::ADMIN_PROFILE_ID.into(),
                name: "Admin".into(),
                enabled: true,
                data_dir: None,
                parent_id: None,
                public_subdomain: None,
                config: crate::profiles::ProfileConfig {
                    email: Some(crate::profiles::EmailSettings {
                        provider: "smtp".into(),
                        smtp_host: Some("smtp.example.com".into()),
                        smtp_port: Some(587),
                        username: Some("admin@example.com".into()),
                        password_env: Some("SMTP_PASSWORD".into()),
                        password: None,
                        from_address: Some("admin@example.com".into()),
                        feishu_app_id: None,
                        feishu_app_secret_env: None,
                        feishu_app_secret: None,
                        feishu_from_address: None,
                        feishu_region: None,
                    }),
                    env_vars: std::collections::HashMap::from([(
                        "SMTP_PASSWORD".into(),
                        "secret".into(),
                    )]),
                    ..Default::default()
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();

        let (auth, password) = derive_dashboard_auth_from_profiles(&store)
            .expect("dashboard auth should derive from admin profile");
        assert_eq!(auth.smtp.as_ref().unwrap().host, "smtp.example.com");
        assert_eq!(auth.smtp.as_ref().unwrap().port, 587);
        assert_eq!(auth.smtp.as_ref().unwrap().username, "admin@example.com");
        assert_eq!(auth.smtp.as_ref().unwrap().password_env, "SMTP_PASSWORD");
        assert_eq!(
            auth.smtp.as_ref().unwrap().from_address,
            "admin@example.com"
        );
        assert_eq!(password.as_deref(), Some("secret"));
    }

    #[test]
    fn dashboard_smtp_password_prefers_matching_admin_profile_email_tool() {
        let _guard = dashboard_smtp_test_env_lock().lock().unwrap();
        let _env = EnvVarGuard::remove("OCTOS_TEST_DASHBOARD_AUTH_ADMIN_SMTP_PASSWORD");
        let dir = tempfile::tempdir().unwrap();
        let store = crate::profiles::ProfileStore::open_unified(dir.path()).unwrap();
        store
            .save(&crate::profiles::UserProfile {
                id: crate::api::auth_handlers::ADMIN_PROFILE_ID.into(),
                name: "Admin".into(),
                enabled: true,
                data_dir: None,
                parent_id: None,
                public_subdomain: None,
                config: crate::profiles::ProfileConfig {
                    email: Some(crate::profiles::EmailSettings {
                        provider: "smtp".into(),
                        smtp_host: Some("smtp.example.com".into()),
                        smtp_port: Some(465),
                        username: Some("admin@example.com".into()),
                        password_env: Some("IGNORED_ENV".into()),
                        password: Some("secret".into()),
                        from_address: Some("admin@example.com".into()),
                        feishu_app_id: None,
                        feishu_app_secret_env: None,
                        feishu_app_secret: None,
                        feishu_from_address: None,
                        feishu_region: None,
                    }),
                    ..Default::default()
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();

        let auth = crate::otp::DashboardAuthConfig {
            smtp: Some(crate::otp::SmtpConfig {
                host: "smtp.example.com".into(),
                port: 465,
                username: "admin@example.com".into(),
                password_env: "OCTOS_TEST_DASHBOARD_AUTH_ADMIN_SMTP_PASSWORD".into(),
                from_address: "admin@example.com".into(),
            }),
            session_expiry_hours: 24,
            allow_self_registration: false,
            static_tokens: Vec::new(),
        };

        let password = resolve_dashboard_auth_smtp_password(&store, &auth);
        assert_eq!(password.as_deref(), Some("secret"));
    }

    #[test]
    fn derives_dashboard_auth_from_first_usable_non_admin_profile() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::profiles::ProfileStore::open_unified(dir.path()).unwrap();
        store
            .save(&crate::profiles::UserProfile {
                id: crate::api::auth_handlers::ADMIN_PROFILE_ID.into(),
                name: "Admin".into(),
                enabled: true,
                data_dir: None,
                parent_id: None,
                public_subdomain: None,
                config: crate::profiles::ProfileConfig {
                    email: Some(crate::profiles::EmailSettings {
                        provider: "smtp".into(),
                        smtp_host: Some(String::new()),
                        smtp_port: Some(465),
                        username: Some(String::new()),
                        password_env: Some("SMTP_PASSWORD".into()),
                        password: None,
                        from_address: Some(String::new()),
                        feishu_app_id: None,
                        feishu_app_secret_env: None,
                        feishu_app_secret: None,
                        feishu_from_address: None,
                        feishu_region: None,
                    }),
                    ..Default::default()
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        store
            .save(&crate::profiles::UserProfile {
                id: "dspfac".into(),
                name: "DSPFAC".into(),
                enabled: true,
                data_dir: None,
                parent_id: None,
                public_subdomain: None,
                config: crate::profiles::ProfileConfig {
                    email: Some(crate::profiles::EmailSettings {
                        provider: "smtp".into(),
                        smtp_host: Some("smtp.gmail.com".into()),
                        smtp_port: Some(465),
                        username: Some("dspfac@gmail.com".into()),
                        password_env: Some("SMTP_PASSWORD".into()),
                        password: Some("app-password".into()),
                        from_address: Some("dspfac@gmail.com".into()),
                        feishu_app_id: None,
                        feishu_app_secret_env: None,
                        feishu_app_secret: None,
                        feishu_from_address: None,
                        feishu_region: None,
                    }),
                    ..Default::default()
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();

        let (auth, password) = derive_dashboard_auth_from_profiles(&store)
            .expect("dashboard auth should derive from usable profile");
        assert_eq!(auth.smtp.as_ref().unwrap().host, "smtp.gmail.com");
        assert_eq!(auth.smtp.as_ref().unwrap().username, "dspfac@gmail.com");
        assert_eq!(auth.smtp.as_ref().unwrap().from_address, "dspfac@gmail.com");
        assert_eq!(password.as_deref(), Some("app-password"));
    }

    #[test]
    fn dashboard_smtp_password_prefers_matching_non_admin_profile_email_tool() {
        let _guard = dashboard_smtp_test_env_lock().lock().unwrap();
        let _env = EnvVarGuard::remove("OCTOS_TEST_DASHBOARD_AUTH_PROFILE_SMTP_PASSWORD");
        let dir = tempfile::tempdir().unwrap();
        let store = crate::profiles::ProfileStore::open_unified(dir.path()).unwrap();
        store
            .save(&crate::profiles::UserProfile {
                id: "dspfac".into(),
                name: "DSPFAC".into(),
                enabled: true,
                data_dir: None,
                parent_id: None,
                public_subdomain: None,
                config: crate::profiles::ProfileConfig {
                    email: Some(crate::profiles::EmailSettings {
                        provider: "smtp".into(),
                        smtp_host: Some("smtp.gmail.com".into()),
                        smtp_port: Some(587),
                        username: Some("dspfac@gmail.com".into()),
                        // Env var NAME, not a password. This field previously
                        // held a 16-lowercase-char literal — the exact shape of
                        // a Gmail App Password — next to a real gmail username.
                        password_env: Some("SMTP_PASSWORD".into()),
                        password: Some("app-password".into()),
                        from_address: Some("dspfac@gmail.com".into()),
                        feishu_app_id: None,
                        feishu_app_secret_env: None,
                        feishu_app_secret: None,
                        feishu_from_address: None,
                        feishu_region: None,
                    }),
                    env_vars: std::collections::HashMap::from([(
                        "SMTP_PASSWORD".into(),
                        crate::auth::keychain::KEYCHAIN_MARKER.into(),
                    )]),
                    ..Default::default()
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();

        let auth = crate::otp::DashboardAuthConfig {
            smtp: Some(crate::otp::SmtpConfig {
                host: "smtp.gmail.com".into(),
                port: 465,
                username: "dspfac@gmail.com".into(),
                password_env: "OCTOS_TEST_DASHBOARD_AUTH_PROFILE_SMTP_PASSWORD".into(),
                from_address: "dspfac@gmail.com".into(),
            }),
            session_expiry_hours: 24,
            allow_self_registration: false,
            static_tokens: Vec::new(),
        };

        let password = resolve_dashboard_auth_smtp_password(&store, &auth);
        assert_eq!(password.as_deref(), Some("app-password"));
    }

}
