use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::abi_schema::{
    COMPACTION_POLICY_SCHEMA_VERSION, WORKSPACE_POLICY_SCHEMA_VERSION, check_supported,
    default_compaction_policy_schema_version, default_workspace_policy_schema_version,
};
pub const WORKSPACE_POLICY_FILE: &str = ".octos-workspace.toml";

/// Harness-facing workspace policy.
///
/// `schema_version` is the durable ABI version; see
/// `docs/OCTOS_HARNESS_ABI_VERSIONING.md` for the stable and experimental
/// fields per version. Older policy files that omit the field are accepted
/// as v1 on deserialization.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspacePolicy {
    /// Durable ABI schema version for this policy. Defaults to
    /// [`WORKSPACE_POLICY_SCHEMA_VERSION`] when absent so pre-versioned
    /// policies continue to load.
    #[serde(default = "default_workspace_policy_schema_version")]
    pub schema_version: u32,
    pub workspace: WorkspacePolicyWorkspace,
    pub version_control: WorkspaceVersionControlPolicy,
    pub tracking: WorkspaceTrackingPolicy,
    #[serde(default)]
    pub artifacts: WorkspaceArtifactsPolicy,
    #[serde(default)]
    pub spawn_tasks: BTreeMap<String, WorkspaceSpawnTaskPolicy>,
    /// Declarative compaction contract (harness M6.3). Absent = legacy extractive
    /// behaviour with no preflight or typed placeholders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionPolicy>,
}

/// Harness-facing compaction contract (M6.3).
///
/// Declares the shape of compaction for a workspace: how many tokens to aim
/// for, which declared artifacts must survive the pass, when to pre-emptively
/// compact before the first LLM call, and how aggressively to prune stale
/// tool outputs. When absent, the runtime falls back to the legacy extractive
/// path and behaves exactly as before M6.3.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionPolicy {
    /// Durable ABI schema version. See
    /// [`COMPACTION_POLICY_SCHEMA_VERSION`]. Missing in legacy files;
    /// defaulted to the current version via
    /// [`default_compaction_policy_schema_version`].
    #[serde(default = "default_compaction_policy_schema_version")]
    pub schema_version: u32,
    /// Target token budget for the compacted conversation after a pass.
    pub token_budget: u32,
    /// Artifact names (keys in `artifacts`) whose declared patterns MUST be
    /// referenced at least once in the compacted message stream. Failure here
    /// trips the validator rail and blocks terminal success.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserved_artifacts: Vec<String>,
    /// Free-form substrings that must survive compaction (e.g. a workspace
    /// invariant flag string). Matched verbatim against message content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserved_invariants: Vec<String>,
    /// Summarizer flavour to use for the compaction pass. Defaults to the
    /// extractive variant until M6.4 wires the LLM-iterative implementation.
    #[serde(default)]
    pub summarizer: CompactionSummarizerKind,
    /// Trigger preflight compaction before the first LLM call when the
    /// conversation already exceeds this token count. `None` disables
    /// preflight entirely (post-call compaction still runs on overflow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight_threshold: Option<u32>,
    /// Replace tool results older than N user-turn boundaries with a typed
    /// `ToolResultPlaceholder`. `None` keeps tool results intact until the
    /// usual token-budget path kicks in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune_tool_results_after_turns: Option<u32>,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
            token_budget: 8_000,
            preserved_artifacts: Vec::new(),
            preserved_invariants: Vec::new(),
            summarizer: CompactionSummarizerKind::default(),
            preflight_threshold: None,
            prune_tool_results_after_turns: None,
        }
    }
}

/// Summarizer strategy declared in a [`CompactionPolicy`]. The runtime maps
/// this to an implementation of [`crate::summarizer::Summarizer`] at wire
/// time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionSummarizerKind {
    /// Deterministic extractive summarizer (preserves legacy behaviour).
    #[default]
    Extractive,
    /// LLM-iterative summarizer. Lands in M6.4; the extractive summarizer is
    /// used as a fallback in the current runtime.
    LlmIterative,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePolicyWorkspace {
    pub kind: WorkspacePolicyKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspacePolicyKind {
    Session,
    /// Coding workspaces — projects with a recognised manifest
    /// (Cargo.toml / package.json / pyproject.toml). Inherits the
    /// `Session` spawn-task contracts and adds AfterTool hooks that
    /// run `cargo check` (and optionally eslint / ruff when present
    /// on PATH) after `edit_file` / `write_file` / `diff_edit` calls
    /// scoped to the language's file extensions. Audit Gap-1 + Q3.
    Coding,
}

impl WorkspacePolicyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Coding => "coding",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceVersionControlPolicy {
    pub provider: WorkspaceVersionControlProvider,
    pub auto_init: bool,
    pub trigger: WorkspaceSnapshotTrigger,
    pub fail_on_error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceVersionControlProvider {
    Git,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSnapshotTrigger {
    TurnEnd,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTrackingPolicy {
    pub ignore: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkspaceArtifactsPolicy {
    #[serde(flatten)]
    pub entries: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct WorkspaceSpawnTaskPolicy {
    #[serde(default)]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub on_verify: Vec<String>,
    /// Legacy completion hook retained for compatibility. Prefer `on_deliver`
    /// for explicit handoff/delivery actions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_complete: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_deliver: Vec<String>,
    #[serde(default)]
    pub on_failure: Vec<String>,
}
impl WorkspaceSpawnTaskPolicy {
    pub fn artifact_sources(&self) -> Vec<&str> {
        if self.artifacts.is_empty() {
            self.artifact.iter().map(String::as_str).collect()
        } else {
            self.artifacts.iter().map(String::as_str).collect()
        }
    }

    pub fn delivery_actions(&self) -> &[String] {
        if self.on_deliver.is_empty() {
            &self.on_complete
        } else {
            &self.on_deliver
        }
    }
}

impl WorkspacePolicy {
    pub fn for_session() -> Self {
        Self {
            schema_version: WORKSPACE_POLICY_SCHEMA_VERSION,
            workspace: WorkspacePolicyWorkspace {
                kind: WorkspacePolicyKind::Session,
            },
            version_control: WorkspaceVersionControlPolicy {
                provider: WorkspaceVersionControlProvider::Git,
                auto_init: false,
                trigger: WorkspaceSnapshotTrigger::TurnEnd,
                fail_on_error: false,
            },
            tracking: WorkspaceTrackingPolicy {
                ignore: vec!["tmp/**".into(), ".DS_Store".into()],
            },
            artifacts: WorkspaceArtifactsPolicy::default(),
            spawn_tasks: BTreeMap::new(),
            compaction: None,
        }
    }

    /// Default policy for a coding workspace (Audit Gap-1 + section 7 Q3).
    ///
    /// Inherits the [`for_session`] baseline and overlays only the
    /// `kind = Coding` marker. The AfterTool `cargo check` / `eslint` /
    /// `ruff` hooks live in [`coding_default_hooks`] so the host (chat.rs /
    /// gateway.rs / serve.rs) can merge them into its `HookExecutor` without
    /// forking the hook runner.
    ///
    /// The split is deliberate: hooks are runtime side-effects executed by
    /// the agent, while `WorkspacePolicy` is a serialized declaration shared
    /// with the LLM. Stuffing process-launch hooks into the workspace policy
    /// would (a) leak operator-side state into a contract the LLM can read
    /// and (b) force every embedder of the policy struct (config_watcher,
    /// session bootstrap, REST inspectors) to know how to run shells.
    pub fn for_coding() -> Self {
        let mut policy = Self::for_session();
        policy.workspace.kind = WorkspacePolicyKind::Coding;
        policy
    }
}

/// Detect the workspace policy kind for `cwd` by probing for well-known
/// language manifests. Order: `Cargo.toml` → `package.json` → `pyproject.toml`
/// → fallback [`WorkspacePolicyKind::Session`].
///
/// This is the entry point for Audit Gap-1's "the harness owns the contract"
/// stance — the runtime decides whether a workspace is `Coding` based on
/// observable filesystem signals, not LLM input. Operators who want to
/// override the inference write an explicit `.octos-workspace.toml`.
pub fn detect_workspace_policy_kind(cwd: &Path) -> WorkspacePolicyKind {
    if cwd.join("Cargo.toml").is_file()
        || cwd.join("package.json").is_file()
        || cwd.join("pyproject.toml").is_file()
    {
        WorkspacePolicyKind::Coding
    } else {
        WorkspacePolicyKind::Session
    }
}

/// Default `after_tool_call` hooks for a [`WorkspacePolicyKind::Coding`]
/// workspace (Audit Gap-1 closure).
///
/// Each entry mirrors the canonical `cargo check` / `eslint` / `ruff`
/// invocation an operator would write by hand. Hosts (chat.rs, gateway.rs,
/// serve.rs) call this on bootstrap and merge the returned hooks into the
/// HookExecutor they hand to the agent. Operator-defined hooks always merge
/// AFTER these defaults, so an operator-written `cargo check` hook with
/// stricter args overrides the default's behaviour by virtue of running too
/// (both fire, but a stricter one denies before the cheaper one matters).
///
/// `requires_bin` gates each entry on a binary lookup — operators who do not
/// have `eslint` or `ruff` installed see the hooks silently skip rather than
/// failing every after-tool callback. `cargo` is assumed present in a Rust
/// workspace (the detector keys on `Cargo.toml`) but is still gated so
/// nothing breaks when a stub `Cargo.toml` lives alongside a non-cargo
/// project.
pub fn coding_default_hooks() -> Vec<crate::hooks::HookConfig> {
    use crate::hooks::{HookConfig, HookEvent};
    let edit_tools = vec![
        "edit_file".to_string(),
        "write_file".to_string(),
        "diff_edit".to_string(),
    ];
    vec![
        // Rust: `cargo check --message-format=short` on `.rs` edits.
        HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec![
                "cargo".into(),
                "check".into(),
                "--message-format=short".into(),
            ],
            timeout_ms: 60_000,
            tool_filter: edit_tools.clone(),
            path_filter: vec!["**/*.rs".into()],
            requires_bin: Some("cargo".into()),
        },
        // JS/TS: ESLint with zero-warning policy. Skipped if eslint is
        // not on PATH. Operators who use a different linter (biome, etc.)
        // add their own hook; the default is best-effort and explicit.
        HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec!["eslint".into(), "--max-warnings".into(), "0".into()],
            timeout_ms: 60_000,
            tool_filter: edit_tools.clone(),
            // Four explicit patterns: the `glob` crate treats braces as
            // LITERALS, so "**/*.{js,ts,tsx,jsx}" matches no real file
            // (#2129 review, finding 10 — verified empirically).
            path_filter: vec![
                "**/*.js".into(),
                "**/*.ts".into(),
                "**/*.tsx".into(),
                "**/*.jsx".into(),
            ],
            requires_bin: Some("eslint".into()),
        },
        // Python: `ruff check` on `.py` edits.
        HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec!["ruff".into(), "check".into()],
            timeout_ms: 60_000,
            tool_filter: edit_tools,
            path_filter: vec!["**/*.py".into()],
            requires_bin: Some("ruff".into()),
        },
    ]
}

pub fn workspace_policy_path(project_root: &Path) -> PathBuf {
    project_root.join(WORKSPACE_POLICY_FILE)
}

pub fn read_workspace_policy(project_root: &Path) -> Result<Option<WorkspacePolicy>> {
    let path = workspace_policy_path(project_root);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .wrap_err_with(|| format!("read workspace policy failed: {}", path.display()))?;
    let policy: WorkspacePolicy = toml::from_str(&raw)
        .wrap_err_with(|| format!("parse workspace policy failed: {}", path.display()))?;
    check_supported(
        "WorkspacePolicy",
        policy.schema_version,
        WORKSPACE_POLICY_SCHEMA_VERSION,
    )
    .wrap_err_with(|| format!("incompatible workspace policy: {}", path.display()))?;

    Ok(Some(policy))
}

pub fn write_workspace_policy(project_root: &Path, policy: &WorkspacePolicy) -> Result<()> {
    std::fs::create_dir_all(project_root)
        .wrap_err_with(|| format!("create project dir failed: {}", project_root.display()))?;
    let path = workspace_policy_path(project_root);
    let rendered = toml::to_string_pretty(policy)
        .wrap_err_with(|| format!("serialize workspace policy failed: {}", path.display()))?;
    std::fs::write(&path, rendered)
        .wrap_err_with(|| format!("write workspace policy failed: {}", path.display()))?;
    Ok(())
}

/// Variant of [`write_workspace_policy`] that fails closed when a
/// policy file is already present at `project_root`.
///
/// Implemented via a single `open(O_CREAT|O_EXCL)`-equivalent syscall
/// (`std::fs::OpenOptions::write(true).create_new(true)`) so two
/// concurrent callers — or a caller racing an operator hand-edit —
/// can never overwrite an existing `.octos-workspace.toml`.
/// `AlreadyExists` is treated as success, matching the "bootstrap
/// only if absent" idempotency contract M11-C relies on for the
/// per-session workspace policy.
///
/// This is intentionally a separate function from
/// [`write_workspace_policy`] so the legacy caller (which expects
/// truncate-on-write semantics for explicit policy edits) is
/// unchanged.
pub fn write_workspace_policy_if_absent(
    project_root: &Path,
    policy: &WorkspacePolicy,
) -> Result<()> {
    use std::io::Write;

    std::fs::create_dir_all(project_root)
        .wrap_err_with(|| format!("create project dir failed: {}", project_root.display()))?;
    let path = workspace_policy_path(project_root);
    let rendered = toml::to_string_pretty(policy)
        .wrap_err_with(|| format!("serialize workspace policy failed: {}", path.display()))?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => file
            .write_all(rendered.as_bytes())
            .wrap_err_with(|| format!("write workspace policy failed: {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "open workspace policy for create-new failed: {}",
                path.display()
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_task_artifact_sources_prefer_multi_artifact_list() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("legacy".into()),
            artifacts: vec!["report".into(), "audio".into()],
            on_verify: Vec::new(),
            on_complete: Vec::new(),
            on_deliver: Vec::new(),
            on_failure: Vec::new(),
        };

        assert_eq!(task.artifact_sources(), vec!["report", "audio"]);
    }

    #[test]
    fn spawn_task_delivery_actions_prefer_explicit_delivery_list() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec!["notify_user:legacy".into()],
            on_deliver: vec!["notify_user:deliver".into()],
            on_failure: Vec::new(),
        };

        assert_eq!(
            task.delivery_actions(),
            &["notify_user:deliver".to_string()]
        );
    }

    #[test]
    fn should_stamp_current_schema_version_when_building_defaults() {
        let session = WorkspacePolicy::for_session();
        let coding = WorkspacePolicy::for_coding();
        assert_eq!(session.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION);
        assert_eq!(coding.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION);
    }

    #[test]
    fn should_default_missing_schema_version_to_v1_when_loading_legacy_toml() {
        // A TOML emitted before M4.6 — no `schema_version` line.
        let legacy = r#"
[workspace]
kind = "session"

[version_control]
provider = "git"
auto_init = true
trigger = "turn_end"
fail_on_error = true

[tracking]
ignore = ["output/**"]
"#;
        let parsed: WorkspacePolicy = toml::from_str(legacy).expect("legacy policy should parse");
        assert_eq!(parsed.schema_version, WORKSPACE_POLICY_SCHEMA_VERSION);
        assert_eq!(parsed.workspace.kind, WorkspacePolicyKind::Session);
    }

    #[test]
    fn should_reject_future_schema_version_with_actionable_error() {
        // A TOML that claims a version the harness cannot understand.
        let future = format!(
            r#"
schema_version = {}

[workspace]
kind = "session"

[version_control]
provider = "git"
auto_init = true
trigger = "turn_end"
fail_on_error = true

[tracking]
ignore = []
"#,
            WORKSPACE_POLICY_SCHEMA_VERSION + 99
        );
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(WORKSPACE_POLICY_FILE), future).unwrap();

        let err = read_workspace_policy(temp.path()).expect_err("future version should fail");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("schema_version"));
        assert!(rendered.contains("upgrade octos"));
    }

    #[test]
    fn write_workspace_policy_if_absent_creates_file_when_missing() {
        let temp = tempfile::tempdir().unwrap();
        let policy = WorkspacePolicy::for_session();

        write_workspace_policy_if_absent(temp.path(), &policy).unwrap();

        let path = workspace_policy_path(temp.path());
        assert!(path.is_file());
        let roundtrip = read_workspace_policy(temp.path()).unwrap().unwrap();
        assert_eq!(roundtrip, policy);
    }

    #[test]
    fn should_return_rust_check_hook_in_coding_defaults() {
        let hooks = coding_default_hooks();
        let cargo = hooks
            .iter()
            .find(|h| h.command.first().map(String::as_str) == Some("cargo"))
            .expect("cargo check hook present");
        assert_eq!(cargo.event, crate::hooks::HookEvent::AfterToolCall);
        assert!(cargo.path_filter.iter().any(|p| p == "**/*.rs"));
        assert!(cargo.tool_filter.iter().any(|t| t == "edit_file"));
        assert!(cargo.tool_filter.iter().any(|t| t == "write_file"));
        assert!(cargo.tool_filter.iter().any(|t| t == "diff_edit"));
        assert_eq!(cargo.requires_bin.as_deref(), Some("cargo"));
    }

    /// #2129: the session bootstrap path routes a detected coding workspace
    /// to `for_coding()`. This pins the two halves the wiring depends on:
    /// detection keys on the manifest, and the coding policy carries the
    /// Coding kind marker while inheriting the session contracts.
    #[test]
    fn coding_bootstrap_pieces_compose() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(
            detect_workspace_policy_kind(tmp.path()),
            WorkspacePolicyKind::Coding
        );
        let policy = WorkspacePolicy::for_coding();
        assert_eq!(policy.workspace.kind, WorkspacePolicyKind::Coding);
        // The defaults the session merges for that kind include the Rust
        // checker, gated on the binary being present.
        let hooks = coding_default_hooks();
        assert!(
            hooks
                .iter()
                .any(|h| h.command.first().map(String::as_str) == Some("cargo")),
            "coding defaults must include the cargo hook"
        );
    }
}
