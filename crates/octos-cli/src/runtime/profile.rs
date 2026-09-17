//! Profile-scope runtime state.
//!
//! See the crate-level [`super`] module docs and
//! `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md` for the two-scope model.
//! This file owns the [`ProfileRuntime`] type and the M11-D self-
//! contained implementation of [`ProfileRuntime::bootstrap`] — the
//! canonical per-profile assembler `octos serve` and `octos gateway`
//! both call.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eyre::{Result, WrapErr};
use octos_agent::{HookExecutor, SandboxConfig, ToolPolicy, ToolRegistry, create_sandbox};
use octos_llm::{LlmProvider, QosCatalog};
use octos_memory::{EpisodeStore, MemoryStore};
use tracing::{info, warn};

use crate::commands::chat;
use crate::commands::gateway::build_system_prompt;
use crate::config::Config;
use crate::profiles::{UserProfile, config_from_profile};
use crate::qos_catalog::build_provider_chain;
use crate::skills_scope::build_account_skills_loader;

static STDIO_SOLO_LEAN_DEFAULTS: AtomicBool = AtomicBool::new(false);

pub(crate) fn enable_stdio_solo_lean_defaults() {
    STDIO_SOLO_LEAN_DEFAULTS.store(true, Ordering::Release);
}

fn stdio_solo_lean_defaults_enabled() -> bool {
    STDIO_SOLO_LEAN_DEFAULTS.load(Ordering::Acquire)
        || std::env::var("OCTOS_SKIP_BUNDLED_SKILLS").ok().as_deref() == Some("1")
}

/// All long-lived state that belongs to a single profile within the
/// current host process.
///
/// One `ProfileRuntime` per `(host process, profile_id)`. The host
/// process is `octos serve`, `octos gateway` (each subprocess), or
/// `octoscode` — every entry point that today reads a [`UserProfile`]
/// off disk and turns it into a running agent ends up holding an
/// `Arc<ProfileRuntime>`.
///
/// # What lives here
///
/// Anything that is an *account property* of the logged-in user:
///
/// - **`llm`** — the top-level LLM provider chain (already wrapped by
///   `RetryProvider` → `ProviderChain`). Two sessions opened by the
///   same user hit the same provider chain.
/// - **`credentials`** — resolved API keys / secrets keyed by env-var
///   name. Populated from `profile.config.env_vars` via the keychain;
///   passed to MCP server spawns and plugin invocations on the session
///   side.
/// - **`skills_dir`** — the per-profile plugin directory
///   (`~/.octos/profiles/<id>/data/skills/`), if it exists. Used at
///   bootstrap time to register profile-scoped skills into
///   [`Self::tool_specs`].
/// - **`plugin_env_template`** — the env-var pairs (e.g.
///   `OCTOS_PROFILE_ID`, `OCTOS_VOICE_DIR`) every plugin spawn for
///   this profile should inherit. Sessions clone this into their own
///   plugin spawns; if a session needs to add session-scoped vars it
///   does so on top of this template.
/// - **`tool_policy`** — the profile's allow/deny tool policy. The
///   policy is *applied per session* (after the session clones
///   [`Self::tool_specs`]) so policy edits don't require rebuilding
///   the base registry.
/// - **`default_sandbox`** — the sandbox config every session
///   inherits unless it explicitly overrides via
///   [`super::SessionRuntime::sandbox`].
/// - **`tool_specs`** — the base [`ToolRegistry`] template. It has
///   builtins registered, plugins loaded, MCP agents wired, the LRU
///   pin set applied — *but no workspace bound*. Sessions clone this
///   and call `with_workspace_root` to get a workspace-bound registry.
///   This is the M11 fix for the multi-tenant base-registry leak
///   codex flagged on PR #868.
/// - **`memory`** / **`memory_store`** — the per-profile
///   [`EpisodeStore`] (redb at `<data_dir>/episodes.redb`) and
///   [`MemoryStore`] (MEMORY.md, daily notes). Memory is profile-
///   scoped because it crosses sessions — a long-running fact a user
///   teaches the agent in one room should be recallable in another
///   room of the same profile.
///
/// # What does NOT live here
///
/// Anything that can legitimately differ between two chats opened by
/// the same logged-in user — `workspace_root`, conversation history,
/// the per-session `Agent`, the session's tool-registry view, the
/// effective sandbox after a session-level override. Those live on
/// [`super::SessionRuntime`].
///
/// # Lifecycle
///
/// Built once per profile on first use via [`Self::bootstrap`]. Held
/// behind an `Arc` so every [`super::SessionRuntime`] for the profile
/// can cheaply share it.
pub struct ProfileRuntime {
    /// Stable identifier for the profile (matches
    /// `UserProfile::id`). Used as part of the cache key in
    /// [`super::SessionRuntimeCache`] and as the value of
    /// `OCTOS_PROFILE_ID` in plugin spawns.
    pub profile_id: String,

    /// The profile's data directory, conventionally
    /// `~/.octos/profiles/<profile_id>/data`. Resolved by the caller
    /// and passed into [`Self::bootstrap`]; held here so sessions and
    /// session-scope bootstrap code don't have to re-derive it.
    pub data_dir: PathBuf,

    /// Optional local-frontend transcript root. Ephemeral chat keeps profile
    /// memory/tools rooted at `data_dir`, while session JSONL and context/task
    /// sidecars use this temporary directory even with per-cwd storage enabled.
    /// Ordinary Serve/Gateway/ACP runtimes leave this unset.
    pub session_store_root: Option<PathBuf>,

    /// The profile's resolved [`crate::config::Config`] (as produced by
    /// `config_from_profile` at bootstrap, with host memory/plugins merged).
    /// Most runtime state is pre-extracted into the typed fields below; this
    /// is retained for the few paths that must resolve a lane provider
    /// LAZILY from `config.sub_providers` (with the profile's credential /
    /// timeout config), e.g. a peer session that runs its turns on a named
    /// `sub_provider` model lane (`peers/<slug>/model`). Kept whole rather
    /// than re-deriving a `Config` off disk on the hot path.
    pub config: crate::config::Config,

    /// The fully-wrapped LLM provider chain for this profile.
    /// Includes retry and provider failover. Every session for this
    /// profile uses this same provider.
    pub llm: Arc<dyn LlmProvider>,

    /// Materialized runtime QoS catalog produced alongside the
    /// provider chain — a cold-start catalog derived from
    /// `model_catalog.json` (context-window / pricing seed).
    pub runtime_qos_catalog: Option<QosCatalog>,

    /// The primary (base) provider's `model_id()` *before* the
    /// retry / failover wrapping is applied.
    /// Gateway uses this for `resolve_provider_policy(..., model_id)`
    /// and as the `primary_key` of the sub-provider router's
    /// fallback ranking.
    pub primary_model_id: String,

    /// The active provider family name (e.g. `kimi`, `deepseek`,
    /// `r9s`). Captured at bootstrap time so gateway can derive its
    /// per-provider tool policy and synthesis config without
    /// re-running provider detection.
    pub provider_name: String,

    /// Resolved credentials for this profile, keyed by env-var name
    /// (e.g. `OPENAI_API_KEY`, `AUTODL_API_KEY`). Populated from
    /// `profile.config.env_vars` via the keychain resolver. Sessions
    /// read this when spawning MCP servers, plugins, and shell tools
    /// that need the profile's API keys.
    pub credentials: HashMap<String, String>,

    /// Path to the per-profile skills directory if one exists
    /// (`<data_dir>/skills/`). `None` when the profile has no
    /// dashboard-installed skills.
    pub skills_dir: Option<PathBuf>,

    /// The profile's tool policy (allow/deny lists, named groups,
    /// per-provider overrides). `None` means "no profile-level policy"
    /// — the agent's default permissions apply.
    pub tool_policy: Option<ToolPolicy>,

    /// The default sandbox config sessions inherit. Sessions may
    /// override (e.g. a slides-builder session wants
    /// `no-network`); when they don't, the runtime falls back to
    /// this value.
    pub default_sandbox: SandboxConfig,

    /// Configured agent iteration budget (`config.max_iterations`) that
    /// sessions — and the sub-agents they spawn — inherit. `None` falls back
    /// to [`AgentConfig`]'s default. Captured here so the session runtime
    /// honors the configured value instead of a hardcoded cap (which silently
    /// starved spawned sub-agents doing multi-step work).
    pub max_iterations: Option<u32>,

    /// Local frontend overrides applied by the canonical session bootstrap.
    /// OUP still owns per-turn intent, context, persistence and cancellation.
    pub session_defaults: Option<octos_agent::AgentConfig>,
    /// Optional operator-selected coding tool/agent profile (chat and ACP).
    pub agent_profile: Option<Arc<octos_agent::profile::ProfileDefinition>>,

    /// Post-edit formatting opt-in (`config.format_after_edit`, issue
    /// #1774) that per-session agents inherit. When true, successful
    /// `edit_file` / `write_file` / `diff_edit` calls run the file's
    /// language formatter and echo the formatted content in the tool
    /// result. Default: false.
    pub format_after_edit: bool,

    /// #1768: opt-in workspace-snapshot config per-session agents use to
    /// build their `SnapshotManager` (None/disabled = no snapshots).
    pub snapshots: Option<octos_agent::SnapshotConfig>,

    /// The base [`ToolRegistry`] template — builtins + plugins +
    /// MCP agents + the LRU pin set — but **NOT** workspace-bound.
    /// Sessions clone this and call `with_workspace_root` to obtain
    /// a workspace-bound registry.
    pub tool_specs: Arc<ToolRegistry>,

    /// Fully pre-assembled system prompt for this profile. Built once
    /// at bootstrap by calling [`build_system_prompt`] (the gateway's
    /// canonical assembler) and then appending every fragment in
    /// [`Self::plugin_prompt_fragments`]. Every [`super::SessionRuntime`]
    /// bootstrapped from this profile copies the value onto its
    /// per-session [`octos_agent::Agent`] via
    /// [`octos_agent::Agent::with_system_prompt`]. This is the M11-F
    /// regression fix (#891) — the previous serve-mode
    /// `try_create_agent` helper called the same build + append loop
    /// inline, but M11-F deleted that helper and routed everything
    /// through [`super::SessionRuntime::bootstrap`], which never
    /// re-derived the prompt. The result was that SKILL.md auto-
    /// injected guidance (e.g. the mofa-fm "call fm_tts directly"
    /// note) never reached the LLM on `/api/chat` or the UI Protocol
    /// WebSocket path. Pre-assembling once on `ProfileRuntime` keeps
    /// the heavy work (memory context, skills summary, bootstrap
    /// files) off the per-request hot path.
    pub system_prompt: String,
    /// The same prompt split at the memory slot — per-session agents
    /// compose `pre → [memory segment] → post` to keep the pre-refactor
    /// precedence (memory before skills/tool guidance).
    pub prompt_parts: crate::commands::gateway::prompt::GatewayPromptParts,

    /// Phase 4 (docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md): per-profile
    /// human-approval rules, converted once at bootstrap and inherited by
    /// every per-session Agent this profile spawns.
    pub human_approval_rules: Option<octos_agent::HumanApprovalRules>,

    /// Long-lived [`EpisodeStore`] for this profile (redb at
    /// `<data_dir>/episodes.redb`). Shared across all sessions of
    /// the profile so task summaries written in one session are
    /// recallable from another.
    pub memory: Arc<EpisodeStore>,

    /// Long-lived [`MemoryStore`] (MEMORY.md + daily notes + recent
    /// memories window) for this profile.
    pub memory_store: Arc<MemoryStore>,

    /// The profile's embedding provider (None when no `embedding`
    /// config and no resolvable key). Sessions hand this to
    /// SpawnTool / DelegateTool so worker agents embed the episodes
    /// they save and run hybrid scored+filtered recall — without it
    /// workers stored episodes vectorless and recall silently skipped.
    pub embedder: Option<Arc<dyn octos_llm::EmbeddingProvider>>,
    /// Resolved `memory.max_inject_tokens` for per-session memory segments.
    pub memory_inject_tokens: usize,
    /// Resolved `memory.refresh.enabled` — gates the capture-policy text in
    /// the memory segment and the per-turn refresh provider.
    pub memory_refresh_enabled: bool,

    /// Pre-built lifecycle hook executor (M11-F regression fix REG-3).
    ///
    /// Pre-M11-F `serve.rs::try_create_agent` merged `config.hooks +
    /// plugin_result.hooks` and called `agent.with_hooks(Arc::new(
    /// HookExecutor::new(all_hooks)))`. M11-F lost that wiring on every
    /// per-session agent build. We assemble the executor once at
    /// profile-bootstrap time and propagate it onto every per-session
    /// [`octos_agent::Agent`] (via [`super::SessionRuntime::bootstrap`]'s
    /// `with_hooks`) AND onto the request-rebuilt agents in both
    /// `ws_standalone_agent` and the UI Protocol per-turn rebuild
    /// loop. `None` keeps the legacy behaviour when no hooks are
    /// configured (the agent's default `hooks: None` field).
    pub hook_executor: Option<Arc<HookExecutor>>,
}

/// Which OS process is calling [`ProfileRuntime::bootstrap`].
///
/// Used to decide whether [`EpisodeStore::open`] should fail loudly
/// on redb lock contention (the canonical owner — `Serve`) or degrade
/// gracefully (the companion process — `Gateway`).
///
/// See the type-level docs on
/// [`octos_memory::EpisodeStore`](EpisodeStore) for why the role
/// split exists: redb is single-writer-single-process, and `octos
/// serve` + `octos gateway` are separate OS processes that both
/// bootstrap the same profile. Serve owns the canonical store;
/// gateway is allowed to degrade so channel polling stays alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapRole {
    /// Caller owns the canonical EpisodeStore. `EpisodeStore::open`
    /// runs in strict mode and fails if the redb file lock is already
    /// held. Use this from `octos serve` and other entry points whose
    /// correctness depends on persistence being intact.
    Serve,
    /// Caller is a companion process that should keep running even
    /// when the canonical EpisodeStore is owned elsewhere.
    /// `EpisodeStore::open_or_degraded` runs and silently installs a
    /// no-op store on lock contention. Use this from
    /// `octos gateway` subprocesses.
    Gateway,
}

impl ProfileRuntime {
    /// Reapply the effective envelope after cwd rebinding or dynamic tool
    /// registration. A cloned registry must never resurrect excluded tools.
    pub(crate) fn apply_tool_envelope(&self, tools: &mut ToolRegistry) {
        if let Some(policy) = &self.tool_policy {
            tools.apply_policy(policy);
        }
        if let Some(profile) = &self.agent_profile {
            tools.filter_by_profile(&profile.tools);
        }
    }

    /// Build a fully populated [`ProfileRuntime`] from a parsed
    /// [`UserProfile`] + the per-profile `data_dir`.
    ///
    /// Self-contained: this is the M11-D consolidation point that
    /// both `octos serve` and `octos gateway` call as their single
    /// per-profile assembler. The function:
    ///
    /// 1. Derives a [`crate::config::Config`] from the profile via
    ///    [`config_from_profile`].
    /// 2. Builds the LLM provider chain via
    ///    [`chat::create_provider`] + [`build_provider_chain`].
    /// 3. Opens [`EpisodeStore`] + [`MemoryStore`] against `data_dir`.
    /// 4. Constructs the base [`ToolRegistry`] (builtins + WebSearch
    ///    with profile keys + browser w/ profile-config timeout + MCP +
    ///    plugins via [`PluginLoader::load_into_with_options`] with
    ///    the profile's plugin env template).
    /// 6. Pins plugin tool names as base (LRU-defense — PR #764).
    /// 7. Applies profile-scope `tool_policy`.
    ///
    /// # Parameters
    ///
    /// - `profile` — the parsed [`UserProfile`] from the profile
    ///   store; drives the per-profile derivations.
    /// - `data_dir` — the resolved per-profile data dir, typically
    ///   `~/.octos/profiles/<id>/data`.
    /// - `octos_home` — the host's `~/.octos` (or `--octos-home`
    ///   override). Used to seed `OCTOS_HOME` in
    ///   `plugin_env_template`; defaults to `data_dir` when `None`.
    ///
    /// # Errors
    ///
    /// Returns an error when the LLM provider construction fails
    /// (typically a missing API key), when the redb episode store
    /// cannot open, or when the tool config store cannot be opened.
    /// Plugin / MCP loading failures are logged at `warn` and do not
    /// fail bootstrap (the profile still serves with builtins only).
    pub async fn bootstrap(
        profile: &UserProfile,
        data_dir: &Path,
        octos_home: Option<&Path>,
        role: BootstrapRole,
    ) -> Result<Arc<Self>> {
        Self::bootstrap_with_host_memory(profile, data_dir, octos_home, role, None).await
    }

    /// Bootstrap a profile runtime while letting the host merge its
    /// memory settings. Host memory settings apply field-by-field when
    /// the profile doesn't override them. A profile serialized with an
    /// empty `memory: {}` block must still inherit the host budget.
    pub async fn bootstrap_with_host_memory(
        profile: &UserProfile,
        data_dir: &Path,
        octos_home: Option<&Path>,
        role: BootstrapRole,
        host_memory: Option<&crate::config::MemoryConfig>,
    ) -> Result<Arc<Self>> {
        let mut config = config_from_profile(profile);
        crate::config::merge_host_memory_into_profile(&mut config.memory, host_memory);

        Self::bootstrap_resolved(profile, data_dir, octos_home, role, config, false, None).await
    }

    /// Local OUP adapters use the same assembler with their already-resolved
    /// CLI config. Do not round-trip this through ProfileConfig: doing so loses
    /// custom endpoints, API styles and explicit CLI policy overrides.
    /// A supplied provider is an embedding seam, not a second runtime path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn bootstrap_resolved(
        profile: &UserProfile,
        data_dir: &Path,
        octos_home: Option<&Path>,
        role: BootstrapRole,
        config: Config,
        no_retry: bool,
        provider_override: Option<Arc<dyn LlmProvider>>,
    ) -> Result<Arc<Self>> {
        // Step 2: resolve the provider name. `config_from_profile`
        // populates `provider`/`model` from `llm.primary` when set,
        // else falls back to `detect_provider(model)`.
        let model = config.model.clone();
        let base_url = config.base_url.clone();
        let provider_name = config
            .provider
            .clone()
            .or_else(|| {
                model
                    .as_deref()
                    .and_then(crate::config::detect_provider)
                    .map(String::from)
            })
            .ok_or_else(|| {
                eyre::eyre!("profile '{}' has no LLM provider configured", profile.id)
            })?;

        // Step 3: build the LLM provider chain.
        let base_provider = match provider_override {
            Some(provider) => provider,
            None => chat::create_provider(&provider_name, &config, model, base_url).wrap_err_with(
                || format!("failed to create LLM provider for profile '{}'", profile.id),
            )?,
        };
        let primary_model_id = base_provider.model_id().to_string();
        let bundle = build_provider_chain(base_provider, &config, data_dir, no_retry);
        let llm = bundle.llm.clone();
        let runtime_qos_catalog = bundle.runtime_qos_catalog.clone();

        // Step 4: open the memory stores.
        //
        // The opener variant depends on the caller's role (see
        // [`BootstrapRole`] docs). Serve must hold the canonical
        // EpisodeStore; gateway falls back to a degraded handle when
        // serve already owns the redb lock so it doesn't crashloop on
        // every startup. Tracked by issue #899.
        //
        // The embedder is resolved FIRST because the episodic HNSW index is
        // built at one fixed width and silently drops any vector of a
        // different length. Sizing it from the configured provider is what
        // makes a non-1536-d embedder (e.g. in-process EmbeddingGemma at 768)
        // actually reach the vector lane instead of degrading to BM25-only.
        let embedder =
            chat::create_embedder(&config).map(|e| e as Arc<dyn octos_llm::EmbeddingProvider>);
        let index_dimension = embedder
            .as_ref()
            .map_or(octos_memory::EPISODIC_INDEX_DIMENSION, |e| e.dimension());

        let memory_open_result = match role {
            BootstrapRole::Serve => {
                EpisodeStore::open_with_dimension(data_dir, index_dimension).await
            }
            BootstrapRole::Gateway => {
                EpisodeStore::open_or_degraded_with_dimension(data_dir, index_dimension).await
            }
        };
        let memory = Arc::new(memory_open_result.wrap_err_with(|| {
            format!("failed to open episode store for profile '{}'", profile.id)
        })?);
        let memory_store = Arc::new(MemoryStore::open(data_dir).await.wrap_err_with(|| {
            format!("failed to open memory store for profile '{}'", profile.id)
        })?);

        // Step 6: resolve credentials from the profile's declared env
        // vars (keychain-aware). Used by MCP, plugin spawns, and the
        // shell tool when a profile-scoped env var is referenced.
        let credentials = crate::auth::keychain::resolve_env_vars(&profile.config.env_vars);

        // Step 7: discover the per-profile skills dir (if any).
        let skills_dir_candidate = data_dir.join("skills");
        let skills_dir = skills_dir_candidate
            .exists()
            .then_some(skills_dir_candidate);

        let effective_octos_home = octos_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| data_dir.to_path_buf());

        // Step 9: build the base ToolRegistry.
        //
        // Sandbox config is profile-derived. We augment
        // `read_allow_paths` with the octos home so the shell sandbox
        // can read shared skills/configs (mirrors gateway's existing
        // setup).
        let mut sandbox_config = config.sandbox.clone();
        if sandbox_config.read_allow_paths.is_empty() {
            sandbox_config
                .read_allow_paths
                .push(effective_octos_home.to_string_lossy().into_owned());
        }
        let default_sandbox = sandbox_config.clone();
        let sandbox = create_sandbox(&sandbox_config);
        // We register against `data_dir` rather than a real workspace
        // root — sessions rebind cwd via `SessionRuntime::bootstrap`
        // before any actual tool call runs.
        let mut tools = ToolRegistry::with_builtins_and_sandbox(data_dir, sandbox);
        tools.set_output_dir_hint(data_dir.join("skill-output").to_string_lossy().into_owned());

        // Step 12: MCP servers from the profile's config (typically
        // empty for profile-only deployments; gateway / serve top-
        // level configs may add more on top).
        if !config.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&config.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => warn!(profile_id = %profile.id, error = %e, "MCP initialization failed"),
            }
        }
        // --- Skill layering v1 ---
        // Resolve the profile's inherited skill-selection layer (parent +
        // global defaults already merged by `resolve_runtime_profile`) into a
        // crate-agnostic filter handed to BOTH the plugin loader (tool specs)
        // and the SkillsLoader (prompt / content injection) below. `None` ⇒ no
        // skills layer ⇒ every discovered skill loads, exactly as before.
        let skill_filter = profile.config.skills.as_ref().map(|s| s.to_agent_filter());
        if profile.config.skills.is_some() {
            let discovered_skill_ids: Vec<String> = build_account_skills_loader(data_dir)
                .list_skills()
                .await
                .map(|skills| skills.into_iter().map(|s| s.name).collect())
                .unwrap_or_default();
            let catalog =
                crate::skills_scope::resolve_profile_skills(profile, &discovered_skill_ids);
            if catalog.has_disabled() {
                info!(
                    profile_id = %profile.id,
                    mode = ?catalog.mode,
                    disabled = ?catalog.disabled,
                    "skill layering: installed skills disabled by profile config"
                );
            }
        }
        // RFC-0 (#1289): LRU tool deferral was removed — the base-tool pin
        // list is no longer needed; every enabled tool is emitted every turn.

        // Memory bank tools — registered profile-side so every
        // session inherits the same memory_store.
        tools.register(octos_agent::RecallMemoryTool::new(memory_store.clone()));
        tools.register(octos_agent::SaveMemoryTool::new(memory_store.clone()));
        tools.register(octos_agent::RecordMemoryUseTool::new(memory_store.clone()));
        if crate::config::MemoryConfig::refresh_enabled(config.memory.as_ref()) {
            tools.register(octos_agent::MemoryNoteTool::new(memory_store.clone()));
        }

        // Step 17: re-apply tool policy AFTER plugin / memory-bank
        // registration so deny entries can target plugin-declared
        // tool names too (PR #688 follow-up — MEDIUM #4).
        if let Some(ref policy) = config.tool_policy {
            tools.apply_policy(policy);
        }

        // `serve --stdio --solo` is the headless coding transport used by
        // ARC-Bench. Apply the same built-in allow-list as
        // `chat --profile coding`, including to profiles created after serve
        // startup (the lazy runtime path checks this same process setting).
        let agent_profile = if stdio_solo_lean_defaults_enabled() {
            let (profile, _) = octos_agent::profile::ProfileDefinition::load("coding")
                .wrap_err("failed to load built-in coding profile for stdio/solo")?;
            profile.apply_to_registry(&mut tools);
            Some(Arc::new(profile))
        } else {
            None
        };

        // RFC-0 (#1289): LRU tool deferral + the `activate_tools` meta-tool
        // were removed. Every enabled tool is now emitted every turn (full
        // schema), so the former auto-defer-non-core-groups pass is gone.

        // Step 18: pre-assemble the profile-scope system prompt.
        //
        // This is the M11-F regression fix (#891). Before M11-F, serve
        // mode's `try_create_agent` helper called `build_system_prompt`
        // + the fragment-append loop inline, so every per-request agent
        // observed the SKILL.md guidance. M11-F deleted that helper and
        // routed everything through `SessionRuntime::bootstrap`, which
        // never re-derived the prompt — meaning `/api/chat` and the UI
        // Protocol WS path lost the mofa-fm "call fm_tts directly"
        // teaching (and any future skill-injected guidance).
        //
        // We assemble once per profile and stash it on the runtime so
        // every `SessionRuntime` bootstrapped from this profile inherits
        // the same prompt onto its per-session `Agent`.
        //
        // `project_dir` is `data_dir` in serve mode. The bootstrap-files
        // assembly (`load_bootstrap_files`) reads AGENTS.md / SOUL.md /
        // USER.md from this dir — gateway uses its `--cwd` / project
        // dir, but serve mode has no project_dir concept, and the
        // per-profile data dir is the only profile-scoped directory we
        // can hand to the helper. Operators who want per-profile
        // bootstrap files drop them in `<data_dir>/`, which matches the
        // pre-M11-F serve-mode behavior.
        let skills_loader = build_account_skills_loader(data_dir).with_skill_filter(skill_filter);
        let max_inject_tokens =
            crate::config::MemoryConfig::effective_max_inject_tokens(config.memory.as_ref());
        let memory_refresh_enabled =
            crate::config::MemoryConfig::refresh_enabled(config.memory.as_ref());
        let prompt_parts = build_system_prompt(
            profile.config.gateway.system_prompt.as_deref(),
            data_dir,
            data_dir,
            &skills_loader,
        )
        .await;
        let system_prompt = prompt_parts.joined();
        let prompt_parts_for_runtime = prompt_parts.clone();

        // M11-F regression fix REG-3: assemble the lifecycle hook
        // executor once per profile and propagate the `Arc` onto every
        // per-session [`octos_agent::Agent`].
        //
        // Pre-M11-F `serve.rs::try_create_agent` merged `config.hooks +
        // plugin_result.hooks` into `Vec<HookConfig>`, wrapped it in
        // `HookExecutor::new`, and called `agent.with_hooks(...)`. M11-F
        // stored `plugin_hooks` on `ProfileRuntime` but never built the
        // executor or attached it. We do both here so the
        // `before_tool_call` / `after_tool_call` / `before_llm_call` /
        // `after_llm_call` hooks fire on the api-mode agent the same
        // way they fire under `octos gateway`.
        //
        // `SessionRuntime::bootstrap` reads this back and chains
        // `.with_hooks(executor.clone())` onto the per-session agent;
        // the per-request rebuild paths in `ws_standalone_agent` and
        // the UI Protocol per-turn builder do the same. Storing as
        // `Option<Arc<HookExecutor>>` preserves the pre-M11-F default
        // when neither source carries any hooks (the agent's
        // `hooks: None` field remains untouched).
        // #2129: the coding default hooks (cargo check / eslint / ruff after
        // edits) merge at THIS shared assembly point so every host —
        // bootstrap sessions, WS per-turn rebuilds, chat, gateway — gets
        // them, not just one consumer. They are SELF-GATING: each declares a
        // path_filter (fires only on matching source edits) and requires_bin
        // (skips when the checker is absent), so a podcast workspace never
        // runs cargo. Defaults first, operator hooks after, per the
        // coding_default_hooks contract. The hook child's working directory
        // comes from the per-turn payload cwd (the workspace root), not the
        // executor, so one profile-level executor serves every session.
        let mut all_hooks = octos_agent::workspace_policy::coding_default_hooks();
        all_hooks.extend(config.hooks.clone());
        // #2153 finding 2: coalesce a burst of edits so a whole-project
        // `cargo check` (up to its 60s timeout) does not run once per edit.
        // The window is measured from the previous check's completion, so
        // several `edit_file` calls in one assistant turn collapse to a single
        // check while a later edit (a new thinking step) still gets a fresh
        // one. Breaker + debounce state are per session (see HookExecutor).
        let hook_executor = Some(Arc::new(
            HookExecutor::new(all_hooks)
                .with_after_event_debounce(std::time::Duration::from_millis(2000)),
        ));

        info!(
            profile_id = %profile.id,
            provider = %provider_name,
            model = %primary_model_id,
            tool_count = tools.specs().len(),
            system_prompt_len = system_prompt.len(),
            hook_count = hook_executor.is_some() as u8,
            "ProfileRuntime: bootstrapped"
        );

        // Validate the per-profile approval policy with the SAME checks the
        // top-level config load applies, so a bad profile rule fails fast
        // instead of gating unexpectedly / creating unanswerable or
        // instantly-expiring requests (review finding #4).
        if let Some(policy) = profile.config.approval_policy.as_ref() {
            policy
                .validate()
                .wrap_err("invalid profile approval_policy")?;
        }

        Ok(Arc::new(Self {
            profile_id: profile.id.clone(),
            data_dir: data_dir.to_path_buf(),
            session_store_root: None,
            // Retained whole for lazy per-lane provider resolution (e.g. a
            // peer running on a named `sub_provider` model lane); the typed
            // fields below carry the pre-extracted hot-path state.
            config: config.clone(),
            llm,
            runtime_qos_catalog,
            primary_model_id,
            provider_name,
            credentials,
            skills_dir,
            tool_policy: config.tool_policy.clone(),
            default_sandbox,
            max_iterations: config.max_iterations,
            session_defaults: None,
            agent_profile,
            format_after_edit: config.format_after_edit,
            snapshots: config.snapshots.clone(),
            tool_specs: Arc::new(tools),
            human_approval_rules: profile
                .config
                .approval_policy
                .as_ref()
                .map(|policy| policy.to_runtime_rules()),
            system_prompt,
            prompt_parts: prompt_parts_for_runtime,
            memory_inject_tokens: max_inject_tokens,
            memory_refresh_enabled,
            memory,
            memory_store,
            embedder,
            hook_executor,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::profiles::{
        GatewaySettings, LlmModelSelectionConfig, LlmProfileConfig, LlmRouteConfig, ProfileConfig,
    };

    use chrono::Utc;

    /// Build a minimal `UserProfile` with no LLM contract. M11-D
    /// bootstrap must reject this with a clear error, not panic.
    #[tokio::test]
    async fn bootstrap_errors_when_profile_has_no_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profiles").join("test").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let profile = UserProfile {
            id: "no-llm".to_string(),
            name: "No LLM".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let err = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .err()
            .expect("bootstrap must fail without a provider");
        assert!(
            err.to_string().contains("no LLM provider configured"),
            "unexpected error: {err}",
        );
    }

    /// M11 regression fix (#891): `ProfileRuntime::bootstrap` must
    /// pre-assemble the full system prompt so every `SessionRuntime`
    /// built from it observes the SKILL.md prompt fragments. Without
    /// this, `/api/chat` and the UI Protocol WS path miss the
    /// mofa-fm SKILL.md (and any future skill-injected guidance) and
    /// the LLM falls back to its prior over the bare tool list.
    ///
    /// Fixture: a single skill (no executable required — the loader's
    /// "extras-only" path handles manifests with empty tools) that
    /// declares `prompts.include = ["SKILL.md"]` and ships a SKILL.md
    /// with a recognizable token. We then bootstrap a profile pointing
    /// at this skills dir and assert the token surfaces on
    /// `ProfileRuntime::system_prompt`.
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn profile_runtime_bootstrap_includes_skill_prompt_fragments() {
        // Uniquely-named env var to avoid contention with other tests.
        const KEY_NAME: &str = "OCTOS_M11_891_TEST_API_KEY";
        // SAFETY: this env var name is unique to this test; nothing
        // else in the test suite reads or writes it. We also unset it
        // on the way out via the guard below.
        unsafe {
            std::env::set_var(KEY_NAME, "test-key-sk-fake");
        }
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: see set_var above.
                unsafe {
                    std::env::remove_var(KEY_NAME);
                }
            }
        }
        let _guard = EnvGuard;

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profiles").join("test").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Plant a fixture skill with a recognizable token in SKILL.md.
        let skills_dir = data_dir.join("skills").join("test-fragment-skill");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("manifest.json"),
            r#"{
                "name": "test-fragment-skill",
                "version": "1.0.0",
                "tools": [],
                "prompts": { "include": ["SKILL.md"] }
            }"#,
        )
        .unwrap();
        // Binary plugin retirement: prompt injection now flows exclusively
        // through the SKILL.md `always: true` frontmatter (skills.rs loader)
        // instead of manifest `prompts.include` fragments.
        std::fs::write(
            skills_dir.join("SKILL.md"),
            "---\nname: test-fragment-skill\ndescription: test\nalways: true\n---\n\n## Test Fragment Skill\n\nMARKER-FRAGMENT-XYZ — call fm_tts directly.\n",
        )
        .unwrap();

        let profile = UserProfile {
            id: "with-skill".to_string(),
            name: "With Skill".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                llm: Some(LlmProfileConfig {
                    primary: Some(LlmModelSelectionConfig {
                        family_id: Some("openai".to_string()),
                        model_id: Some("gpt-4o-mini".to_string()),
                        route: Some(LlmRouteConfig {
                            route_id: None,
                            label: None,
                            base_url: None,
                            api_key_env: Some(KEY_NAME.to_string()),
                            api_type: None,
                        }),
                        ..Default::default()
                    }),
                    fallbacks: Vec::new(),
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let rt = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .expect("bootstrap should succeed with a valid provider config");

        assert!(
            rt.system_prompt.contains("MARKER-FRAGMENT-XYZ"),
            "system_prompt should contain SKILL.md fragment; got: {}",
            rt.system_prompt
        );
        // Sanity: it's not _only_ the fragment — base prompt content
        // (e.g. the date marker injected by `build_system_prompt`)
        // should also be present.
        assert!(
            rt.system_prompt.contains("Current date:"),
            "system_prompt should also contain the base prompt body; got: {}",
            rt.system_prompt
        );
    }

    /// Regression test for the M11-F production crashloop tracked in
    /// `octos-org/octos#899`:
    ///
    /// `octos serve` and `octos gateway` are separate OS processes,
    /// both calling `ProfileRuntime::bootstrap` against the same
    /// per-profile data dir. Before this fix the second bootstrap
    /// crashed inside `EpisodeStore::open` with
    /// `redb::DatabaseError::DatabaseAlreadyOpen`, gateway exited,
    /// launchd auto-restarted it, and every profile crashlooped every
    /// ~2 seconds. Now the second bootstrap must succeed with the
    /// EpisodeStore in degraded mode.
    ///
    /// We simulate the cross-process race by bootstrapping the same
    /// profile twice in a row in the same test — the first handle on
    /// `rt_owner.memory` keeps the redb lock held while the second
    /// `ProfileRuntime::bootstrap` call runs, exercising the same
    /// `DatabaseAlreadyOpen` path the gateway subprocess hits in
    /// production.
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn bootstrap_succeeds_when_redb_already_owned_by_sibling_process() {
        const KEY_NAME: &str = "OCTOS_GH899_TEST_API_KEY";
        // SAFETY: env var name is unique to this test.
        unsafe {
            std::env::set_var(KEY_NAME, "test-key-sk-fake");
        }
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: see set_var above.
                unsafe {
                    std::env::remove_var(KEY_NAME);
                }
            }
        }
        let _guard = EnvGuard;

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profiles").join("gh899").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let profile = UserProfile {
            id: "gh899".to_string(),
            name: "GH899".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                llm: Some(LlmProfileConfig {
                    primary: Some(LlmModelSelectionConfig {
                        family_id: Some("openai".to_string()),
                        model_id: Some("gpt-4o-mini".to_string()),
                        route: Some(LlmRouteConfig {
                            route_id: None,
                            label: None,
                            base_url: None,
                            api_key_env: Some(KEY_NAME.to_string()),
                            api_type: None,
                        }),
                        ..Default::default()
                    }),
                    fallbacks: Vec::new(),
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Simulates `octos serve`: bootstraps first as `Serve`,
        // takes the redb lock. `rt_owner` stays live for the whole
        // test so the lock remains held.
        let rt_owner = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .expect("first bootstrap (owner) should succeed");
        assert!(
            !rt_owner.memory.is_degraded(),
            "first bootstrap should hold the canonical redb",
        );

        // Simulates `octos gateway` running as a subprocess of serve:
        // hits the lock contention. Before #899 this returned
        // `Err(failed to open episode store ... Database already open)`.
        // Now it must succeed because the `Gateway` role opts into
        // the degraded fallback; the resulting handle's EpisodeStore
        // operates in degraded mode.
        let rt_sibling =
            ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Gateway)
                .await
                .expect(
                    "second bootstrap (Gateway role) should succeed even \
                     when redb is already locked — this is the crashloop \
                     fix from issue #899",
                );
        assert!(
            rt_sibling.memory.is_degraded(),
            "Gateway-role bootstrap's episode store must be degraded",
        );
    }
}
