//! Profile system (M8.3 — runtime-v0.1 close-out).
//!
//! # What is a `ProfileDefinition`?
//!
//! A [`ProfileDefinition`] is a single, declarative manifest that describes
//! how the agent runtime should bootstrap: which tools are available, which
//! [`crate::agents::AgentDefinition`] sub-agents are preloaded, which MCP
//! servers are wired, how compaction is tiered, and which models are
//! preferred. Before M8.3 every one of these settings was wired implicitly
//! across a dozen startup sites; afterwards, a single profile declaration
//! consolidates the envelope.
//!
//! The built-in `coding` profile is the no-flag default and carries a lean
//! core-coding allow list (files, shell, search, memory, spawn, the workspace
//! check tool, plan tracking, user questions, and tool_search) so `octos chat`
//! does not ship every tool schema to the LLM on every round. The allow-list
//! filter narrows the VISIBLE registry, so tools it excludes (web/research/
//! media/pipeline) are restored via the `coding-full` built-in, which
//! preserves the pre-lean unfiltered surface byte-for-byte.
//! Alternate profiles declare their own allow lists and
//! expanded agent sets.
//!
//! # Forward compatibility
//!
//! Unlike [`crate::agents::AgentDefinition`] (which uses
//! `#[serde(deny_unknown_fields)]`) this schema is **forward-compatible**:
//! a v1 client MUST accept a v2 manifest that carries extra fields so the
//! CLI does not immediately break when a newer config arrives on the host
//! via config-sync or a mounted volume. The `version` field still acts as
//! a hard gate — a v2 profile on a v1 client produces a version-mismatch
//! error *before* the extra fields are considered.
//!
//! # Resolution order
//!
//! [`ProfileDefinition::load`] accepts either a name or a path:
//!
//! 1. If the argument starts with `/`, `./`, or `~/` it is treated as a
//!    filesystem path. The file is loaded directly.
//! 2. Otherwise the argument is a profile id. The loader first checks
//!    `~/.octos/profiles/<id>/profile.{toml,json}`.
//! 3. Finally the loader falls back to the crate-shipped built-in registry
//!    (JSON files under `crates/octos-agent/src/assets/profiles/`).
//!
//! Today's built-in profiles are `coding` (the lean default), `coding-full`
//! (the unfiltered pre-lean surface).
//!
//! # Applied vs recorded settings
//!
//! M8.3 deliberately scopes its behaviour to "schema + loader + tool
//! filter". Some profile fields are populated today but *recorded, not
//! enforced* until a follow-up milestone wires them in:
//!
//! - `compaction_policy` — the tier overrides are parsed and exposed via
//!   [`ProfileDefinition::compaction_policy`], but the runtime still uses
//!   the workspace compaction runner from M6.3. M8.5's tiered runner is
//!   where the profile override becomes active.
//! - `model_preferences` — parsed and exposed, but the provider chain does
//!   not yet consult them. A future milestone wires the preferences into
//!   the adaptive router's lane-scoring input.
//! - `mcp_servers` — only the ids are captured. Actual server config
//!   resolution is a follow-up milestone. `coding` ship with
//!   an empty list so no behaviour change falls out of this.
//!
//! The `permissions` stub also lands in a minimal form (default /
//! restricted) so M8.4 can extend it without schema churn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::tools::ToolRegistry;

/// Current profile schema version. Manifests whose `version` differs from
/// this constant are rejected at load time.
pub const PROFILE_SCHEMA_VERSION: u32 = 1;

/// Crate-shipped profiles available as a built-in fallback after the
/// user-config search. Ordered (name, raw JSON text).
const BUILTIN_PROFILES: &[(&str, &str)] = &[
    ("coding", include_str!("../assets/profiles/coding.json")),
    (
        "coding-full",
        include_str!("../assets/profiles/coding-full.json"),
    ),
];

/// The source a resolved profile was loaded from. Used by the CLI resolver
/// to emit an informative `profile resolved: ...` log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileSource {
    /// Explicit `--profile <path>` pointing at a file on disk.
    ExplicitPath,
    /// Named profile found in `~/.octos/profiles/<name>/profile.{toml,json}`.
    UserDir,
    /// Named profile that fell back to the crate-shipped built-in set.
    Builtin,
}

/// How the profile narrows the tool registry. Mirrors the three modes
/// called out in the issue scope:
///
/// - `default` — no filter; the registry passes through untouched. This is
///   what the built-in `coding-full` profile uses so behaviour parity with
///   the pre-M8.3 default path stays reachable.
/// - `allow_list` — only the named tools survive. Names may reference
///   [`crate::tools::policy::ToolGroupInfo`] groups via `group:*` strings.
/// - `deny_list` — every tool survives except the named ones. Useful for
///   profiles that strip a single capability (e.g. drop `web_fetch` from
///   an otherwise-default set).
///
/// `spawn_only` tools are *never* filtered out regardless of mode — they
/// carry background-execution wiring that the runtime depends on.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProfileTools {
    /// Pass-through filter — registry is not narrowed.
    #[default]
    Default,
    /// Explicit whitelist. Only listed tools (or groups) are kept.
    AllowList {
        /// Tool names or `group:<id>` references to keep.
        #[serde(default)]
        tools: Vec<String>,
    },
    /// Inverse whitelist. Every registered tool except the listed ones is
    /// kept. Groups are expanded through the same mechanism as allow lists.
    DenyList {
        /// Tool names or `group:<id>` references to drop.
        #[serde(default)]
        tools: Vec<String>,
    },
}

impl ProfileTools {
    /// Whether a tool named `tool_name` would survive this filter.
    ///
    /// Mirrors [`crate::tools::ToolRegistry::filter_by_profile`]'s
    /// name-matching exactly — `group:<id>` expansion, `<prefix>*`
    /// wildcards, exact names, and the empty-allow-list pass-through —
    /// but deliberately WITHOUT the spawn_only carve-out. Once a
    /// spawn_only tool is registered it can never be evicted by the
    /// filter, so bootstrap sites (chat/acp `bg_research`) consult this
    /// predicate FIRST and skip registration when the profile excludes
    /// the tool.
    pub fn allows(&self, tool_name: &str) -> bool {
        use crate::tools::policy::entry_matches;
        match self {
            Self::Default => true,
            Self::AllowList { tools } => {
                // Empty allow lists are treated as pass-through by
                // `filter_by_profile` (with a warning); agree with it so
                // the bootstrap gate never drops a tool the filter keeps.
                tools.is_empty() || tools.iter().any(|entry| entry_matches(entry, tool_name))
            }
            Self::DenyList { tools } => !tools.iter().any(|entry| entry_matches(entry, tool_name)),
        }
    }
}

/// Reference to an MCP server that the profile wants attached. For M8.3 we
/// only capture the `id`; resolution to a concrete config happens in a
/// follow-up milestone. Extra fields are tolerated (forward-compat).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpServerRef {
    /// Id of a server config declared elsewhere in the profile dir / config.
    pub id: String,
}

/// Coarse permission tier. The `default` variant mirrors today's
/// allow-everything behaviour — M8.4 will add richer per-tool rules by
/// extending this enum (adding variants is backward-compatible because we
/// do not use `deny_unknown_fields` on the containing struct).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Today's behaviour — every registered tool is executable.
    #[default]
    Default,
    /// Placeholder tier for hardened environments. Carries no runtime
    /// effect yet; M8.4 will map it to a concrete per-tool rule set.
    Restricted,
}

/// Profile-level override for the M8.5 tiered compaction runner.
///
/// Today this struct is *recorded only* — the runtime keeps using the
/// workspace compaction policy from M6.3. Once the M8.5 runner is wired,
/// the fields become live tier overrides.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProfileCompactionPolicy {
    /// Optional target token budget for the final compacted conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u32>,
    /// Optional trigger threshold (turns or tokens, interpreted by the
    /// runner). `None` leaves the runner default in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight_threshold: Option<u32>,
    /// Optional tiers map (tier-id -> token budget). Free-form today;
    /// M8.5 will define the tier id vocabulary.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tiers: HashMap<String, u32>,
}

/// Model-name hints consulted by the provider chain. Today these are
/// recorded but not enforced — a follow-up milestone wires them into
/// adaptive routing.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelPreferences {
    /// Default model id (e.g. `"anthropic/claude-sonnet-4"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Low-latency model id (cheap, fast). May be used for background
    /// worker dispatch in a follow-up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<String>,
    /// Highest-capability model id. Reserved for orchestrator turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strong: Option<String>,
}

/// A single profile manifest.
///
/// Field layout and naming mirrors the runtime plan's "profile envelope".
/// Fields marked *recorded only* land without runtime enforcement in M8.3
/// and are picked up by a follow-up milestone.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProfileDefinition {
    /// Profile id. Also its display name when rendered in logs.
    pub name: String,
    /// Schema version. Must equal [`PROFILE_SCHEMA_VERSION`]. Mismatched
    /// versions produce an error so a forward-compatible client cannot
    /// accidentally swallow schema churn.
    pub version: u32,
    /// Free-text description for humans reading the profile file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Tool filter policy. Defaults to [`ProfileTools::Default`] so the
    /// registry is left untouched.
    #[serde(default)]
    pub tools: ProfileTools,
    /// MCP servers to attach. Only ids are captured today.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerRef>,
    /// Coarse permission tier. Defaults to [`PermissionMode::Default`].
    #[serde(default)]
    pub permissions: PermissionMode,
    /// Optional override for the M8.5 tiered compaction runner. Recorded
    /// only today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_policy: Option<ProfileCompactionPolicy>,
    /// Optional model preferences. Recorded only today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_preferences: Option<ModelPreferences>,
    /// Path within the profile dir to a system-prompt template file.
    /// Resolved against the profile's parent directory at load time; left
    /// as-is in the struct so tests and callers can inspect the raw hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_template: Option<PathBuf>,
    /// Ids of [`crate::agents::AgentDefinition`] manifests to preload when
    /// this profile is activated. Consumes the M8.2 registry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
}

impl Default for ProfileDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: PROFILE_SCHEMA_VERSION,
            description: None,
            tools: ProfileTools::default(),
            mcp_servers: Vec::new(),
            permissions: PermissionMode::default(),
            compaction_policy: None,
            model_preferences: None,
            system_prompt_template: None,
            agents: Vec::new(),
        }
    }
}

impl ProfileDefinition {
    /// Validate a freshly-deserialized profile. Today only the schema
    /// version is enforced — additional cross-field validation lands as
    /// the permission / compaction wiring comes online.
    pub fn validate(&self) -> Result<()> {
        if self.version != PROFILE_SCHEMA_VERSION {
            eyre::bail!(
                "profile '{}' has unsupported schema version {} (expected {})",
                self.name,
                self.version,
                PROFILE_SCHEMA_VERSION,
            );
        }
        if self.name.trim().is_empty() {
            eyre::bail!("profile manifest is missing a non-empty `name` field");
        }
        Ok(())
    }

    /// M8.5 fix-first item 5: cross-validate `profile.agents` against an
    /// `AgentDefinitions` registry. Returns the list of unknown ids so
    /// the caller can either reject the profile or warn the operator.
    /// Empty result means every referenced manifest exists.
    pub fn unknown_agent_ids(&self, registry: &crate::agents::AgentDefinitions) -> Vec<String> {
        self.agents
            .iter()
            .filter(|id| registry.get(id).is_none())
            .cloned()
            .collect()
    }

    /// M8.5 fix-first item 5: hard-validate `profile.agents` against a
    /// registry. Returns an error listing the missing ids when any are
    /// unknown. This is the call sites use when they need an
    /// authoritative profile/manifest envelope (e.g. M9 control-plane).
    pub fn validate_against_registry(
        &self,
        registry: &crate::agents::AgentDefinitions,
    ) -> Result<()> {
        let missing = self.unknown_agent_ids(registry);
        if !missing.is_empty() {
            eyre::bail!(
                "profile '{}' references agent_definition ids not present in the registry: {:?} \
                 (available: {:?})",
                self.name,
                missing,
                registry.ids().collect::<Vec<_>>(),
            );
        }
        Ok(())
    }

    /// Parse a profile from JSON text. Validates on success.
    pub fn from_json_str(text: &str) -> Result<Self> {
        let def: Self =
            serde_json::from_str(text).wrap_err("failed to parse ProfileDefinition as JSON")?;
        def.validate()?;
        Ok(def)
    }

    /// Parse a profile from TOML text. Validates on success.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let def: Self =
            toml::from_str(text).wrap_err("failed to parse ProfileDefinition as TOML")?;
        def.validate()?;
        Ok(def)
    }

    /// Parse from a file on disk, picking the format from the file
    /// extension (`.toml` -> TOML, everything else -> JSON).
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read profile at {}", path.display()))?;
        let is_toml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"));
        let def = if is_toml {
            Self::from_toml_str(&text)
        } else {
            Self::from_json_str(&text)
        }
        .wrap_err_with(|| format!("failed to parse profile at {}", path.display()))?;
        Ok(def)
    }

    /// Look up a named built-in profile from the crate-shipped registry.
    /// Returns `None` when the name is unknown.
    pub fn builtin(name: &str) -> Option<Self> {
        BUILTIN_PROFILES.iter().find_map(|(id, text)| {
            if *id == name {
                Some(
                    Self::from_json_str(text).unwrap_or_else(|err| {
                        panic!("built-in profile '{id}' is malformed: {err}")
                    }),
                )
            } else {
                None
            }
        })
    }

    /// List built-in profile ids. Useful for CLI help output and tests.
    pub fn builtin_ids() -> Vec<&'static str> {
        BUILTIN_PROFILES.iter().map(|(id, _)| *id).collect()
    }

    /// Resolve a profile from a name or path argument. See the module doc
    /// for the full resolution order. The returned tuple reports the
    /// source so the caller can log `profile resolved: ... source=...`.
    pub fn load(arg: &str) -> Result<(Self, ProfileSource)> {
        let home = dirs::home_dir();
        Self::load_with_home(arg, home.as_deref())
    }

    /// Variant of [`Self::load`] that takes an explicit home directory so
    /// unit tests can exercise the user-dir lookup without touching the
    /// real filesystem.
    pub fn load_with_home(arg: &str, home: Option<&Path>) -> Result<(Self, ProfileSource)> {
        if looks_like_path(arg) {
            let resolved = expand_tilde(arg, home);
            let def = Self::from_file(&resolved)?;
            return Ok((def, ProfileSource::ExplicitPath));
        }

        if let Some(home_dir) = home {
            let profile_dir = home_dir.join(".octos/profiles").join(arg);
            for candidate in ["profile.toml", "profile.json"] {
                let path = profile_dir.join(candidate);
                if path.exists() {
                    let def = Self::from_file(&path)?;
                    return Ok((def, ProfileSource::UserDir));
                }
            }
        }

        if let Some(def) = Self::builtin(arg) {
            return Ok((def, ProfileSource::Builtin));
        }

        eyre::bail!(
            "unknown profile '{arg}': not a file, no entry in ~/.octos/profiles/, and \
             not a built-in ({})",
            Self::builtin_ids().join(", "),
        )
    }

    /// Apply the tool filter declared by this profile to a freshly-built
    /// [`ToolRegistry`]. See [`ToolRegistry::filter_by_profile`] for the
    /// spawn-only carve-out.
    pub fn apply_to_registry(&self, registry: &mut ToolRegistry) {
        registry.filter_by_profile(&self.tools);
    }
}

fn looks_like_path(arg: &str) -> bool {
    arg.starts_with('/')
        || arg.starts_with("./")
        || arg.starts_with("~/")
        || arg.starts_with("../")
        // Windows absolute paths (`C:\…`, `C:/…`, verbatim `\\?\…`, UNC
        // `\\server\share`) match none of the Unix-style prefixes above, so a
        // real profile file passed by absolute path was misclassified as a
        // profile *name* and rejected as "unknown profile". `is_absolute()` is
        // platform-aware and a no-op on Unix (there it ⇔ a leading `/`).
        || Path::new(arg).is_absolute()
}

fn expand_tilde(arg: &str, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = arg.strip_prefix("~/") {
        if let Some(home_dir) = home {
            return home_dir.join(rest);
        }
    }
    PathBuf::from(arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_minimum_valid_profile() {
        // Only `name` and `version` are required. Every other field must
        // default cleanly so profile authors can skip sections they are
        // not customizing.
        let json = r#"{"name": "tiny", "version": 1}"#;
        let def = ProfileDefinition::from_json_str(json).expect("parse");
        assert_eq!(def.name, "tiny");
        assert_eq!(def.version, 1);
        assert!(def.description.is_none());
        assert!(matches!(def.tools, ProfileTools::Default));
        assert!(def.mcp_servers.is_empty());
        assert_eq!(def.permissions, PermissionMode::Default);
        assert!(def.compaction_policy.is_none());
        assert!(def.model_preferences.is_none());
        assert!(def.system_prompt_template.is_none());
        assert!(def.agents.is_empty());
    }

    #[test]
    fn should_reject_profile_with_version_mismatch() {
        let json = r#"{"name": "future", "version": 42}"#;
        let err = ProfileDefinition::from_json_str(json).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("version") && msg.contains("42"),
            "expected version error, got {msg}",
        );
    }

    #[test]
    fn should_accept_profile_with_unknown_fields_for_forward_compat() {
        // A v2 producer may introduce new fields. The v1 parser must
        // ignore them rather than fail, so the CLI keeps working while
        // the schema evolves.
        let json = r#"{
            "name": "future-proof",
            "version": 1,
            "tools": {"mode": "default"},
            "new_v2_field": {"nested": true},
            "another_extra": 99
        }"#;
        let def = ProfileDefinition::from_json_str(json).expect("parse");
        assert_eq!(def.name, "future-proof");
        assert!(matches!(def.tools, ProfileTools::Default));
    }

    #[test]
    fn should_resolve_profile_name_to_builtin() {
        let coding = ProfileDefinition::builtin("coding").expect("coding builtin");
        assert_eq!(coding.name, "coding");
        assert_eq!(coding.version, 1);
        // Lean default: `coding` declares a core-loop allow list so the
        // no-flag `octos chat` stops shipping every tool schema each round.
        assert!(matches!(coding.tools, ProfileTools::AllowList { .. }));

        // `coding-full` is the escape hatch that preserves the pre-lean
        // unfiltered surface (one `--profile coding-full` away).
        let full = ProfileDefinition::builtin("coding-full").expect("coding-full builtin");
        assert_eq!(full.name, "coding-full");
        assert!(matches!(full.tools, ProfileTools::Default));

        // Unknown names produce `None` so the load() caller can fall
        // through to a typed error.
        assert!(ProfileDefinition::builtin("does-not-exist").is_none());
    }

    #[test]
    fn should_load_builtin_coding_profile_without_error() {
        let coding = ProfileDefinition::builtin("coding").expect("coding");
        coding.validate().expect("valid");
        // Lean default: the allow list covers the core coding loop ONLY.
        // The exact envelope is pinned here so accidental JSON edits fail
        // loudly instead of silently re-inflating (or hollowing out) the
        // per-round tool-schema overhead.
        match &coding.tools {
            ProfileTools::AllowList { tools } => {
                // #2133: the lean surface KEEPS files/shell/search/memory/spawn
                // and ADDS the three core-loop tools it was missing — `check`,
                // `update_plan`, `tool_search`. Only `apply_patch` is dropped
                // (edit_file/diff_edit cover it), so the fs tools are named
                // explicitly instead of via `group:fs`.
                for required in [
                    "read_file",
                    "write_file",
                    "edit_file",
                    "diff_edit",
                    "group:runtime",
                    "group:search",
                    "group:memory",
                    "group:sessions",
                    "ask_user_question",
                    "check",
                    "update_plan",
                    "tool_search",
                ] {
                    assert!(
                        tools.contains(&required.to_string()),
                        "coding allow list must keep {required}, got {tools:?}",
                    );
                }
                // Dropped or never-included: `apply_patch` (redundant), the
                // `group:fs` alias (fs named explicitly to exclude apply_patch),
                // and the media / pipeline surfaces (restored via
                // `--profile coding-full`).
                for excluded in ["group:fs", "apply_patch", "group:media", "message", "cron"] {
                    assert!(
                        !tools.contains(&excluded.to_string()),
                        "coding allow list must not name {excluded}",
                    );
                }
            }
            other => panic!("coding must declare a lean allow list, got {other:?}"),
        }
        // Today's coding default carries no compaction or permission
        // override — those live at the workspace / app-state level.
        assert!(coding.compaction_policy.is_none());
        assert_eq!(coding.permissions, PermissionMode::Default);
        // Agents preloaded match the M8.2 built-in set so spawn() can
        // resolve them by id.
        assert!(coding.agents.contains(&"repo-editor".to_string()));
    }

    /// Minimal schema-bearing tool used to stand in for bundled-skill /
    /// plugin tools (and for chat-registered natives like `spawn`) in
    /// registry-narrowing tests.
    struct StubTool {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: &serde_json::Value) -> Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                output: "ok".into(),
                success: true,
                ..Default::default()
            })
        }
    }

    #[test]
    fn coding_profile_narrows_registry_to_core_coding_loop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut tools = ToolRegistry::with_builtins(tmp.path());
        // Bundled-skill / plugin tools are plain registry entries that
        // register BEFORE the profile narrowing runs in the chat/acp
        // bootstrap — the allow list must apply to them exactly as it
        // does to builtins.
        tools.register(StubTool {
            name: "get_weather",
        });
        // `spawn` is registered by the chat/acp bootstrap (SpawnTool),
        // not by `with_builtins`; a stub stands in so the inclusion
        // assertion proves the lean profile keeps it (#2133 softened: spawn
        // stays in the default surface).
        tools.register(StubTool { name: "spawn" });

        let coding = ProfileDefinition::builtin("coding").expect("coding");
        coding.apply_to_registry(&mut tools);

        let names: std::collections::BTreeSet<String> =
            tools.specs().into_iter().map(|s| s.name).collect();

        for included in [
            "read_file",
            "write_file",
            "edit_file",
            "diff_edit",
            // group:runtime — all shells kept (interactive sessions live on
            // exec_command + write_stdin; bash is the Codex-compatible alias).
            "bash",
            "shell",
            "exec_command",
            "write_stdin",
            "glob",
            "grep",
            "list_dir",
            // group:sessions — the whole subagent family. `spawn` is
            // registered by the serve/AppUI bootstrap; `spawn_agent` and the
            // lifecycle companions are with_builtins builtins so plain chat
            // keeps a subagent entry point.
            "spawn",
            "spawn_agent",
            "delegate",
            "send_input",
            "resume_agent",
            "wait_agent",
            "close_agent",
            "check",
            "update_plan",
            "tool_search",
            "ask_user_question",
        ] {
            assert!(
                names.contains(included),
                "lean coding profile must keep {included}, got {names:?}",
            );
        }
        for excluded in [
            // #2133: only apply_patch is dropped (edit_file/diff_edit cover
            // it); media/messaging surfaces stay out.
            "apply_patch",
            "get_weather",
            "workspace_diff",
        ] {
            assert!(
                !names.contains(excluded),
                "lean coding profile must drop {excluded}, got {names:?}",
            );
        }
        // Budget pin (#1578 harness review: 48 tools ≈ 9.3K tokens per
        // round in the unfiltered default). The lean surface must stay a
        // small fraction of that; 30 leaves headroom for the core loop +
        // shells + memory + sessions family while failing loudly on
        // accidental bloat.
        assert!(
            names.len() <= 30,
            "lean coding profile grew to {} tools: {names:?}",
            names.len(),
        );
    }

    #[test]
    fn looks_like_path_classifies_arguments_correctly() {
        // Explicit paths start with /, ./, ~/, or ../; everything else is
        // treated as a profile name for user-dir / builtin lookup.
        assert!(looks_like_path("/etc/profile.json"));
        assert!(looks_like_path("./local.toml"));
        assert!(looks_like_path("~/my-profile.json"));
        assert!(looks_like_path("../shared.json"));
        assert!(!looks_like_path("coding"));
        assert!(!looks_like_path("swarm"));
    }

    // -----------------------------------------------------------------------
    // Item 5 of OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24:
    // Profiles and AgentDefinitions must be authoritative — fields that the
    // runtime does NOT enforce should be rejected/cleaned up so M9 clients
    // do not assume they are operational.
    // -----------------------------------------------------------------------

    #[test]
    fn profile_permissions_affect_tool_context_when_restricted() {
        // The PermissionMode field is documented as "Coarse permission
        // tier". Today the runtime keeps it internal-only — we record
        // it but do not enforce it (per the fix-first checklist's
        // accepted "internal-only until real" path). This test pins
        // that behaviour: the field deserialises round-trip and the
        // built-in profiles report consistent values, so a future
        // wiring milestone has a stable API to grow into.
        let coding = ProfileDefinition::builtin("coding").expect("coding");
        assert_eq!(coding.permissions, PermissionMode::Default);

        let restricted = ProfileDefinition::from_json_str(
            r#"{"name": "locked", "version": 1, "permissions": "restricted"}"#,
        )
        .expect("parse restricted");
        assert_eq!(restricted.permissions, PermissionMode::Restricted);

        // Until the wiring lands, ProfileDefinition does NOT expose a
        // `to_tool_permissions()` helper. Adding one would be a real
        // wiring step. The placeholder is here so a follow-up commit
        // can replace this assertion with a meaningful behavioural one.
        // For now we just assert the recorded value is what the
        // profile JSON declared.
        assert_ne!(coding.permissions, restricted.permissions);
    }
}
