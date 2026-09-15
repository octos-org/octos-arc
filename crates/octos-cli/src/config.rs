//! Configuration file support for octos CLI.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

/// Current config version.
const CURRENT_CONFIG_VERSION: u32 = 1;

/// Deployment mode determines how octos serve behaves.
/// LLM provider configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    /// Config version for migration.
    #[serde(default)]
    pub version: Option<u32>,

    /// LLM provider: "anthropic", "openai", or "gemini".
    #[serde(default)]
    pub provider: Option<String>,

    /// Model name.
    #[serde(default)]
    pub model: Option<String>,

    /// Operator override for the primary provider's effective context window,
    /// in tokens. When set it wraps the (probed) primary provider in
    /// `ContextWindowOverride` as the OUTERMOST layer, so it beats both the
    /// static catalog and the runtime probe (#2135). Projected from
    /// `LlmModelSelectionConfig.context_window` by `config_from_profile`.
    /// `None` = defer to probe/catalog. (#2142)
    #[serde(default)]
    pub context_window: Option<u32>,

    /// #2166: the configured PRIMARY model's typed inference defaults,
    /// flattened out of `LlmModelSelectionConfig` by `config_from_profile`
    /// so session bootstrap can compose them AHEAD of the profile-gateway
    /// knobs (`gateway.llm_temperature` / `gateway.reasoning_effort` /
    /// `gateway.llm_sampling_params`). Ownership stays with the durable
    /// model selection; these are read-only projections. `None` = inherit.
    /// Range validation lives on the AppUI wire schema (#2166).
    #[serde(default)]
    pub model_temperature: Option<f32>,
    /// #2166: primary model default `top_p`. At runtime it overrides a
    /// same-named `top_p` key in `gateway.llm_sampling_params` (#2176);
    /// every other sampler key in that map is untouched.
    #[serde(default)]
    pub model_top_p: Option<f32>,
    /// #2166: primary model default reasoning effort. Precedence:
    /// session/turn override → this → `gateway.reasoning_effort` → none.
    #[serde(default)]
    pub model_reasoning_effort: Option<octos_llm::ReasoningEffort>,

    /// Custom base URL for the API endpoint.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Environment variable name for API key (default: ANTHROPIC_API_KEY, OPENAI_API_KEY, or GEMINI_API_KEY).
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Profile-scoped environment values, including API keys persisted by the dashboard/AppUI.
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,

    /// When true, [`Config::get_api_key`] skips the global `AuthStore` lookup so
    /// an explicitly-supplied key (e.g. the `env_vars`-injected key the
    /// `octos-ffi` embedding API passes) is authoritative and cannot be silently
    /// shadowed by a host's `octos auth login` credentials for the same
    /// provider. Internal, not (de)serialized; default `false` preserves the
    /// CLI / gateway resolution order.
    #[serde(skip)]
    pub bypass_auth_store: bool,

    /// Override auto-detected model behavior hints for the OpenAI provider.
    /// Useful for custom/unknown models behind OpenAI-compatible proxies.
    #[serde(default)]
    pub model_hints: Option<octos_llm::openai::ModelHints>,

    /// API protocol type: "openai" (default) or "anthropic".
    /// When set to "anthropic", the Anthropic Messages API format is used
    /// regardless of the provider name (for Anthropic-compatible proxies).
    #[serde(default)]
    pub api_type: Option<String>,

    /// Admin auth token (for dashboard login). Also settable via --auth-token CLI arg
    /// or OCTOS_AUTH_TOKEN env var.
    #[serde(default)]
    pub auth_token: Option<String>,

    /// Gateway configuration (optional).
    #[serde(default)]
    pub gateway: Option<GatewayConfig>,

    /// MCP server configurations.
    #[serde(default)]
    pub mcp_servers: Vec<octos_agent::McpServerConfig>,

    /// Sandbox configuration.
    #[serde(default)]
    pub sandbox: octos_agent::SandboxConfig,

    /// Workspace snapshot-undo configuration (#1768). Opt-in: when
    /// `snapshots.enabled` is true, the agent records a git-backed
    /// snapshot of the workspace (into a separate git dir under
    /// `<data_dir>/snapshots/`, never the user's own `.git`) before each
    /// mutating tool batch. Absent or `enabled: false` (the default) =
    /// feature off.
    #[serde(default)]
    pub snapshots: Option<octos_agent::SnapshotConfig>,

    /// Build-cache pool configuration (outer-loop #1–#3; design
    /// docs/build-cache-pool.md §2). Optional like `snapshots`: absent
    /// means defaults (2 peer slots + 1 verify slot per repository, a
    /// 50 GB free-space gate, 168 h stale window). Peers and outer-loop
    /// verification draw cargo target dirs from this pool instead of each
    /// growing an unbounded `target/`.
    #[serde(default)]
    pub build_cache: Option<crate::build_cache::BuildCacheConfig>,

    /// Tool access policy (allow/deny lists with group and wildcard support).
    #[serde(default)]
    pub tool_policy: Option<octos_agent::ToolPolicy>,

    /// Per-provider tool policies. Key = model ID or provider name prefix.
    /// Example: `{"gemini": {"deny": ["diff_edit"]}}`.
    #[serde(default)]
    pub tool_policy_by_provider: std::collections::HashMap<String, octos_agent::ToolPolicy>,

    /// Embedding configuration for hybrid memory search.
    #[serde(default)]
    pub embedding: Option<EmbeddingConfig>,

    /// Memory subsystem configuration.
    #[serde(default)]
    pub memory: Option<MemoryConfig>,

    /// Fallback models for provider failover chain.
    /// When the primary provider fails with a retriable error, the next model is tried.
    #[serde(default)]
    pub fallback_models: Vec<FallbackModel>,

    /// Maximum agent iterations per message (overridden by --max-iterations).
    #[serde(default)]
    pub max_iterations: Option<u32>,

    /// Post-edit formatting (issue #1774): when true, a successful
    /// `edit_file` / `write_file` / `diff_edit` runs the file's language
    /// formatter (rustfmt / prettier / black / gofmt) and returns the
    /// formatted content in the tool result. Formatters run file-scoped with
    /// a sanitized environment and a hard 5s timeout; a missing binary or a
    /// formatter failure never fails the edit. Default: false (opt-in).
    #[serde(default)]
    pub format_after_edit: bool,

    /// Lifecycle hooks for agent events.
    #[serde(default)]
    pub hooks: Vec<octos_agent::HookConfig>,

    /// Human-approval rules for tool calls that require a human decision
    /// before executing (suspend-and-resume flow on gateway channels — see
    /// `docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md`).
    #[serde(default)]
    pub approval_policy: Option<ApprovalPolicyConfig>,

    /// Context-based tool tag filter. When set, only tools matching at least one
    /// tag are visible to the LLM. Example: `["code", "search"]`.
    #[serde(default)]
    pub context_filter: Vec<String>,

    /// Sub-providers available for subagent spawning via the spawn tool.
    /// Each entry registers a provider under a short key that the LLM can reference.
    #[serde(default)]
    pub sub_providers: Vec<SubProviderConfig>,

    /// AppUi (octos-app, octoscode, etc.) session defaults applied by
    /// `octos serve`. Operators can anchor every AppUi session that
    /// does not advertise the `session.workspace_cwd.v1` capability to
    /// a chosen folder via `appui.default_session_cwd` — the Tier-2
    /// fallback consulted by the UI Protocol dispatcher when no
    /// client-supplied cwd is present and before
    /// `SessionRuntime::bootstrap`'s Tier-3 profile-default workspace
    /// root. Capability-gated client-sent cwds (Tier-1) still take
    /// precedence.
    #[serde(default)]
    pub appui: AppUiConfig,

    /// Per-subcommand CLI-flag defaults — the "initial startup config".
    ///
    /// Keys are subcommand names (`serve`, `gateway`, `chat`); each value is a
    /// JSON object of snake_case flag id → default value (e.g.
    /// `{"serve": {"port": 50080, "solo": true}}`). Consulted by the startup
    /// layering ([`crate::config_layer`]) BELOW an explicit CLI flag / env var
    /// but ABOVE the built-in clap default, so operators can persist their
    /// preferred flags without retyping them. Hand-edited in `config.json`.
    ///
    /// Unknown command keys round-trip untouched. The block is empty by default
    /// and omitted from serialization so configs that never used it stay
    /// byte-identical.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub cli: std::collections::BTreeMap<String, serde_json::Value>,
}

/// AppUi session defaults applied by `octos serve`'s API agent.
///
/// All fields have backward-compatible defaults. `allowed_origins` defaults
/// to empty, `default_session_cwd` defaults to `None` (no
/// server-side default cwd; sessions fall through Tier-3 of the
/// `session_tool_registry` chain unchanged), but `sessions_in_cwd` defaults
/// to `true` — see its field doc for the coexistence trade-off — so an
/// absent or empty `[appui]` section now enables per-project session storage
/// for cwd-hinted launches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppUiConfig {
    /// Additional browser origins allowed to call the REST API and open
    /// either UI Protocol WebSocket endpoint.
    ///
    /// Every entry must be an exact `http://` or `https://` origin (scheme,
    /// host, and optional port only). `octos serve` validates and normalizes
    /// the list at startup. `OCTOS_APPUI_ALLOWED_ORIGINS`, when non-empty,
    /// replaces this list with a comma-separated deployment override.
    #[serde(default)]
    pub allowed_origins: Vec<String>,

    /// Optional default workspace cwd for AppUi sessions. When set, every
    /// `session/open` call against this server falls back to this cwd
    /// (Tier-2 of `session_tool_registry`'s fallback chain) when the
    /// client does not advertise `session.workspace_cwd.v1` and send its
    /// own cwd. Capability-gated client-sent cwds (Tier-1) take precedence.
    ///
    /// Use absolute paths. Tilde (`~`) is not expanded — operators who
    /// prefer a home-relative path should resolve it before writing
    /// `config.json`.
    #[serde(default)]
    pub default_session_cwd: Option<PathBuf>,

    /// Relocate AppUi/stdio "coding-agent" session storage from the global
    /// per-profile store (`<data_dir>/sessions/`) to a **per-project** store
    /// at `<cwd>/.octos/sessions/`, so `resume` / `session/list` show the
    /// conversations that belong to the folder the client launched in.
    ///
    /// Scope: only sessions opened with a `cwd`/`workspace_hint` (the
    /// AppUi/coding-agent path). No-hint web-chat and every gateway session
    /// stay on the per-profile store regardless of this flag — the
    /// sessions-root resolver returns `profile.data_dir` when there is no
    /// hint, so per-cwd storage is inert for those paths by construction.
    ///
    /// Default `true`: a bare launch in a folder resumes that folder's own
    /// conversations, which is the launch-flow contract (`launch/resolve` +
    /// the sticky `active-profile` marker record their per-project store).
    /// An operator can still force the legacy global store by setting
    /// `sessions_in_cwd = false`.
    ///
    /// Coexistence trade-off (this flip is NOT a migration): cwd-hinted
    /// coding sessions created before the default flipped live under the old
    /// per-profile store and do NOT appear in a per-project `session/list`
    /// for their folder — their cwd was never persisted, so they cannot be
    /// relocated. They remain reachable via a no-`cwd` `session/list` (which
    /// still resolves to `profile.data_dir`). No-hint web-chat and every
    /// gateway session are unaffected: they never used per-cwd storage, so
    /// flipping the default is inert for those paths by construction.
    #[serde(default = "default_sessions_in_cwd")]
    pub sessions_in_cwd: bool,
}

/// Default for [`AppUiConfig::sessions_in_cwd`] — `true` so a bare launch in
/// a folder resumes that folder's own conversations. Kept in sync with the
/// manual [`Default`] impl below (serde uses this for a present `[appui]`
/// missing the key; `Default` covers an absent `[appui]` section).
fn default_sessions_in_cwd() -> bool {
    true
}

impl Default for AppUiConfig {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            default_session_cwd: None,
            sessions_in_cwd: default_sessions_in_cwd(),
        }
    }
}

/// A fallback model for the provider failover chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FallbackModel {
    /// Provider name (e.g. "openai", "gemini").
    pub provider: String,
    /// Model name.
    #[serde(default)]
    pub model: Option<String>,
    /// Custom base URL.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Override the API key env var for this fallback.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Override auto-detected model hints for this fallback.
    #[serde(default)]
    pub model_hints: Option<octos_llm::openai::ModelHints>,
    /// API protocol type: "openai" or "anthropic". Overrides provider default.
    #[serde(default)]
    pub api_type: Option<String>,
    /// Published output price in USD per million tokens (for cost-aware routing).
    #[serde(default)]
    pub cost_per_m: Option<f64>,
    /// Mark as strong model (reliable with 30+ tools, large payloads).
    /// Used by slides sessions to filter failover candidates.
    /// Defaults to true for backward compat — set false for weak/proxy providers.
    #[serde(default = "default_true")]
    pub strong: bool,
    /// Operator override for THIS fallback's effective context window, in
    /// tokens. Wraps this fallback provider in `ContextWindowOverride`
    /// (outermost) so it beats the catalog and the probe (#2135). Projected
    /// from the per-fallback `LlmModelSelectionConfig.context_window`.
    /// `None` = defer to probe/catalog. (#2142)
    #[serde(default)]
    pub context_window: Option<u32>,
}

pub fn default_true() -> bool {
    true
}

/// Default disposition for tools not matched by any approval rule.
/// v1 supports `allow` only (unmatched tools run without human approval).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicyDefault {
    #[default]
    Allow,
}

/// Severity attached to approval requests (rendered by capable clients).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicyRiskLevel {
    Normal,
    Critical,
}

/// What happens when an approval request expires unanswered.
/// v1 supports `notify` only (a notice is sent to the originating chat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicyTimeoutBehavior {
    Notify,
}

/// One human-approval rule: tool calls matching `tools` suspend the turn
/// until a user in `authorized_approvers` approves or denies, or the request
/// expires after `expires_in_secs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRuleConfig {
    /// Tool names this rule gates (exact match, e.g. `["shell", "write_file"]`).
    pub tools: Vec<String>,
    /// Must be `true` — present so a rule's intent is explicit in config.
    pub require_approval: bool,
    pub risk_level: ApprovalPolicyRiskLevel,
    /// Channel user IDs allowed to answer (e.g. `["@alice:example.org"]`).
    pub authorized_approvers: Vec<String>,
    /// Seconds until the pending request expires.
    pub expires_in_secs: u64,
    pub on_timeout: ApprovalPolicyTimeoutBehavior,
}

/// Config surface for the human-approval flow
/// (`docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md`). Converted to
/// [`octos_agent::HumanApprovalRules`] via [`Self::to_runtime_rules`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ApprovalPolicyConfig {
    #[serde(default)]
    pub default: ApprovalPolicyDefault,
    #[serde(default)]
    pub rules: Vec<ApprovalRuleConfig>,
}

impl ApprovalPolicyRiskLevel {
    pub fn to_runtime(self) -> octos_agent::ApprovalRiskLevel {
        match self {
            Self::Normal => octos_agent::ApprovalRiskLevel::Normal,
            Self::Critical => octos_agent::ApprovalRiskLevel::Critical,
        }
    }
}

impl ApprovalPolicyTimeoutBehavior {
    pub fn to_runtime(self) -> octos_agent::ApprovalTimeoutBehavior {
        match self {
            Self::Notify => octos_agent::ApprovalTimeoutBehavior::Notify,
        }
    }
}

impl ApprovalRuleConfig {
    pub fn to_runtime(&self) -> octos_agent::ApprovalRule {
        octos_agent::ApprovalRule {
            tools: self.tools.clone(),
            risk_level: self.risk_level.to_runtime(),
            authorized_approvers: self.authorized_approvers.clone(),
            expires_in_secs: self.expires_in_secs,
            on_timeout: self.on_timeout.to_runtime(),
        }
    }
}

impl ApprovalPolicyConfig {
    /// Validate every rule: non-empty `tools`, `require_approval` true,
    /// non-empty `authorized_approvers`, positive `expires_in_secs`. Shared by
    /// the top-level config load and the per-profile bootstrap path so a bad
    /// rule fails fast in both instead of gating unexpectedly / creating
    /// unanswerable or instantly-expiring requests (review finding #4).
    pub fn validate(&self) -> Result<()> {
        for (idx, rule) in self.rules.iter().enumerate() {
            if rule.tools.is_empty() {
                eyre::bail!("approval_policy.rules[{idx}].tools must not be empty");
            }
            if !rule.require_approval {
                eyre::bail!("approval_policy.rules[{idx}].require_approval must be true");
            }
            if rule.authorized_approvers.is_empty() {
                eyre::bail!("approval_policy.rules[{idx}].authorized_approvers must not be empty");
            }
            if rule.expires_in_secs == 0 {
                eyre::bail!("approval_policy.rules[{idx}].expires_in_secs must be > 0");
            }
        }
        Ok(())
    }

    pub fn to_runtime_rules(&self) -> octos_agent::HumanApprovalRules {
        octos_agent::HumanApprovalRules::new(
            self.rules
                .iter()
                .map(ApprovalRuleConfig::to_runtime)
                .collect(),
        )
    }
}

/// A sub-provider available for subagent spawning via the spawn tool.
/// The LLM sees these as selectable model options with cost/capability metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubProviderConfig {
    /// Short key used to reference this provider (e.g. "cheap", "strong").
    pub key: String,
    /// Provider name (e.g. "openai", "anthropic", "gemini").
    pub provider: String,
    /// Model name (e.g. "gpt-4o-mini").
    #[serde(default)]
    pub model: Option<String>,
    /// Environment variable name holding the API key for this sub-provider.
    /// If not set, falls back to the default for the provider (e.g. OPENAI_API_KEY).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Custom base URL for this sub-provider.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Human-readable description of when/why to use this model.
    /// Shown to the LLM in the spawn tool schema.
    #[serde(default)]
    pub description: Option<String>,
    /// Default context window (tokens) applied when this sub-provider is selected.
    /// If set, sub-agents using this provider get this context budget automatically
    /// (unless the LLM explicitly overrides it). This controls how aggressively the
    /// sub-agent trims conversation history during its tool loop.
    #[serde(default)]
    pub default_context_window: Option<u32>,
    /// Maximum output tokens per LLM call for this model.
    /// If not set, auto-detected from the model name. Set explicitly when the
    /// auto-detection is wrong or for custom/local models.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// API protocol type: "openai" or "anthropic". Overrides provider default.
    #[serde(default)]
    pub api_type: Option<String>,
}

/// Embedding provider configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Provider name (currently only "openai").
    #[serde(default = "default_embedding_provider")]
    pub provider: String,

    /// Environment variable name for the API key (overrides provider default).
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Custom base URL for the embedding API.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Embedding model id (default: text-embedding-3-small). Set for
    /// OpenAI-compatible providers with their own catalogs (e.g.
    /// DashScope `text-embedding-v4`).
    #[serde(default)]
    pub model: Option<String>,

    /// OpenAI-standard `dimensions` request field. The episodic HNSW
    /// index is fixed at 1536 dims — set this when the model's native
    /// output differs or its vectors are dropped to BM25-only.
    #[serde(default)]
    pub dimensions: Option<u32>,

    /// Path to the local `.gguf` file for the in-process `llamacpp` provider
    /// (feature `embed-llama`; add `embed-llama-metal` / `embed-llama-cuda` to
    /// offload). Any GGUF embedding model works, e.g.
    /// `ggml-org/embeddinggemma-300M-GGUF`. Ignored by remote providers.
    #[serde(default)]
    pub model_path: Option<String>,
}

fn default_embedding_provider() -> String {
    "openai".to_string()
}

/// Memory subsystem configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    /// Token budget for the memory block injected into the system prompt
    /// (long-term memory + daily notes + bank summary combined). Defaults to
    /// [`octos_memory::DEFAULT_MAX_INJECT_TOKENS`]. The budget is spent in
    /// priority order (MEMORY.md, today's notes, bank abstracts, older daily
    /// notes) and omissions are disclosed to the model with a marker.
    #[serde(default)]
    pub max_inject_tokens: Option<usize>,

    /// Automatic memory refreshing (capture + consolidation pipeline).
    #[serde(default)]
    pub refresh: Option<MemoryRefreshConfig>,
}

/// Automatic memory-refresh settings. Default OFF: when disabled there is
/// no `memory_note` tool, no capture policy in the prompt, no per-turn
/// memory re-read, and no background sweep — behavior is identical to
/// before the feature existed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemoryRefreshConfig {
    /// Master switch for the capture layer + read-side refresh + the
    /// background extraction sweep. DEFAULT-ON: `None` means enabled —
    /// automatic memory is the product behavior; set `false` (or
    /// `OCTOS_MEMORY_REFRESH_ENABLED=0`) to opt out.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Model key for the extraction pass (unset → the profile's provider).
    #[serde(default)]
    pub extract_model: Option<String>,
    /// Model key for the consolidation pass (unset → profile's provider).
    #[serde(default)]
    pub consolidate_model: Option<String>,
    /// A session must be idle at least this long before it is swept.
    #[serde(default)]
    pub min_idle_minutes: Option<u64>,
    /// Sessions idle longer than this are too old to sweep.
    #[serde(default)]
    pub max_session_age_days: Option<u64>,
    /// Sessions extracted per pass.
    #[serde(default)]
    pub max_sessions_per_pass: Option<usize>,
    /// Daily extraction-call budget per profile.
    #[serde(default)]
    pub max_extractions_per_day: Option<u32>,
    /// Daily consolidation-run budget per profile (used by the
    /// consolidator; reserved here so the config surface is complete).
    #[serde(default)]
    pub max_consolidations_per_day: Option<u32>,
    /// Daily token budget per profile, shared by extraction+consolidation.
    #[serde(default)]
    pub max_daily_tokens: Option<u64>,
    /// Background tick interval (extraction pass; consolidation piggybacks).
    #[serde(default)]
    pub consolidate_interval_minutes: Option<u64>,
    /// Fast-lane debounce after a user note (consolidator; reserved).
    #[serde(default)]
    pub debounce_seconds: Option<u64>,
    /// Entries unused/unrefreshed this long become archive candidates
    /// (consolidator; reserved).
    #[serde(default)]
    pub unused_days: Option<u32>,
    /// Durable MEMORY.md size cap enforced at consolidation (reserved).
    #[serde(default)]
    pub max_memory_file_tokens: Option<usize>,
    /// Pending-confirm forget requests expire after this many days
    /// (consolidator; reserved).
    #[serde(default)]
    pub pending_confirm_days: Option<u32>,
    /// Hard input budget for one extraction call, tokens (CJK-aware
    /// estimate). Provider metadata does not expose context windows, so
    /// this is explicit.
    #[serde(default)]
    pub max_extract_input_tokens: Option<usize>,
}

impl MemoryConfig {
    /// Effective injection budget, applying the default when unset.
    pub fn effective_max_inject_tokens(config: Option<&MemoryConfig>) -> usize {
        config
            .and_then(|m| m.max_inject_tokens)
            .unwrap_or(octos_memory::DEFAULT_MAX_INJECT_TOKENS)
    }

    /// Whether automatic memory refreshing (capture + read refresh) is on.
    /// DEFAULT-ON: an absent memory/refresh block (or an unset `enabled`)
    /// means enabled; only an explicit `false` opts out.
    pub fn refresh_enabled(config: Option<&MemoryConfig>) -> bool {
        config
            .and_then(|m| m.refresh.as_ref())
            .and_then(|r| r.enabled)
            .unwrap_or(true)
    }
}

/// Field-level host→profile memory inheritance (in-process runtimes: the
/// profile bootstrap and the actor factory). A profile that says nothing
/// inherits the host block wholesale; a profile block present only for
/// knobs (tri-state `enabled` unset) still inherits the host's
/// enabled/disabled decision — under DEFAULT-ON semantics dropping it
/// would bypass a host-level opt-out.
pub fn merge_host_memory_into_profile(
    config: &mut Option<MemoryConfig>,
    host_memory: Option<&MemoryConfig>,
) {
    let Some(host) = host_memory else {
        return;
    };
    let mem = config.get_or_insert_with(Default::default);
    if mem.max_inject_tokens.is_none() {
        mem.max_inject_tokens = host.max_inject_tokens;
    }
    if mem.refresh.is_none() {
        mem.refresh = host.refresh.clone();
    } else if let (Some(profile_refresh), Some(host_refresh)) =
        (mem.refresh.as_mut(), host.refresh.as_ref())
    {
        if profile_refresh.enabled.is_none() {
            profile_refresh.enabled = host_refresh.enabled;
        }
    }
}

fn merge_env_memory_policy(config: &mut Config) {
    if config
        .memory
        .as_ref()
        .and_then(|m| m.max_inject_tokens)
        .is_none()
    {
        if let Ok(v) = std::env::var("OCTOS_MEMORY_MAX_INJECT_TOKENS") {
            if let Ok(n) = v.trim().parse::<usize>() {
                config
                    .memory
                    .get_or_insert_with(Default::default)
                    .max_inject_tokens = Some(n);
            }
        }
    }
    if config
        .memory
        .as_ref()
        .and_then(|m| m.refresh.as_ref())
        .and_then(|r| r.enabled)
        .is_none()
    {
        if let Ok(v) = std::env::var("OCTOS_MEMORY_REFRESH_ENABLED") {
            let parsed = match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                other => {
                    if !other.is_empty() {
                        tracing::warn!(
                            value = %v,
                            "ignoring unrecognized OCTOS_MEMORY_REFRESH_ENABLED"
                        );
                    }
                    None
                }
            };
            if let Some(on) = parsed {
                config
                    .memory
                    .get_or_insert_with(Default::default)
                    .refresh
                    .get_or_insert_with(Default::default)
                    .enabled = Some(on);
            }
        }
    }
}

/// Message queue mode for handling messages arriving during active agent runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QueueMode {
    /// Process queued messages one at a time (FIFO).
    Followup,
    /// Concatenate queued messages from the same session into one before processing.
    #[default]
    Collect,
    /// Keep only the latest message, discard older queued messages.
    ///
    /// Renamed from `steer`: that word means mid-turn INJECTION everywhere
    /// else in this codebase (`turn/steer`, the agent's `SteerBuffer`),
    /// where nothing is dropped — the opposite of this variant. The serde
    /// alias keeps existing configs parsing; the `/queue` chat command
    /// accepts both spellings.
    #[serde(alias = "steer")]
    Latest,
    /// Cancel the current run and process the new message immediately.
    Interrupt,
    /// If the current LLM call exceeds the patience threshold and a new message
    /// arrives, spawn a full agent task for the new message concurrently.
    /// Both results are delivered to the user.
    Speculative,
}

/// Gateway mode configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Maximum conversation history messages to include.
    #[serde(default = "default_max_history")]
    pub max_history: usize,

    /// Custom system prompt for gateway mode.
    #[serde(default)]
    pub system_prompt: Option<String>,

    /// Message queue mode for messages arriving while a run is active:
    /// "followup" | "collect" (default) | "latest" | "interrupt" |
    /// "speculative". See [`QueueMode`].
    #[serde(default)]
    pub queue_mode: QueueMode,

    /// Maximum sessions to keep in memory (LRU eviction). Default: 1000.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,

    /// Maximum concurrent session processing. Default: 10.
    #[serde(default = "default_max_concurrent_sessions")]
    pub max_concurrent_sessions: usize,

    /// Per-action timeout in seconds for the browser tool. Default: 300 (5 minutes).
    /// If a single browser action exceeds this, the session is killed and an error is returned.
    #[serde(default)]
    pub browser_timeout_secs: Option<u64>,

    /// LLM HTTP request timeout in seconds. Default: 120.
    #[serde(default)]
    pub llm_timeout_secs: Option<u64>,

    /// LLM HTTP connect timeout in seconds. Default: 30.
    #[serde(default)]
    pub llm_connect_timeout_secs: Option<u64>,

    /// Maximum seconds for all parallel tool calls to complete. Default: 300.
    #[serde(default)]
    pub tool_timeout_secs: Option<u64>,

    /// Maximum seconds for processing a single session message. Default: 600.
    #[serde(default)]
    pub session_timeout_secs: Option<u64>,

    /// Default max output tokens per LLM call. When set, overrides the built-in
    /// default from model_limits.json. Pipeline nodes can further override per-node.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,

    /// Reasoning effort for thinking models (`low`|`medium`|`high`). Applied to
    /// every turn; only models that declare a reasoning style receive it
    /// (DeepSeek V4 gets `reasoning_effort` + `thinking`, OpenAI reasoning models
    /// and Grok get `reasoning_effort`), so non-thinking models silently ignore it.
    #[serde(default)]
    pub reasoning_effort: Option<octos_llm::ReasoningEffort>,

    /// Sampling temperature override for chat LLM calls. When unset (the
    /// default), the built-in `ChatConfig` default (`0.0`, greedy) is used and
    /// the request is byte-for-byte unchanged — so cloud providers are
    /// unaffected. Set a value (e.g. `0.7`) to override it; this is primarily
    /// for **local / OpenAI-compatible** models, where forced greedy decoding
    /// triggers repetition collapse (a small model re-emits the same tool call
    /// until `max_tokens`). See issue #2172.
    #[serde(default)]
    pub llm_temperature: Option<f32>,

    /// Extra sampler params for OpenAI-compatible servers, flattened verbatim
    /// into the request body — e.g. `{"repeat_penalty": 1.1, "top_p": 0.95}`.
    /// For params octos does not model. `None` → nothing added, so cloud
    /// requests are unchanged. The robust fix for local-model repetition
    /// collapse (temperature alone is only a partial mitigation). #2172.
    #[serde(default)]
    pub llm_sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            max_history: default_max_history(),
            system_prompt: None,
            queue_mode: QueueMode::default(),
            max_sessions: default_max_sessions(),
            max_concurrent_sessions: default_max_concurrent_sessions(),
            browser_timeout_secs: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
            tool_timeout_secs: None,
            session_timeout_secs: None,
            max_output_tokens: None,
            reasoning_effort: None,
            llm_temperature: None,
            llm_sampling_params: None,
        }
    }
}

fn default_max_sessions() -> usize {
    1000
}

fn default_max_concurrent_sessions() -> usize {
    10
}

fn default_max_history() -> usize {
    50
}

/// Load `config.json` as a raw `serde_json::Value`, apply `mutate`, and
/// atomically write the result back. Preserves unknown fields that the
/// strongly-typed [`Config`] struct would otherwise silently drop.
///
/// Creates the parent directory and an empty JSON object if the file does
/// not exist yet. Writes to a sibling `*.tmp` file first, then renames.
pub fn write_mutation<F>(path: &Path, mutate: F) -> Result<()>
where
    F: FnOnce(&mut serde_json::Value) -> Result<()>,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create dir: {}", parent.display()))?;
    }
    let mut value: serde_json::Value = if path.exists() {
        let body = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&body)
            .wrap_err_with(|| format!("failed to parse {}", path.display()))?
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };
    mutate(&mut value)?;
    let body = serde_json::to_string_pretty(&value)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).wrap_err_with(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .wrap_err_with(|| format!("failed to rename into {}", path.display()))?;
    Ok(())
}

/// Resolve the `config.json` path a command WOULD read/write, using existence
/// checks only (never parses the file — safe when the config is malformed).
///
/// Mirrors [`Config::load_resolved`]'s precedence so `octos config` targets the
/// exact same file the runtime loads:
/// 1. `config_override` (an explicit `--config <FILE>`),
/// 2. (default installs only) project-local `cwd/.octos/config.json`,
/// 3. `ctx.config_home/config.json`,
/// 4. (default installs only) legacy `~/.octos/config.json`.
///
/// Falls back to the canonical `config_home/config.json` (the write location)
/// when no file exists yet.
pub fn resolve_config_file_path(
    cwd: &Path,
    ctx: &crate::config_context::ConfigContext,
    config_override: Option<&Path>,
) -> PathBuf {
    if let Some(path) = config_override {
        return path.to_path_buf();
    }
    if ctx.is_default {
        let local = cwd.join(".octos").join("config.json");
        if local.exists() {
            return local;
        }
    }
    let home = ctx.config_home.join("config.json");
    if home.exists() {
        return home;
    }
    if ctx.is_default {
        if let Some(home_dir) = dirs::home_dir() {
            let legacy = home_dir.join(".octos").join("config.json");
            if legacy != home && legacy.exists() {
                return legacy;
            }
        }
    }
    home
}

impl Config {
    /// Path to the runtime config file under the resolved data dir.
    pub fn data_dir_config_path(data_dir: &Path) -> PathBuf {
        data_dir.join("config.json")
    }

    /// Load config from the current project plus the canonical config context.
    ///
    /// This is the preferred entrypoint: it threads the single
    /// [`ConfigContext`](crate::config_context::ConfigContext) so the precedence
    /// is identical at every call site.
    pub fn load_with_context(
        cwd: &Path,
        ctx: &crate::config_context::ConfigContext,
    ) -> Result<Self> {
        Self::load_with_context_path(cwd, ctx).map(|(config, _)| config)
    }

    /// Like [`Self::load_with_context`] but also returns the resolved config
    /// path when one exists.
    pub fn load_with_context_path(
        cwd: &Path,
        ctx: &crate::config_context::ConfigContext,
    ) -> Result<(Self, Option<PathBuf>)> {
        Self::load_resolved(cwd, &ctx.config_home, ctx.is_default)
    }

    /// Core loader. Precedence:
    /// 1. (only when `is_default`) project-local `cwd/.octos/config.json`
    /// 2. `config_home/config.json`
    /// 3. (only when `is_default`) legacy `~/.octos/config.json`
    /// 4. defaults (with `merge_env_plugin_policy`)
    ///
    /// Explicit / tenant contexts (`is_default == false`) read ONLY from
    /// `config_home`. They MUST NOT read the ambient project-local
    /// `cwd/.octos/config.json` either: a tenant/`serve` process whose `cwd`
    /// happens to be `$HOME` would otherwise pick up the host's
    /// `~/.octos/config.json` (and, in `serve`, expose it to admin writes via
    /// `AppState.config_path`). That isolation is the whole point of this
    /// resolver. The project-local convenience is reserved for default-context
    /// `octos chat`/`gateway` invocations.
    fn load_resolved(
        cwd: &Path,
        config_home: &Path,
        is_default: bool,
    ) -> Result<(Self, Option<PathBuf>)> {
        // 1. Project-local config — DEFAULT context only. In explicit/tenant
        //    contexts this is skipped so an ambient `cwd/.octos/config.json`
        //    (e.g. the host's `~/.octos`) can never leak in.
        if is_default {
            let local_config = cwd.join(".octos").join("config.json");
            if local_config.exists() {
                tracing::info!(path = %local_config.display(), "loading config (project-local)");
                return Ok((Self::from_file(&local_config)?, Some(local_config)));
            }
        }

        // 2. The resolved config_home (XDG for default, data_dir/OCTOS_CONFIG_DIR
        //    for explicit). Resolved exactly once by `resolve_config_context`.
        let home_config = config_home.join("config.json");
        if home_config.exists() {
            tracing::info!(path = %home_config.display(), "loading config (config home)");
            return Ok((Self::from_file(&home_config)?, Some(home_config)));
        }

        // 3. Legacy back-compat: only for default installs, and only when the
        //    legacy path differs from config_home (so we don't double-check the
        //    same file). Explicit/tenant contexts never reach here.
        if is_default {
            // Prefer an explicit `HOME` before the OS profile dir so the legacy
            // `~/.octos` lookup is testable and user-overridable on Windows,
            // where `dirs::home_dir()` reads FOLDERID_Profile and ignores
            // `HOME`/`USERPROFILE`. On Unix `dirs::home_dir()` already consults
            // `HOME`, so this is behaviour-preserving there.
            let legacy_home = std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(dirs::home_dir);
            if let Some(home) = legacy_home {
                let legacy_config = home.join(".octos").join("config.json");
                if legacy_config != home_config && legacy_config.exists() {
                    tracing::info!(
                        path = %legacy_config.display(),
                        "loading config (legacy ~/.octos — migrate by moving it to the current config path)"
                    );
                    return Ok((Self::from_file(&legacy_config)?, Some(legacy_config)));
                }
            }
        }

        // 4. No config found, use defaults.
        tracing::info!("no config.json found, using defaults");
        let mut config = Self::default();
        merge_env_memory_policy(&mut config);
        Ok((config, None))
    }

    /// Load config from a specific file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read config file: {}", path.display()))?;

        // Parse as raw Value first for migration
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .wrap_err_with(|| format!("failed to parse config file: {}", path.display()))?;

        let migrated = migrate_config(&mut value);

        let mut config: Self = serde_json::from_value(value)
            .wrap_err_with(|| format!("failed to deserialize config: {}", path.display()))?;

        // Expand environment variables in config values
        config.expand_env_vars();
        config.validate_approval_policy()?;

        merge_env_memory_policy(&mut config);

        // Log if migration changed something (don't silently rewrite user's config)
        if migrated {
            tracing::info!(
                path = %path.display(),
                version = CURRENT_CONFIG_VERSION,
                "Config file needs migration to version {}; the migration is applied on load.",
                CURRENT_CONFIG_VERSION
            );
        }

        Ok(config)
    }

    /// Validate the human-approval rule set at config-load time so a typo'd
    /// rule fails fast instead of silently never gating (or gating with an
    /// unanswerable request). Runs after `expand_env_vars` so `${VAR}`
    /// references in approver lists are validated post-expansion.
    fn validate_approval_policy(&self) -> Result<()> {
        match &self.approval_policy {
            Some(policy) => policy.validate(),
            None => Ok(()),
        }
    }

    /// Expand environment variables in config values.
    /// Supports ${VAR_NAME} syntax.
    fn expand_env_vars(&mut self) {
        if let Some(ref mut base_url) = self.base_url {
            *base_url = Self::expand_env_var(base_url);
        }
        if let Some(ref mut model) = self.model {
            *model = Self::expand_env_var(model);
        }
        if let Some(ref mut provider) = self.provider {
            *provider = Self::expand_env_var(provider);
        }
        // Approval rules: expand ${VAR} in authorized_approvers so a
        // deployment can reference `${MATRIX_APPROVER}` etc. Without this the
        // literal `${VAR}` would pass the non-empty validation check and then
        // never match a real Matrix user id (review finding #4).
        if let Some(ref mut policy) = self.approval_policy {
            for rule in &mut policy.rules {
                for approver in &mut rule.authorized_approvers {
                    *approver = Self::expand_env_var(approver);
                }
            }
        }
    }

    /// Expand ${VAR_NAME} patterns in a string.
    fn expand_env_var(s: &str) -> String {
        let mut result = s.to_string();
        let mut start = 0;

        while let Some(begin) = result[start..].find("${") {
            let begin = start + begin;
            if let Some(end) = result[begin..].find('}') {
                let end = begin + end;
                let var_name = &result[begin + 2..end];
                if let Ok(value) = std::env::var(var_name) {
                    result = format!("{}{}{}", &result[..begin], value, &result[end + 1..]);
                    start = begin + value.len();
                } else {
                    start = end + 1;
                }
            } else {
                break;
            }
        }
        result
    }

    /// Get the API key: auth store first, then environment variable.
    /// [`Self::get_api_key`] with an explicit env-var override (e.g. the
    /// embedding block's `api_key_env`): resolves through the SAME chain —
    /// secret registration, auth store, `env_vars` (keychain-resolved),
    /// process env — instead of a bare `std::env::var` read.
    pub fn get_api_key_with_env(&self, provider: &str, env_var: Option<&str>) -> Result<String> {
        match env_var {
            // A CUSTOM var means "use this variable": the provider-scoped
            // auth store must not win, or a stored `octos auth login -p
            // openai` token would be sent to the custom OpenAI-compatible
            // endpoint the override targets. But when the override IS the
            // provider's default var name (a redundant-but-legal config),
            // the full provider chain — auth store included — still
            // applies, preserving pre-existing login-based setups.
            // A registry-known key name (the provider's `api_key_env` OR any
            // declared `key_env_aliases`, e.g. KIMI_API_KEY for moonshot) is
            // NOT a custom override: run the FULL chain (auth store + sibling
            // expansion) via `resolve_api_key`, so `octos doctor`, the dashboard,
            // and embedding config agree with what `get_api_key` accepts. Without
            // this, an init-generated Moonshot config (api_key_env=KIMI_API_KEY)
            // would be misclassified as custom and skip the MOONSHOT_API_KEY
            // fallback + auth store.
            Some(var) if Self::provider_knows_key_env(provider, var) => {
                self.resolve_api_key(provider, var.to_string())
            }
            // A genuinely custom var means "use this variable": the
            // provider-scoped auth store must not win, or a stored `octos auth
            // login -p openai` token would be sent to the custom
            // OpenAI-compatible endpoint the override targets.
            Some(var) => self.resolve_env_var_only(var),
            None => self.get_api_key(provider),
        }
    }

    /// The env-var name the provider chain would use by default.
    pub(crate) fn provider_default_env_var(provider: &str) -> Option<String> {
        Some(
            octos_llm::registry::lookup(provider)
                .and_then(|e| e.api_key_env)
                .map(String::from)
                .unwrap_or_else(|| format!("{}_API_KEY", provider.to_uppercase())),
        )
    }

    /// Whether `name` is one of `provider`'s known key env var names — its
    /// registry `api_key_env` or a declared `key_env_alias`. For a provider not
    /// in the registry, falls back to the conventional `{PROVIDER}_API_KEY`.
    /// Case-sensitive (Unix env names are case-sensitive).
    pub(crate) fn provider_knows_key_env(provider: &str, name: &str) -> bool {
        match octos_llm::registry::lookup(provider) {
            Some(entry) => entry.is_known_key_env(name),
            None => name == format!("{}_API_KEY", provider.to_uppercase()),
        }
    }

    /// Resolve a key from an explicit env-var name WITHOUT provider-scoped
    /// auth-store lookup: secret registration → `env_vars` map (keychain-
    /// resolved) → process env.
    fn resolve_env_var_only(&self, env_var: &str) -> Result<String> {
        octos_agent::register_secret_env_names([env_var]);
        if let Some(value) = self.env_vars.get(env_var).and_then(|value| {
            crate::auth::keychain::resolve_value(env_var, value).filter(|value| !value.is_empty())
        }) {
            return Ok(value);
        }
        // Treat an empty value as unset, matching `resolve_api_key`, so status
        // reporting and resolution agree and an empty Bearer key is never sent.
        match std::env::var(env_var) {
            Ok(value) if !value.is_empty() => Ok(value),
            _ => Err(eyre::eyre!(
                "{env_var} not set or empty (explicit embedding api_key_env)"
            )),
        }
    }

    pub fn get_api_key(&self, provider: &str) -> Result<String> {
        // Resolve the env var name we expect to hold this provider's key, and
        // mark it as a secret FIRST — before any early return — so the
        // configured key var is stripped from the default subprocess
        // environment regardless of which resolution path (auth store /
        // env_vars / keychain / process env) actually wins below. This also
        // covers a custom `api_key_env` whose NAME does not look secret to the
        // heuristic, so it can't be `echo`'d from the shell tool. Registered
        // names are still allowlistable: a tool that declares the var in its
        // manifest `env` list may receive it (the sanctioned path for skills
        // that call LLMs). See `octos_agent::subprocess_env`.
        let env_var = self.api_key_env.clone().unwrap_or_else(|| {
            octos_llm::registry::lookup(provider)
                .and_then(|e| e.api_key_env)
                .map(String::from)
                .unwrap_or_else(|| format!("{}_API_KEY", provider.to_uppercase()))
        });
        self.resolve_api_key(provider, env_var)
    }

    /// Shared resolution body: auth store → env_vars (keychain) → process
    /// env, with the var name secret-registered first.
    fn resolve_api_key(&self, provider: &str, env_var: String) -> Result<String> {
        // Candidate env-var names. A provider may declare sibling key vars in
        // the registry (e.g. Moonshot accepts MOONSHOT_API_KEY or KIMI_API_KEY).
        // We only expand to those siblings when the configured var is ITSELF one
        // of the provider's known key names — an arbitrary custom `api_key_env`
        // override (e.g. a proxy key) stays exclusive so a missing override
        // never falls back to an unrelated ambient credential.
        let mut candidates = vec![env_var.clone()];
        if let Some(entry) = octos_llm::registry::lookup(provider) {
            // Only expand to sibling key vars when the configured var is itself
            // a declared key name (case-sensitive — see `is_known_key_env`).
            if entry.is_known_key_env(&env_var) {
                for k in entry.key_env_names() {
                    if !candidates.iter().any(|c| c == k) {
                        candidates.push(k.to_string());
                    }
                }
            }
        }
        octos_agent::register_secret_env_names(candidates.iter());

        // Check auth store first. Auth is GLOBAL: it lives under the resolver's
        // `auth_home` (OCTOS_CONFIG_DIR if set, else the XDG default). This is
        // independent of `--data-dir`, so per-profile gateways keep the host's
        // shared `octos auth login` credentials. We resolve the context with no
        // cli_data_dir because auth_home never depends on it.
        //
        // `bypass_auth_store` opts out (used by octos-ffi): a caller that passed
        // an explicit key must have it win over any ambient login credential.
        if !self.bypass_auth_store {
            let auth_home = crate::config_context::resolve_config_context(None).auth_home;
            if let Ok(store) = crate::auth::AuthStore::at(&auth_home) {
                if let Some(cred) = store.get(provider) {
                    if !cred.is_expired() {
                        return Ok(cred.access_token.clone());
                    }
                }
            }
        }

        for name in &candidates {
            if let Some(value) = self.env_vars.get(name).and_then(|value| {
                crate::auth::keychain::resolve_value(name, value).filter(|value| !value.is_empty())
            }) {
                return Ok(value);
            }
        }

        for name in &candidates {
            if let Ok(value) = std::env::var(name) {
                if !value.is_empty() {
                    return Ok(value);
                }
            }
        }

        Err(eyre::eyre!(
            "{env_var} not set or empty. Run `octos auth login -p {provider}` or set the env var"
        ))
    }

    /// Validate the configuration, returning any warnings.
    #[allow(clippy::manual_map)]
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        // Check provider is valid
        if let Some(ref provider) = self.provider {
            if provider != "custom" && octos_llm::registry::lookup(provider).is_none() {
                let valid = octos_llm::registry::all_names();
                warnings.push(format!(
                    "Unknown provider '{}'. Valid options: {}",
                    provider,
                    valid.join(", ")
                ));
            }
        }

        // Check model/provider mismatch
        if let (Some(provider), Some(model)) = (&self.provider, &self.model) {
            if !is_valid_model_for_provider(provider, model) {
                warnings.push(format!(
                    "Model '{model}' may not be valid for provider '{provider}'. Check provider docs."
                ));
            }
        }

        // Check base_url format
        if let Some(ref url) = self.base_url {
            if !(url.starts_with("http://") || url.starts_with("https://")) || url.contains(' ') {
                warnings.push(format!("base_url '{url}' is not a valid URL"));
            }
        }

        // Check gateway config
        if let Some(ref gw) = self.gateway {
            if gw.max_history == 0 || gw.max_history > 1000 {
                warnings.push(format!(
                    "max_history {} is out of range (1-1000)",
                    gw.max_history
                ));
            }
        }

        // Check build_cache section floors (peer/verify slot caps >= 1,
        // stale window >= 1). Warnings, not errors: a bad value still gets
        // a working (default-clamped) pool.
        if let Some(ref bc) = self.build_cache {
            for warning in bc.validate() {
                warnings.push(warning);
            }
        }

        // Check API key is set
        let provider = match self.provider.as_deref() {
            Some(p) => p,
            None => {
                warnings.push(
                    "No provider configured. Create config.json with a provider entry to set \
                     up your LLM provider."
                        .to_string(),
                );
                return warnings;
            }
        };
        if self.get_api_key(provider).is_err() {
            let env_var = self.api_key_env.clone().unwrap_or_else(|| {
                octos_llm::registry::lookup(provider)
                    .and_then(|e| e.api_key_env)
                    .map(String::from)
                    .unwrap_or_else(|| format!("{}_API_KEY", provider.to_uppercase()))
            });
            warnings.push(format!("{env_var} environment variable not set"));
        }

        warnings
    }
}

/// Migrate config to current version. Returns true if anything changed.
fn migrate_config(value: &mut serde_json::Value) -> bool {
    let current = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

    if current >= CURRENT_CONFIG_VERSION {
        return false;
    }

    // Future migrations go here:
    // if current < 2 { ... }

    // Set version to current
    value["version"] = serde_json::json!(CURRENT_CONFIG_VERSION);
    true
}

/// Check if a model name looks reasonable for a given provider.
/// Not exhaustive -- warns on clear mismatches only.
fn is_valid_model_for_provider(provider: &str, model: &str) -> bool {
    let m = model.to_lowercase();
    match provider {
        "anthropic" => m.contains("claude"),
        "openai" => {
            m.contains("gpt") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
        }
        "gemini" | "google" => m.contains("gemini"),
        "deepseek" => m.contains("deepseek"),
        "moonshot" | "kimi" => m.contains("kimi") || m.contains("moonshot"),
        "dashscope" | "qwen" => m.contains("qwen"),
        "zhipu" | "glm" => m.contains("glm"),
        "zai" | "z.ai" => true, // Z.AI hosts multiple models (GLM, Claude, etc.)
        "minimax" => m.contains("minimax"),
        // These host many models, accept any
        "groq" | "nvidia" | "nim" | "ollama" | "vllm" | "local" | "openrouter" => true,
        _ => true,
    }
}

/// Detect LLM provider from model name when no explicit provider is set.
pub fn detect_provider(model: &str) -> Option<&'static str> {
    octos_llm::registry::detect_provider(model)
}

#[cfg(test)]
mod tests {
    /// `QueueMode::Latest` was renamed from `steer` (the word collides with
    /// `turn/steer`, which injects rather than discards). Existing configs
    /// and `/queue steer` must keep working; new ones use `latest`.
    #[test]
    fn queue_mode_latest_parses_both_spellings_and_defaults_to_collect() {
        use super::QueueMode;
        let latest: QueueMode = serde_json::from_str("\"latest\"").expect("canonical name");
        assert_eq!(latest, QueueMode::Latest);
        let steer: QueueMode = serde_json::from_str("\"steer\"").expect("legacy alias");
        assert_eq!(steer, QueueMode::Latest);
        // Round-trip now WRITES the canonical spelling.
        assert_eq!(
            serde_json::to_string(&QueueMode::Latest).expect("serialize"),
            "\"latest\""
        );
        assert_eq!(QueueMode::default(), QueueMode::Collect);
    }

    use super::*;

    /// Crate-wide lock for EVERY test that pivots the global `HOME` /
    /// `OCTOS_HOME` / `OCTOS_CONFIG_DIR` env vars. These are process-global, so
    /// all such tests (here and in `config_context`) must serialize against the
    /// SAME mutex — per-module locks would let env-mutating tests race across
    /// modules (a flaky-failure source).
    use crate::config_context::TEST_ENV_LOCK as HOME_ENV_LOCK;

    #[test]
    fn write_mutation_preserves_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"mode":"local","unknown_field":{"keep":"me"},"nested":[1,2,3]}"#,
        )
        .unwrap();
        write_mutation(&path, |v| {
            v.as_object_mut()
                .unwrap()
                .insert("mode".into(), serde_json::json!("cloud"));
            Ok(())
        })
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["mode"], "cloud");
        assert_eq!(parsed["unknown_field"]["keep"], "me");
        assert_eq!(parsed["nested"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    #[allow(unsafe_code)]
    fn test_expand_env_var() {
        // SAFETY: test-only, single-threaded
        unsafe {
            std::env::set_var("TEST_VAR", "hello");
        }
        assert_eq!(Config::expand_env_var("${TEST_VAR}"), "hello");
        assert_eq!(
            Config::expand_env_var("prefix_${TEST_VAR}_suffix"),
            "prefix_hello_suffix"
        );
        assert_eq!(Config::expand_env_var("no_var"), "no_var");
        assert_eq!(
            Config::expand_env_var("${UNDEFINED_VAR}"),
            "${UNDEFINED_VAR}"
        );
        // SAFETY: test-only, single-threaded
        unsafe {
            std::env::remove_var("TEST_VAR");
        }
    }

    #[test]
    fn appui_sessions_in_cwd_defaults_on_and_can_be_disabled() {
        // Absent `[appui]` → the `#[serde(default)]` on the parent field calls
        // `AppUiConfig::default()` → per-project storage ON.
        let absent: Config = serde_json::from_str(r#"{"provider": "anthropic"}"#).unwrap();
        assert!(
            absent.appui.sessions_in_cwd,
            "an absent [appui] section must default sessions_in_cwd on",
        );
        assert!(absent.appui.allowed_origins.is_empty());

        // Present `[appui]` missing the key → the field-level
        // `#[serde(default = \"default_sessions_in_cwd\")]` path → ON.
        let empty: Config =
            serde_json::from_str(r#"{"provider": "anthropic", "appui": {}}"#).unwrap();
        assert!(
            empty.appui.sessions_in_cwd,
            "an empty [appui] section must default sessions_in_cwd on",
        );

        // Explicit opt-out → the legacy global per-profile store is honored.
        let disabled: Config = serde_json::from_str(
            r#"{"provider": "anthropic", "appui": {"sessions_in_cwd": false}}"#,
        )
        .unwrap();
        assert!(
            !disabled.appui.sessions_in_cwd,
            "an operator can still force the legacy global store off",
        );

        // The programmatic Default agrees with both serde paths.
        assert!(AppUiConfig::default().sessions_in_cwd);
        assert!(AppUiConfig::default().allowed_origins.is_empty());
    }

    #[test]
    fn test_detect_provider_claude() {
        assert_eq!(
            detect_provider("claude-sonnet-4-20250514"),
            Some("anthropic")
        );
        assert_eq!(detect_provider("claude-3-haiku"), Some("anthropic"));
    }

    #[test]
    fn test_validate_unknown_provider() {
        let config = Config {
            provider: Some("invalid".to_string()),
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("Unknown provider")));
    }

    #[test]
    fn test_load_prefers_project_local_over_data_dir_config() {
        let cwd = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let local_dir = cwd.path().join(".octos");
        std::fs::create_dir_all(&local_dir).unwrap();
        let local_config = local_dir.join("config.json");
        let data_dir_config = data_dir.path().join("config.json");

        std::fs::write(
            &local_config,
            r#"{"provider":"anthropic","model":"claude-sonnet-4-20250514"}"#,
        )
        .unwrap();
        std::fs::write(
            &data_dir_config,
            r#"{"provider":"openai","model":"gpt-4o"}"#,
        )
        .unwrap();

        // Project-local precedence is a DEFAULT-context convenience.
        let (config, path) = Config::load_resolved(cwd.path(), data_dir.path(), true).unwrap();
        assert_eq!(config.provider.as_deref(), Some("anthropic"));
        assert_eq!(path.as_deref(), Some(local_config.as_path()));
    }

    /// Gate 7 (end-to-end, the load-bearing "no per-profile login regression"):
    /// `get_api_key` resolves the GLOBAL auth store (XDG `auth_home`), NOT a
    /// per-profile `data_dir/auth.json`. We seed a credential at the XDG
    /// location and prove the API-key lookup finds it. `OCTOS_CONFIG_DIR` must
    /// be unset for the default/global case, so this test serializes on the
    /// shared env lock and clears all three env vars.
    #[test]
    #[allow(unsafe_code)]
    fn get_api_key_reads_global_xdg_auth_store() {
        let _g = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path();

        let original_home = std::env::var_os("HOME");
        let original_octos_home = std::env::var_os("OCTOS_HOME");
        let original_octos_config = std::env::var_os("OCTOS_CONFIG_DIR");
        // Must also clear XDG_CONFIG_HOME: auth_home derives from it, so an
        // ambient absolute value would write auth.json outside the temp HOME.
        let original_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: serialized by HOME_ENV_LOCK; restored below.
        unsafe {
            std::env::set_var("HOME", fake_home);
            std::env::remove_var("OCTOS_HOME");
            std::env::remove_var("OCTOS_CONFIG_DIR");
            std::env::remove_var("XDG_CONFIG_HOME");
        }

        // Resolve the GLOBAL auth_home (XDG) and seed a credential there.
        let ctx = crate::config_context::resolve_config_context(None);
        let mut store = crate::auth::AuthStore::open(&ctx).unwrap();
        store
            .set(
                "anthropic",
                crate::auth::AuthCredential {
                    access_token: "global-xdg-token".to_string(),
                    refresh_token: None,
                    expires_at: None,
                    provider: "anthropic".to_string(),
                    auth_method: "paste_token".to_string(),
                },
            )
            .unwrap();

        // A default config — get_api_key should consult the GLOBAL auth store
        // and return the seeded token (it does NOT look at any data_dir).
        let config = Config::default();
        let key = config.get_api_key("anthropic");

        match original_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match original_octos_home {
            Some(v) => unsafe { std::env::set_var("OCTOS_HOME", v) },
            None => unsafe { std::env::remove_var("OCTOS_HOME") },
        }
        match original_octos_config {
            Some(v) => unsafe { std::env::set_var("OCTOS_CONFIG_DIR", v) },
            None => unsafe { std::env::remove_var("OCTOS_CONFIG_DIR") },
        }
        match original_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }

        assert_eq!(
            key.unwrap(),
            "global-xdg-token",
            "get_api_key must read the GLOBAL XDG auth store (shared login)"
        );
    }

    #[test]
    fn should_reject_approval_policy_when_rule_invalid() {
        let base = ApprovalRuleConfig {
            tools: vec!["shell".into()],
            require_approval: true,
            risk_level: ApprovalPolicyRiskLevel::Critical,
            authorized_approvers: vec!["@alice:example.org".into()],
            expires_in_secs: 300,
            on_timeout: ApprovalPolicyTimeoutBehavior::Notify,
        };
        let config_with = |rule: ApprovalRuleConfig| Config {
            approval_policy: Some(ApprovalPolicyConfig {
                default: ApprovalPolicyDefault::Allow,
                rules: vec![rule],
            }),
            ..Default::default()
        };

        let mut rule = base.clone();
        rule.tools.clear();
        assert!(config_with(rule).validate_approval_policy().is_err());

        let mut rule = base.clone();
        rule.require_approval = false;
        assert!(config_with(rule).validate_approval_policy().is_err());

        let mut rule = base.clone();
        rule.authorized_approvers.clear();
        assert!(config_with(rule).validate_approval_policy().is_err());

        let mut rule = base.clone();
        rule.expires_in_secs = 0;
        assert!(config_with(rule).validate_approval_policy().is_err());

        assert!(config_with(base).validate_approval_policy().is_ok());
    }

    // --- memory refresh flag ---

    #[test]
    fn should_default_refresh_on_when_memory_absent_or_empty() {
        // DEFAULT-ON: automatic memory is the product behavior; absence of
        // config means enabled.
        assert!(MemoryConfig::refresh_enabled(None));
        let empty: MemoryConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(MemoryConfig::refresh_enabled(Some(&empty)));
        let refresh_empty: MemoryConfig =
            serde_json::from_value(serde_json::json!({"refresh": {}})).unwrap();
        assert!(MemoryConfig::refresh_enabled(Some(&refresh_empty)));
    }
}
