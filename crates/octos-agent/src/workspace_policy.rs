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
    pub validation: ValidationPolicy,
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

/// Tiered validation checks run at different points in the turn lifecycle.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ValidationPolicy {
    /// Tier 1: cheap checks run every turn (< 100ms). e.g. file_exists, build exit code.
    #[serde(default)]
    pub on_turn_end: Vec<String>,
    /// Tier 2: medium checks run when source files change (1-5s). e.g. preview render.
    #[serde(default)]
    pub on_source_change: Vec<String>,
    /// Tier 3: expensive checks run on completion/publish only (10-30s). e.g. Playwright.
    #[serde(default)]
    pub on_completion: Vec<String>,
    /// Typed declarative validators (M4.3). Runs via `ValidatorRunner`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validators: Vec<Validator>,
}

/// Typed declarative validator spec.
///
/// Each validator is identified by a stable `id`, produces a typed
/// [`crate::validators::ValidatorOutcome`], and may be `required` (a failure
/// blocks terminal success) or optional (a failure produces a warning only).
///
/// Wave-3a introduced an explicit [`Required::Soft`] tier — surfaced via the
/// `soft_fail` companion field — so partial-artifact contracts can warn and
/// continue without demoting the spawn task. The historic boolean
/// `required` field is preserved verbatim for serde + ABI back-compat; the
/// runtime collapses both fields into a single [`Required`] gate value via
/// [`Validator::tier`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Validator {
    /// Stable identifier, unique within the validator list.
    pub id: String,
    /// Required validators block terminal success when they fail. Soft-fail
    /// validators (see [`Self::soft_fail`]) ignore this flag.
    #[serde(default = "default_required_bool")]
    pub required: bool,
    /// When `true`, a failed outcome surfaces as a warning + ledger entry
    /// but does NOT demote the spawn task — even if `required` is also
    /// `true`. Defaults to `false` so existing policies preserve the
    /// hard-fail semantics they have today. Use this to declare partial-
    /// artifact contracts (e.g. "the primary report is hard-required, the
    /// sub-artifacts are soft").
    #[serde(default, skip_serializing_if = "is_default_false")]
    pub soft_fail: bool,
    /// Optional per-validator timeout in milliseconds. Applies to command and
    /// tool validators. File-existence validators ignore the timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Which lifecycle phase this validator runs in. Defaults to completion.
    #[serde(default, skip_serializing_if = "is_default_phase")]
    pub phase: ValidatorPhaseKind,
    #[serde(flatten)]
    pub spec: ValidatorSpec,
}

impl Validator {
    /// Collapse `required` + `soft_fail` into the operator-visible
    /// gate-strength tier. The mapping is:
    ///
    /// | `required` | `soft_fail` | `tier()`         |
    /// | ---------- | ----------- | ---------------- |
    /// | `true`     | `false`     | [`Required::Hard`] |
    /// | `true`     | `true`      | [`Required::Soft`] |
    /// | `false`    | `false`     | [`Required::None`] |
    /// | `false`    | `true`      | [`Required::Soft`] |
    ///
    /// `soft_fail = true` always overrides the hard semantics so the
    /// validator never demotes its spawn task.
    pub fn tier(&self) -> Required {
        if self.soft_fail {
            Required::Soft
        } else if self.required {
            Required::Hard
        } else {
            Required::None
        }
    }
}

/// Operator-facing strength label for a validator's gate over terminal
/// success. Surfaced through [`Validator::tier`] and the persisted ledger
/// outcome record so dashboards can split hard, soft, and informational
/// failures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Required {
    /// A failure of this validator demotes the spawn task to `Failed`.
    /// Equivalent to the historic `required: true`.
    #[default]
    Hard,
    /// A failure surfaces a warning + persists to the ledger but does NOT
    /// demote the spawn task. Use for sub-artifacts and partial-artifact
    /// contracts where the primary deliverable is hard-required but
    /// auxiliary outputs are nice-to-have.
    Soft,
    /// A failure is fully optional — same gate behaviour as `Soft`, but
    /// operator-visible as "this validator is informational only".
    /// Equivalent to the historic `required: false` with `soft_fail = false`.
    None,
}

impl Required {
    /// Does a non-`Pass` outcome from this validator block terminal success?
    pub fn is_hard(self) -> bool {
        matches!(self, Self::Hard)
    }

    /// Should a non-`Pass` outcome surface as a warning (without demoting
    /// the spawn task)? True for both `Soft` and `None` — operators are free
    /// to filter on the explicit tier via the persisted outcome record.
    pub fn is_warning_only(self) -> bool {
        matches!(self, Self::Soft | Self::None)
    }

    /// Stable label for metrics + ledger records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Soft => "soft",
            Self::None => "none",
        }
    }
}

fn default_required_bool() -> bool {
    true
}

fn is_default_false(value: &bool) -> bool {
    !*value
}

fn is_default_phase(phase: &ValidatorPhaseKind) -> bool {
    *phase == ValidatorPhaseKind::default()
}

/// Where a file-list-driven validator (`MagicBytes`) sources its candidate
/// file paths.
///
/// Defaults to [`ValidatorFileSource::Glob`] so existing TOML contracts keep
/// their historic behaviour: the validator resolves the `glob` field against
/// the workspace root. Opting into [`ValidatorFileSource::SpawnOnlyFiles`]
/// tells the validator to use the originating spawn_only tool's
/// `files_to_send` list verbatim (the plugin protocol's authoritative list
/// of files the skill just produced), optionally filtered by an extension
/// suffix.
///
/// Issue octos #1034: topic-suffixed plugin output directories break
/// globbing because the per-topic directory name is unpredictable.
/// `files_to_send` carries the exact path the plugin wrote and is the
/// canonical source of truth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorFileSource {
    /// Resolve files by glob (the legacy behaviour). The validator's `glob`
    /// field is matched against the workspace root.
    #[default]
    Glob,
    /// Use the originating spawn_only tool's `files_to_send` list, optionally
    /// filtered by a file extension supplied alongside (e.g. `extension =
    /// "mp3"`). The `glob` field is IGNORED in this mode. When the file list
    /// is empty (non-spawn-only contexts) the validator surfaces a `Fail`
    /// outcome so misconfigured policies are caught early.
    SpawnOnlyFiles,
}

impl ValidatorFileSource {
    /// Predicate used by `skip_serializing_if` so a default value does not
    /// pollute TOML output for contracts that did not opt in.
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Glob)
    }
}

/// Lifecycle phase a validator runs in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorPhaseKind {
    /// Runs on every turn end (cheap checks).
    TurnEnd,
    /// Runs on completion / publish (expensive checks).
    #[default]
    Completion,
}

/// The typed body of a [`Validator`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValidatorSpec {
    /// Run a subprocess command. Dispatched via the shell-safety layer and
    /// existing `BLOCKED_ENV_VARS` sanitization. No direct `Command::new("sh")`
    /// bypass.
    Command {
        cmd: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
    /// Invoke a registered agent tool. Outcome status follows the tool's
    /// `ToolResult.success`.
    ToolCall {
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// Assert that a file exists (and optionally meets a minimum byte count).
    FileExists {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_bytes: Option<u64>,
    },
    /// HTTP probe — call URL (with `${args.<path>}` template interpolation
    /// against the spawn task's input args), assert the response is the
    /// expected status code, optionally assert a substring is present in the
    /// response body. Default timeout 5s (overridden by
    /// [`Validator::timeout_ms`]).
    HttpProbe {
        url_template: String,
        #[serde(default = "default_http_probe_status")]
        expected_status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_contains: Option<String>,
    },
    /// Specialization of `HttpProbe` for the common case of asserting
    /// ominix-api has registered a custom voice. Calls
    /// `GET ${OMINIX_API_URL:-http://127.0.0.1:8081}/v1/voices` and
    /// asserts the response's `voices[].name` array contains the
    /// interpolated `name_arg` value. Surfaces the available list in the
    /// failure message so the LLM can react in one round.
    OminixVoiceExists {
        /// Argument key in the spawn task's input args (e.g. `name`) that
        /// holds the voice name to look up.
        name_arg: String,
    },
    /// Assert each file matching `glob` has the magic-byte prefix for the
    /// declared `format`. Catches "tool wrote 0 bytes" or "tool wrote an
    /// HTML error page in place of an MP3".
    ///
    /// The format field is named `format` rather than `kind` to avoid
    /// colliding with serde's `kind` discriminator tag.
    ///
    /// When `source = "spawn_only_files"` (octos #1034) the validator skips
    /// the glob entirely and consumes the originating spawn_only tool's
    /// `files_to_send` list, optionally narrowed by `extension`.
    MagicBytes {
        #[serde(default)]
        glob: String,
        format: MagicByteKind,
        #[serde(default, skip_serializing_if = "ValidatorFileSource::is_default")]
        source: ValidatorFileSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
    },
    /// Polling HTTP probe — repeatedly GET a templated URL (with
    /// `${args.<key>}` interpolation against the spawn task's input args)
    /// until the expected status code (+ optional body substring) is
    /// observed or the deadline expires.
    ///
    /// Closes the silent-failure path where a spawn task kicks off an
    /// asynchronous external operation (training a voice, deploying a site)
    /// whose completion the harness must verify without baking polling logic
    /// into every skill. Emits [`crate::validators::ValidatorStatus::Pass`]
    /// on the first success; [`Fail`] (with the last response summary in
    /// the message) when the deadline expires; [`Timeout`] only if a single
    /// probe within the deadline window itself times out at the HTTP level.
    HttpProbeUntil {
        url_template: String,
        #[serde(default = "default_http_probe_status")]
        expected_status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_contains: Option<String>,
        /// Interval between probe attempts in milliseconds.
        #[serde(default = "default_http_probe_until_interval_ms")]
        poll_interval_ms: u64,
        /// Hard wall-clock deadline in milliseconds. Once reached the
        /// validator emits a [`Fail`] outcome surfacing the most recent
        /// response so the LLM/operator can debug in one round.
        #[serde(default = "default_http_probe_until_deadline_ms")]
        deadline_ms: u64,
    },
    /// Assert a single file's SHA-256 digest equals `sha256`. Accepts either
    /// an explicit hex digest OR a `${args.<key>}` template so the spawn task
    /// can supply the expected hash through its input args (e.g. a manifest
    /// `sha256` field captured at install time). Lifts the inline
    /// `manage_skills::download_binary` checksum check onto the canonical
    /// validator path so it shows up in the contract diagnostics ledger.
    Sha256Match { glob: String, sha256: String },
}

/// File-format signature used by [`ValidatorSpec::MagicBytes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MagicByteKind {
    Mp3,
    Wav,
    Png,
    Jpeg,
    Pdf,
    Mp4,
    WebM,
    /// OOXML / OpenDocument-style ZIP container (PPTX, DOCX, XLSX, ODT, ...).
    /// Matches the three ZIP signatures: local-file-header (`PK\x03\x04`),
    /// end-of-central-directory (`PK\x05\x06`), and spanned-archive
    /// (`PK\x07\x08`).
    Pptx,
}

impl MagicByteKind {
    /// Return the alternative magic-byte prefixes for this file format.
    /// A file matches if any prefix is present at the start of the byte
    /// stream.
    pub fn prefixes(self) -> &'static [&'static [u8]] {
        match self {
            // MP3 with ID3v2 tag, or a raw MPEG frame sync (0xFF Fx/Ex/Dx).
            Self::Mp3 => &[
                b"ID3",
                &[0xFF, 0xFB],
                &[0xFF, 0xFA],
                &[0xFF, 0xF3],
                &[0xFF, 0xF2],
                &[0xFF, 0xE3],
                &[0xFF, 0xE2],
            ],
            Self::Wav => &[b"RIFF"],
            Self::Png => &[&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]],
            Self::Jpeg => &[&[0xFF, 0xD8, 0xFF]],
            Self::Pdf => &[b"%PDF-"],
            // MP4: 4-byte size prefix followed by 'ftyp'. Most MP4s also
            // start with 'ftyp' offset by 4 bytes, but checking the brand
            // directly is simpler — see `magic_bytes_match`.
            Self::Mp4 => &[b"ftyp"],
            Self::WebM => &[&[0x1A, 0x45, 0xDF, 0xA3]],
            // PPTX (and any OOXML/zip container): all three ZIP signatures
            // are accepted so a minimally-built archive is not rejected as
            // structurally invalid.
            Self::Pptx => &[
                &[0x50, 0x4B, 0x03, 0x04],
                &[0x50, 0x4B, 0x05, 0x06],
                &[0x50, 0x4B, 0x07, 0x08],
            ],
        }
    }

    /// Does `data` start with one of the prefixes for this format?
    ///
    /// For MP4, the `ftyp` marker lives at offset 4 (after the box-size
    /// prefix), so the check is byte-position aware. For other formats the
    /// prefix is at the beginning.
    pub fn matches(self, data: &[u8]) -> bool {
        if self == Self::Mp4 {
            // MP4: bytes 4..8 must be 'ftyp'.
            return data.len() >= 8 && &data[4..8] == b"ftyp";
        }
        let prefixes = self.prefixes();
        prefixes.iter().any(|p| data.starts_with(p))
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Pdf => "pdf",
            Self::Mp4 => "mp4",
            Self::WebM => "webm",
            Self::Pptx => "pptx",
        }
    }
}

fn default_http_probe_status() -> u16 {
    200
}

fn default_http_probe_until_interval_ms() -> u64 {
    2_000
}

fn default_http_probe_until_deadline_ms() -> u64 {
    30_000
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
    /// Per-spawn-task typed validators run at the completion gate, in
    /// addition to the workspace-wide `[validation].validators`. Each entry
    /// is auto-tagged as required+completion phase; pass an explicit
    /// `Validator` struct (with `id`, `required`, etc.) for finer control.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_completion: Vec<SpawnTaskValidatorSpec>,
}

/// TOML-friendly wrapper for the per-spawn-task `on_completion` validator
/// list. Accepts either:
///
/// * A bare `ValidatorSpec` table (no `id`/`required`/`phase`) — auto-tagged
///   as required + completion phase + a synthetic `id` derived from the
///   spawn task name and validator index.
/// * A full `Validator` table with `id`, `required`, `timeout_ms`, etc.
///
/// Both forms surface to the runner as a [`Validator`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SpawnTaskValidatorSpec {
    /// Full Validator struct with `id`, `required`, etc.
    Full(Validator),
    /// Bare spec table — id, required, and phase are auto-filled by
    /// [`SpawnTaskValidatorSpec::into_validator`].
    Bare(ValidatorSpec),
}

impl SpawnTaskValidatorSpec {
    /// Lower this entry into a fully-formed `Validator` using `task_name`
    /// and `index` to synthesize a stable id when only a bare spec was
    /// provided.
    pub fn into_validator(self, task_name: &str, index: usize) -> Validator {
        match self {
            Self::Full(validator) => validator,
            Self::Bare(spec) => Validator {
                id: format!("{task_name}.on_completion[{index}]"),
                required: true,
                soft_fail: false,
                timeout_ms: None,
                phase: ValidatorPhaseKind::Completion,
                spec,
            },
        }
    }
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
            validation: ValidationPolicy::default(),
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
            on_completion: Vec::new(),
        };

        assert_eq!(task.artifact_sources(), vec!["report", "audio"]);
    }

    #[test]
    fn spawn_task_artifact_sources_fall_back_to_single_artifact() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: Vec::new(),
            on_deliver: Vec::new(),
            on_failure: Vec::new(),
            on_completion: Vec::new(),
        };

        assert_eq!(task.artifact_sources(), vec!["primary_audio"]);
    }

    #[test]
    fn spawn_task_artifact_sources_roundtrip_omits_empty_list() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: vec!["file_exists:$artifact".into()],
            on_complete: Vec::new(),
            on_deliver: Vec::new(),
            on_failure: Vec::new(),
            on_completion: Vec::new(),
        };

        let rendered = toml::to_string_pretty(&task).unwrap();
        assert!(!rendered.contains("artifacts = []"));
        let roundtrip: WorkspaceSpawnTaskPolicy = toml::from_str(&rendered).unwrap();
        assert_eq!(roundtrip.artifact_sources(), vec!["primary_audio"]);
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
            on_completion: Vec::new(),
        };

        assert_eq!(
            task.delivery_actions(),
            &["notify_user:deliver".to_string()]
        );
    }

    #[test]
    fn spawn_task_delivery_actions_fall_back_to_legacy_completion_list() {
        let task = WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec!["notify_user:legacy".into()],
            on_deliver: Vec::new(),
            on_failure: Vec::new(),
            on_completion: Vec::new(),
        };

        assert_eq!(task.delivery_actions(), &["notify_user:legacy".to_string()]);
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
    fn should_roundtrip_typed_validators_through_toml() {
        let mut policy = WorkspacePolicy::for_session();
        policy.validation.validators = vec![
            Validator {
                id: "cmd".into(),
                required: true,
                soft_fail: false,
                timeout_ms: Some(3000),
                phase: ValidatorPhaseKind::Completion,
                spec: ValidatorSpec::Command {
                    cmd: "echo".into(),
                    args: vec!["hello".into()],
                },
            },
            Validator {
                id: "file".into(),
                required: false,
                soft_fail: false,
                timeout_ms: None,
                phase: ValidatorPhaseKind::TurnEnd,
                spec: ValidatorSpec::FileExists {
                    path: "out.txt".into(),
                    min_bytes: Some(128),
                },
            },
            Validator {
                id: "tool".into(),
                required: true,
                soft_fail: false,
                timeout_ms: Some(5000),
                phase: ValidatorPhaseKind::Completion,
                spec: ValidatorSpec::ToolCall {
                    tool: "custom_tool".into(),
                    args: serde_json::json!({"mode": "strict"}),
                },
            },
        ];
        let rendered = toml::to_string_pretty(&policy).unwrap();
        assert!(rendered.contains("[[validation.validators]]"));
        assert!(rendered.contains("kind = \"command\""));
        assert!(rendered.contains("kind = \"file_exists\""));
        assert!(rendered.contains("kind = \"tool_call\""));
        let parsed: WorkspacePolicy = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn validator_defaults_to_required_and_completion_phase() {
        let toml = r#"
            id = "x"
            kind = "file_exists"
            path = "output.txt"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        assert_eq!(parsed.id, "x");
        assert!(parsed.required, "required defaults to true");
        assert!(!parsed.soft_fail, "soft_fail defaults to false");
        assert_eq!(parsed.tier(), Required::Hard);
        assert_eq!(parsed.phase, ValidatorPhaseKind::Completion);
        assert!(parsed.timeout_ms.is_none());
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
    fn write_workspace_policy_if_absent_preserves_existing_file() {
        // This is the M11-C contract: under concurrent bootstrap or
        // operator edit, a pre-existing `.octos-workspace.toml` is
        // never clobbered. Equivalent to `OpenOptions::create_new`
        // failing closed on `AlreadyExists`.
        let temp = tempfile::tempdir().unwrap();
        let path = workspace_policy_path(temp.path());
        let sentinel = "# operator hand-edit do not overwrite\n";
        std::fs::write(&path, sentinel).unwrap();

        // Should succeed (idempotent) but NOT overwrite.
        write_workspace_policy_if_absent(temp.path(), &WorkspacePolicy::for_session()).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, sentinel);
    }

    #[test]
    fn magic_byte_kind_matches_recognized_prefixes() {
        assert!(MagicByteKind::Mp3.matches(b"ID3\0\0"));
        assert!(MagicByteKind::Mp3.matches(&[0xFF, 0xFB, 0x90, 0x00]));
        assert!(!MagicByteKind::Mp3.matches(b"GIF87a"));

        assert!(MagicByteKind::Wav.matches(b"RIFFxxxxWAVE"));
        assert!(!MagicByteKind::Wav.matches(b"ID3xxxx"));

        assert!(MagicByteKind::Pdf.matches(b"%PDF-1.4"));
        assert!(MagicByteKind::Png.matches(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]));
        assert!(MagicByteKind::Jpeg.matches(&[0xFF, 0xD8, 0xFF, 0xE0]));

        // MP4: 'ftyp' must appear at byte offset 4 (after size prefix).
        let mp4: [u8; 16] = [
            0, 0, 0, 0x20, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm', 0, 0, 0, 0,
        ];
        assert!(MagicByteKind::Mp4.matches(&mp4));
    }

    #[test]
    fn magic_byte_kind_pptx_matches_zip_signatures() {
        // PPTX is a ZIP archive — accept the OOXML local-file-header
        // signature (`PK\x03\x04`) and the central-directory variants so a
        // tool that emits a minimal/empty archive isn't spuriously rejected.
        assert!(MagicByteKind::Pptx.matches(b"PK\x03\x04rest of zip"));
        assert!(MagicByteKind::Pptx.matches(b"PK\x05\x06"));
        assert!(MagicByteKind::Pptx.matches(b"PK\x07\x08"));
        // An HTML error page surfaced in place of a PPTX must be rejected so
        // the silent-failure path is caught at the harness gate.
        assert!(!MagicByteKind::Pptx.matches(b"<!DOCTYPE html>"));
    }

    #[test]
    fn spawn_task_validator_spec_roundtrips_through_toml_bare_and_full_forms() {
        // Bare form: just the spec table. id, required, phase auto-filled.
        let bare_toml = r#"
            kind = "ominix_voice_exists"
            name_arg = "name"
        "#;
        let bare: SpawnTaskValidatorSpec = toml::from_str(bare_toml).unwrap();
        let validator = bare.into_validator("fm_voice_save", 0);
        assert_eq!(validator.id, "fm_voice_save.on_completion[0]");
        assert!(validator.required);
        assert!(!validator.soft_fail);
        assert_eq!(validator.tier(), Required::Hard);
        assert_eq!(validator.phase, ValidatorPhaseKind::Completion);
        match validator.spec {
            ValidatorSpec::OminixVoiceExists { ref name_arg } => {
                assert_eq!(name_arg, "name");
            }
            _ => panic!("expected OminixVoiceExists"),
        }

        // Full form: explicit id, required, phase.
        let full_toml = r#"
            id = "voice_optional"
            required = false
            phase = "completion"
            kind = "magic_bytes"
            glob = "*.mp3"
            format = "mp3"
        "#;
        let full: SpawnTaskValidatorSpec = toml::from_str(full_toml).unwrap();
        let validator = full.into_validator("ignored", 99);
        assert_eq!(validator.id, "voice_optional");
        assert!(!validator.required);
        assert_eq!(validator.tier(), Required::None);
    }

    // -----------------------------------------------------------------
    // Wave-3a: HttpProbeUntil + Sha256Match + soft_fail TOML roundtrips
    // -----------------------------------------------------------------

    #[test]
    fn http_probe_until_roundtrips_through_toml_with_default_intervals() {
        // Operators must be able to declare a polling probe in TOML with
        // only the URL set; the runtime fills the poll/deadline defaults.
        let toml = r#"
            id = "voice_train_done"
            kind = "http_probe_until"
            url_template = "http://x/v1/train/status?task_id=${args.task_id}"
            expected_contains = "complete"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        match parsed.spec {
            ValidatorSpec::HttpProbeUntil {
                ref url_template,
                expected_status,
                ref expected_contains,
                poll_interval_ms,
                deadline_ms,
            } => {
                assert_eq!(
                    url_template,
                    "http://x/v1/train/status?task_id=${args.task_id}"
                );
                assert_eq!(expected_status, 200);
                assert_eq!(expected_contains.as_deref(), Some("complete"));
                assert_eq!(poll_interval_ms, 2_000);
                assert_eq!(deadline_ms, 30_000);
            }
            ref other => panic!("expected HttpProbeUntil, got {other:?}"),
        }
        // Round-trip the validator through TOML to confirm fields survive.
        let rendered = toml::to_string_pretty(&parsed).unwrap();
        let reparsed: Validator = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn sha256_match_roundtrips_through_toml() {
        let toml = r#"
            id = "skill_main_hash"
            kind = "sha256_match"
            glob = "skill_main"
            sha256 = "${args.expected_sha256}"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        match parsed.spec {
            ValidatorSpec::Sha256Match {
                ref glob,
                ref sha256,
            } => {
                assert_eq!(glob, "skill_main");
                assert_eq!(sha256, "${args.expected_sha256}");
            }
            ref other => panic!("expected Sha256Match, got {other:?}"),
        }
    }

    #[test]
    fn soft_fail_validator_roundtrips_through_toml() {
        // The soft_fail companion field is the Wave-3a serde contract for
        // partial-artifact contracts: hard-required validators that should
        // surface as warnings rather than demote the spawn task.
        let toml = r#"
            id = "sub_artifact_warn"
            required = true
            soft_fail = true
            kind = "file_exists"
            path = "sub-artifact.md"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        assert!(parsed.required, "required field preserved verbatim");
        assert!(parsed.soft_fail, "soft_fail toggled on");
        assert_eq!(parsed.tier(), Required::Soft);
        // Round-trip: soft_fail must serialize and deserialize cleanly.
        let rendered = toml::to_string_pretty(&parsed).unwrap();
        assert!(
            rendered.contains("soft_fail = true"),
            "soft_fail = true should be emitted in TOML: {rendered}"
        );
        let reparsed: Validator = toml::from_str(&rendered).unwrap();
        assert_eq!(reparsed, parsed);
    }

    #[test]
    fn soft_fail_default_false_is_omitted_from_serialized_toml() {
        // Existing operator policies (no soft_fail field) must round-trip
        // byte-for-byte: soft_fail = false is the default and shouldn't
        // surface in the rendered TOML.
        let toml = r#"
            id = "primary_required"
            kind = "file_exists"
            path = "primary.md"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        assert!(!parsed.soft_fail);
        let rendered = toml::to_string_pretty(&parsed).unwrap();
        assert!(
            !rendered.contains("soft_fail"),
            "default soft_fail = false must not be emitted: {rendered}"
        );
    }

    #[test]
    fn validator_tier_collapses_required_and_soft_fail_correctly() {
        // The 4-case truth table from `Validator::tier`'s rustdoc.
        let make = |required: bool, soft_fail: bool| Validator {
            id: "x".into(),
            required,
            soft_fail,
            timeout_ms: None,
            phase: ValidatorPhaseKind::Completion,
            spec: ValidatorSpec::FileExists {
                path: "x".into(),
                min_bytes: None,
            },
        };
        assert_eq!(make(true, false).tier(), Required::Hard);
        assert_eq!(make(true, true).tier(), Required::Soft);
        assert_eq!(make(false, false).tier(), Required::None);
        assert_eq!(make(false, true).tier(), Required::Soft);
    }

    // ----- WorkspacePolicyKind::Coding (Audit Gap-1 + section 7 Q3) -----

    #[test]
    fn should_detect_coding_kind_when_cargo_toml_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert_eq!(
            detect_workspace_policy_kind(tmp.path()),
            WorkspacePolicyKind::Coding
        );
    }

    #[test]
    fn should_detect_coding_kind_when_package_json_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        assert_eq!(
            detect_workspace_policy_kind(tmp.path()),
            WorkspacePolicyKind::Coding
        );
    }

    #[test]
    fn should_detect_coding_kind_when_pyproject_toml_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[project]\n").unwrap();
        assert_eq!(
            detect_workspace_policy_kind(tmp.path()),
            WorkspacePolicyKind::Coding
        );
    }

    #[test]
    fn should_fall_back_to_session_kind_when_no_language_signal() {
        let tmp = tempfile::tempdir().unwrap();
        // Just an unrelated file — no manifest probes match.
        std::fs::write(tmp.path().join("README.md"), "# hi").unwrap();
        assert_eq!(
            detect_workspace_policy_kind(tmp.path()),
            WorkspacePolicyKind::Session
        );
    }

    #[test]
    fn should_return_coding_policy_marker_for_coding_kind() {
        let policy = WorkspacePolicy::for_coding();
        assert_eq!(policy.workspace.kind, WorkspacePolicyKind::Coding);
        // The coding policy shares the session baseline (no bundled
        // spawn-task contracts in the slimmed coding-agent build).
        assert!(policy.spawn_tasks.is_empty());
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

    #[test]
    fn should_return_eslint_hook_gated_on_bin_in_coding_defaults() {
        let hooks = coding_default_hooks();
        let eslint = hooks
            .iter()
            .find(|h| h.command.first().map(String::as_str) == Some("eslint"))
            .expect("eslint hook present");
        // ESLint hook must be opt-out friendly via requires_bin — operators
        // without eslint on PATH must NOT see hook failures every edit.
        assert_eq!(eslint.requires_bin.as_deref(), Some("eslint"));
        // Four explicit patterns — the glob crate treats braces as literals
        // (#2129 review, finding 10).
        for ext in ["js", "ts", "tsx", "jsx"] {
            let pattern = format!("**/*.{ext}");
            assert!(
                eslint.path_filter.iter().any(|p| p == &pattern),
                "missing {pattern}"
            );
        }
    }

    #[test]
    fn should_return_ruff_hook_gated_on_bin_in_coding_defaults() {
        let hooks = coding_default_hooks();
        let ruff = hooks
            .iter()
            .find(|h| h.command.first().map(String::as_str) == Some("ruff"))
            .expect("ruff hook present");
        assert_eq!(ruff.requires_bin.as_deref(), Some("ruff"));
        assert!(ruff.path_filter.iter().any(|p| p == "**/*.py"));
    }

    #[test]
    fn should_not_emit_coding_hooks_for_session_kind() {
        // Session policies retain the legacy no-default-hooks behaviour so
        // existing operators don't see a sudden new wave of cargo checks.
        let session = WorkspacePolicy::for_session();
        assert_eq!(session.workspace.kind, WorkspacePolicyKind::Session);
        // The hooks helper is global (not method-on-policy); we assert that
        // callers must opt in by inspecting the kind themselves.
        assert_ne!(session.workspace.kind, WorkspacePolicyKind::Coding);
    }

    #[test]
    fn should_serialize_coding_kind_as_kebab_case() {
        let policy = WorkspacePolicy::for_coding();
        let rendered = toml::to_string(&policy).unwrap();
        assert!(
            rendered.contains("kind = \"coding\""),
            "expected kebab-case 'coding' in serialized policy:\n{rendered}"
        );
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
