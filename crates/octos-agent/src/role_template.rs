//! Role templates for subagent / spawn-only child tasks.
//!
//! Issue #971 (M14-C). The wire `TaskListEntry.role` field (PR #1103) and
//! `BackgroundTask.role` projection (PR #1109/#1113) already carry a role
//! string forward from spawn-time to UX hydration, but the set of legal
//! values has lived as a free-form `Option<String>` agreed in comments
//! across `octos-core::ui_protocol`, `octos-agent::task_supervisor`, and
//! the orchestrator paths in `octos-cli::api`. Without a typed registry
//! every new caller risks coining a slightly different spelling
//! (`"review"` vs `"reviewer"`, `"test"` vs `"test_worker"`) and the UX
//! has no place to look up role metadata (allowed tool budget, default
//! sandbox + approval policy, model preference, prompt prefix).
//!
//! This module is the single source of truth for the four canonical
//! roles M14-C targets:
//!
//! `reviewer` — repository/code reviewer that walks a diff and emits
//! findings. `implementer` — implementation worker that edits files to
//! satisfy a task. `test_worker` — verification worker that runs the
//! test/lint/build suite and reports failures. `explorer` — read-only
//! codebase analyst that gathers context for an upstream planner.
//!
//! M14-C runtime wiring: `spawn` / `spawn_agent` consult this registry
//! when a child declares `role`, native review specialists stamp the
//! resolved runtime template onto their `TaskSupervisor` task records,
//! and AppUI receives the resulting role/source/artifact/runtime-policy
//! projection through `task/list` and `task/updated`.

use std::fmt;
use std::str::FromStr;

use serde_json::{Value, json};

/// Canonical name for the repository / code reviewer role.
pub const ROLE_REVIEWER: &str = "reviewer";
/// Canonical name for the implementation worker role.
pub const ROLE_IMPLEMENTER: &str = "implementer";
/// Canonical name for the test / verification worker role.
pub const ROLE_TEST_WORKER: &str = "test_worker";
/// Canonical name for the read-only codebase analyst role.
pub const ROLE_EXPLORER: &str = "explorer";

/// Sentinel for `RoleTemplate::default_sandbox_mode` meaning "use the
/// session's auto-detected sandbox" (matches `SandboxMode::Auto`).
pub const SANDBOX_AUTO: &str = "auto";
/// Sentinel for `RoleTemplate::default_sandbox_mode` meaning "the role
/// is read-only and does not need an exec sandbox" (matches
/// `SandboxMode::None`).
pub const SANDBOX_NONE: &str = "none";

/// Sentinel for `RoleTemplate::default_approval_policy` meaning "ask
/// the upstream client before exec" (matches `ApprovalPolicy::Ask`).
pub const APPROVAL_ASK: &str = "ask";
/// Sentinel for `RoleTemplate::default_approval_policy` meaning "never
/// prompt; reject ask-required commands at the tool boundary" (matches
/// `ApprovalPolicy::Never`).
pub const APPROVAL_NEVER: &str = "never";

/// Tools the default `ToolRegistry::with_builtins`-constructed child
/// registry reliably registers AND that actually function inside a
/// detached subagent context (independent of memory store / research
/// index / session actor / native-spawn delegate wiring). Issue #971
/// (M14-C) codex P1 fix: `RoleTemplate::to_spawn_compatible_allow`
/// intersects the role's expanded budget with this list so the spawn
/// tool's availability check does not reject tools that aren't
/// registered in the stand-alone child runtime.
///
/// Notably absent (issue #971 codex iter-3 P2.1): `spawn_agent`.
/// `with_builtins` registers `SpawnAgentTool::new()` WITHOUT a native
/// spawn delegate; the alias in a child therefore always returns the
/// "spawn_agent requires the session runtime to register a native
/// spawn tool delegate" error. Advertising it via the implementer
/// template would offer a tool that fails with a no-delegate error
/// rather than the session capability the template promises — so the
/// alias is filtered here. When the session actor wires a real spawn
/// delegate into a per-session registry, that path stays separate
/// from the static M14-C role budget and the parent's `ToolPolicy`
/// can re-grant the alias explicitly.
///
/// Kept in stable lexicographic order so the const slice can be
/// searched via `.contains()` without allocating a HashSet at the
/// caller. Update whenever
/// `ToolRegistry::with_builtins_and_permissions` adds or removes a
/// non-feature-gated builtin AND that builtin is actually usable
/// from inside a detached subagent.
pub(crate) const SPAWN_BUILTIN_TOOLS: &[&str] = &[
    "apply_patch",
    "ask_user_question",
    "bash",
    "close_agent",
    "diff_edit",
    "edit_file",
    "exec_command",
    "glob",
    "grep",
    "list_dir",
    "read_file",
    "resume_agent",
    "send_input",
    "shell",
    "tool_search",
    "tool_suggest",
    "update_plan",
    "view_image",
    "wait_agent",
    "write_file",
    "write_stdin",
];

/// Soft model preference hint. Templates set this so the orchestrator
/// can route review / implementation children to a coding-grade lane
/// while letting explorers fall onto the cheap analyst lane. Treated as
/// advisory — concrete model resolution still flows through
/// `ModelStylesheet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPreference {
    /// Coding / reasoning grade model (e.g. `claude-opus-4-7`).
    Coding,
    /// Lighter analyst-grade model that prioritises throughput / cost.
    Analyst,
    /// Cheap / fast model suitable for read-only fanout.
    Cheap,
}

impl ModelPreference {
    /// Stable string representation used in metadata payloads and
    /// `runtime_policy_stamp.model_preference`. Round-trips via
    /// `ModelPreference::from_str`.
    pub const fn as_str(self) -> &'static str {
        match self {
            ModelPreference::Coding => "coding",
            ModelPreference::Analyst => "analyst",
            ModelPreference::Cheap => "cheap",
        }
    }

    /// Parse the stable string representation. Unknown values map to
    /// `None` — callers should treat that as "no preference". Mirrors
    /// the `FromStr` impl but returns `Option` so callers can keep a
    /// soft-fallback "no preference" path without converting an error.
    pub fn parse(value: &str) -> Option<Self> {
        Self::from_str(value).ok()
    }
}

impl FromStr for ModelPreference {
    type Err = UnknownModelPreference;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "coding" => Ok(ModelPreference::Coding),
            "analyst" => Ok(ModelPreference::Analyst),
            "cheap" => Ok(ModelPreference::Cheap),
            other => Err(UnknownModelPreference(other.to_owned())),
        }
    }
}

/// Error returned by `<ModelPreference as FromStr>::from_str` when the
/// input is not one of the registered preference names. Carries the
/// offending input so callers can surface diagnostic context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownModelPreference(pub String);

impl fmt::Display for UnknownModelPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown model preference: {:?}", self.0)
    }
}

impl std::error::Error for UnknownModelPreference {}

impl fmt::Display for ModelPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A registered subagent role: typed metadata clients consult to render
/// UX and the orchestrator consults to gate tool budget + sandbox.
///
/// All fields are `'static` so the registry is a `const` table. To
/// extend the registry, add a `RoleTemplate` to `ROLE_TEMPLATES` below
/// and update the guard tests so the canonical name + tool group
/// membership are pinned.
#[derive(Debug, Clone, Copy)]
pub struct RoleTemplate {
    /// Canonical, machine-readable role identifier. Must match one of
    /// the `ROLE_*` constants in this module. Pinned by guard tests so
    /// downstream code can rely on the spelling.
    pub name: &'static str,
    /// Human-readable role label suitable for inline UX badges
    /// (e.g. "Reviewer", "Test Worker").
    pub display_name: &'static str,
    /// One-line description of what the role does. Bounded so it can
    /// be surfaced in tooltips without truncation.
    pub description: &'static str,
    /// The tools the role advertises as in-budget. Each entry is
    /// either a group identifier (e.g. `"group:search"`, matching
    /// `tools::policy::TOOL_GROUPS`) OR a single exact tool name
    /// (e.g. `"read_file"`). This matches the shape `ToolPolicy.allow`
    /// already accepts, so a downstream caller can feed this slice
    /// straight into the policy. Mixing the two is intentional:
    /// read-only roles need to grant `read_file` WITHOUT pulling in
    /// the mutating tools that `group:fs` expands to (`write_file`,
    /// `apply_patch`, `edit_file`, `diff_edit`).
    pub allowed_tools: &'static [&'static str],
    /// Default sandbox mode the role suggests. One of `SANDBOX_AUTO`
    /// or `SANDBOX_NONE`. Templates intentionally do not advertise
    /// specific backends — backend selection is environment-driven.
    pub default_sandbox_mode: &'static str,
    /// Default approval policy the role suggests. One of
    /// `APPROVAL_ASK` or `APPROVAL_NEVER`.
    pub default_approval_policy: &'static str,
    /// Soft model preference. Advisory only — the orchestrator still
    /// resolves the concrete provider via the stylesheet.
    pub model_preference: ModelPreference,
    /// Bounded prompt prefix the orchestrator prepends to the system
    /// prompt for this role. Kept short (< ~600 chars) so it does not
    /// crowd the user-supplied system prompt or the per-task brief.
    pub prompt_prefix: &'static str,
}

impl RoleTemplate {
    /// Look up a role template by its canonical name. Returns `None`
    /// for unknown values so callers can defensively reject drift
    /// instead of silently defaulting.
    pub fn for_name(name: &str) -> Option<&'static RoleTemplate> {
        ROLE_TEMPLATES.iter().find(|tpl| tpl.name == name)
    }

    /// Slice of every registered role template, in stable declaration
    /// order. UX surfaces (e.g. the spawn-role dropdown in the admin
    /// dashboard) can iterate this for free.
    pub fn all() -> &'static [RoleTemplate] {
        ROLE_TEMPLATES
    }

    /// True if `entry` is advertised as in-budget for this role.
    /// Pure-string equality — the registry stores either group
    /// identifiers (`"group:search"`) or exact tool names
    /// (`"read_file"`). Callers asking "is `group:fs` in the budget?"
    /// should pass `"group:fs"` literally; callers asking "is
    /// `read_file` in the budget?" should pass `"read_file"`. To
    /// expand groups to their tool members use
    /// `tools::policy::tool_group_info`.
    pub fn permits(&self, entry: &str) -> bool {
        self.allowed_tools.contains(&entry)
    }

    /// Return the template tool budget in the owned shape expected by
    /// `SpawnTool::Input.allowed_tools` and `ToolPolicy.allow`.
    /// Issue #971 (M14-C) wires this into the spawn aliases so a role
    /// name on the wire resolves to the same allow list the runtime
    /// gates the child agent on.
    ///
    /// The shape is identical to the static `allowed_tools` slice: each
    /// entry is either a `group:*` identifier or a bare exact tool name.
    /// Group expansion happens at `ToolPolicy::evaluate` time, so the
    /// safety property guarded by the unit tests in this module
    /// (read-only roles never advertise a mutating group) carries
    /// through into the runtime allow list unchanged.
    ///
    /// Use [`Self::to_expanded_tool_names`] instead when feeding the
    /// spawn tool's `allowed_tools` field — that consumer does exact-
    /// name lookup against the tool registry and does NOT understand
    /// `group:*` entries, so groups have to be expanded first.
    pub fn allowed_tools_vec(&self) -> Vec<String> {
        self.allowed_tools
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect()
    }

    /// Expand the template's tool budget into the concrete exact tool
    /// names the spawn tool's availability check expects. Each
    /// `group:*` entry is resolved through
    /// [`crate::tools::policy::tool_group_info`]; bare tool names pass
    /// through unchanged. Unknown group names fall through as opaque
    /// entries so the caller's downstream availability check surfaces
    /// the same "required tool not available" error a typo on a bare
    /// tool name would.
    ///
    /// Issue #971 (M14-C) codex P1: `SpawnTool::ensure_subagent_tools_available`
    /// fails closed on every `group:*` entry it sees because it does
    /// `tools.get(tool_name).is_none()`. Without this expansion the
    /// real `spawn_agent({"role": "reviewer"})` path would error with
    /// "required tool(s) not available: group:search" the moment the
    /// role wiring fired.
    ///
    /// **Note**: this can return tool names not present in every
    /// runtime (e.g. `recall_memory` requires a memory store provider;
    /// `spawn` is only registered by the session actor, not by
    /// `ToolRegistry::with_builtins`). Callers feeding the spawn
    /// tool's child registry should use [`Self::to_spawn_compatible_allow`]
    /// instead, which intersects this expansion with the set of tools
    /// the default `ToolRegistry::with_builtins` reliably registers.
    pub fn to_expanded_tool_names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(self.allowed_tools.len());
        for entry in self.allowed_tools {
            if entry.starts_with("group:") {
                if let Some(group) = crate::tools::policy::tool_group_info(entry) {
                    for tool in group.tools {
                        let owned = (*tool).to_string();
                        if !out.contains(&owned) {
                            out.push(owned);
                        }
                    }
                    continue;
                }
                // Unknown group — fall through as-is so the caller's
                // downstream availability check reports a clear error.
            }
            let owned = (*entry).to_string();
            if !out.contains(&owned) {
                out.push(owned);
            }
        }
        out
    }

    /// Filtered variant of [`Self::to_expanded_tool_names`] that emits
    /// only tools the default `ToolRegistry::with_builtins`-built child
    /// registry actually registers. Issue #971 (M14-C) codex P1 fix
    /// (iteration 2): the prior wiring forwarded `recall_memory` /
    /// `synthesize_research` / `save_memory` / `spawn` from role
    /// templates into the child's `Input.allowed_tools`, which
    /// `ensure_subagent_tools_available` then rejected — every default
    /// role-based spawn failed before the child could run.
    ///
    /// The kept set is the intersection of the role's expanded budget
    /// with [`SPAWN_BUILTIN_TOOLS`] (the tools `with_builtins`
    /// guarantees). Tools requiring extra runtime wiring (memory
    /// store, research index, session-actor-bound spawn, ...) are
    /// filtered out at the spawn boundary; if a session actor extends
    /// the child registry with those, the parent's `ToolPolicy` still
    /// gates them through the standard policy plumbing.
    pub fn to_spawn_compatible_allow(&self) -> Vec<String> {
        self.to_expanded_tool_names()
            .into_iter()
            .filter(|tool| SPAWN_BUILTIN_TOOLS.contains(&tool.as_str()))
            .collect()
    }

    /// Bounded UX summary the orchestrator surfaces in `tool/status/list`
    /// alongside the coding tool contract (issue #971 / M14-C deliverable
    /// "Test: role/tool/sandbox/model policy is resolved by the server
    /// runtime"). Mirrors `RoleTemplate` fields the UX cares about
    /// without leaking the prompt prefix (which is server-owned).
    pub fn summary(&self) -> RoleTemplateSummary<'static> {
        RoleTemplateSummary {
            name: self.name,
            display_name: self.display_name,
            description: self.description,
            allowed_tools: self.allowed_tools,
            default_sandbox_mode: self.default_sandbox_mode,
            default_approval_policy: self.default_approval_policy,
            model_preference: self.model_preference.as_str(),
        }
    }

    /// Infer a canonical role from Codex-style `agent_type` values.
    ///
    /// Codex prompts commonly ask for generic `worker`, `reviewer`,
    /// `tester`, or `explorer` agents. This helper keeps that vocabulary
    /// model-visible while resolving the actual role through the backend
    /// registry.
    pub fn for_codex_agent_type(agent_type: &str) -> Option<&'static RoleTemplate> {
        let normalized = agent_type.trim().to_ascii_lowercase();
        let role = match normalized.as_str() {
            "review"
            | "reviewer"
            | "code_reviewer"
            | "code-reviewer"
            | "repo_reviewer"
            | "repository_reviewer" => ROLE_REVIEWER,
            "worker"
            | "implementer"
            | "implementation_worker"
            | "implementation-worker"
            | "coder"
            | "developer" => ROLE_IMPLEMENTER,
            "test"
            | "tester"
            | "test_worker"
            | "test-worker"
            | "verification_worker"
            | "verifier" => ROLE_TEST_WORKER,
            "explore" | "explorer" | "analyst" | "codebase_analyst" | "read_only" | "read-only" => {
                ROLE_EXPLORER
            }
            other => other,
        };
        Self::for_name(role)
    }

    /// Backend-owned runtime stamp for task/list and task/updated
    /// projection. The stamp is intentionally descriptive rather than a
    /// command surface: it records which server template resolved the
    /// child role, tool budget, sandbox, approval, and model preference.
    pub fn runtime_policy_stamp(
        &self,
        source: &str,
        backend: &str,
        requested_model: Option<&str>,
    ) -> Value {
        json!({
            "template_id": "m14-c.subagent_runtime.v1",
            "role": self.name,
            "role_name": self.display_name,
            "source": source,
            "backend": backend,
            "sandbox": self.default_sandbox_mode,
            "approval_policy": self.default_approval_policy,
            "tool_policy_id": format!("role:{}", self.name),
            "allowed_tools": self.allowed_tools,
            "model_preference": self.model_preference.as_str(),
            "requested_model": requested_model,
        })
    }
}

/// Bounded UX summary of a [`RoleTemplate`], emitted as part of the
/// `tool/status/list` payload so AppUI / TUI can render a spawn-role
/// picker without round-tripping to a dedicated endpoint. Mirrors every
/// `RoleTemplate` field clients legitimately need; the `prompt_prefix`
/// is intentionally excluded because it is a server-owned secret that
/// shouldn't ride on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RoleTemplateSummary<'a> {
    pub name: &'a str,
    pub display_name: &'a str,
    pub description: &'a str,
    pub allowed_tools: &'a [&'a str],
    pub default_sandbox_mode: &'a str,
    pub default_approval_policy: &'a str,
    pub model_preference: &'a str,
}

/// All registered role templates. Keep this list aligned with the
/// guard tests at the bottom of this module — any drift in the name
/// set or tool-group budget is a load-bearing change that downstream
/// `TaskListEntry.role` consumers care about.
const ROLE_TEMPLATES: &[RoleTemplate] = &[
    RoleTemplate {
        name: ROLE_REVIEWER,
        display_name: "Reviewer",
        description: "Repository / code reviewer. Walks the diff and emits structured findings, \
                      and may WRITE its review report to a file; does not modify the code under \
                      review.",
        // Reviewers READ files, search, and WRITE their own review
        // report. `write_file` is listed as an EXACT tool name (never
        // `group:fs`): a reviewer needs to produce a written deliverable
        // (e.g. `security-review.md`), but must not gain the
        // code-mutating tools that `group:fs` expands to
        // (`edit_file`/`apply_patch`/`diff_edit`). Every OTHER entry is
        // an exact read-only tool name or a group that contains ONLY
        // read-only tools — because if a future caller pipes
        // `allowed_tools` straight into `ToolPolicy.allow`, group
        // expansion would silently grant every member tool. The set
        // deliberately excludes: `group:fs` (the code-patching tools)
        // and `group:memory` (`save_memory`). A reviewer writes its
        // REPORT, not the reviewed code.
        allowed_tools: &["read_file", "write_file", "group:search", "recall_memory"],
        default_sandbox_mode: SANDBOX_NONE,
        default_approval_policy: APPROVAL_NEVER,
        model_preference: ModelPreference::Coding,
        prompt_prefix: "You are a code reviewer. Read the diff and the surrounding context, \
                        then emit findings as a bounded list. WRITE your review to the requested \
                        report file (use write_file), but do NOT edit the code under review, run \
                        the test suite, or spawn further agents. Prefer concrete file:line \
                        references and explain the WHY of each finding.",
    },
    RoleTemplate {
        name: ROLE_IMPLEMENTER,
        display_name: "Implementer",
        description: "Implementation worker. Edits workspace files to satisfy a bounded task; \
                      may run shell commands inside the session sandbox.",
        // Implementers need fs read/write, search, shell, and the
        // delegated-child sessions group so they can fan out to
        // test_worker for verification. `group:fs` is appropriate
        // here because the role IS supposed to mutate files.
        allowed_tools: &[
            "group:fs",
            "group:search",
            "group:runtime",
            "group:sessions",
            "group:memory",
        ],
        default_sandbox_mode: SANDBOX_AUTO,
        default_approval_policy: APPROVAL_ASK,
        model_preference: ModelPreference::Coding,
        prompt_prefix: "You are an implementation worker. Make the smallest change that \
                        satisfies the brief. Read before writing, prefer Edit over Write, \
                        and stop once the change compiles and the relevant tests pass. \
                        Surface any out-of-scope drift in the final summary instead of \
                        silently expanding the patch.",
    },
    RoleTemplate {
        name: ROLE_TEST_WORKER,
        display_name: "Test Worker",
        description: "Verification worker. Runs the test / lint / build suite the upstream \
                      task implies and reports concrete failures.",
        // Test workers run commands, read files, and WRITE their
        // verification report. `write_file` is listed as an EXACT tool
        // name (never `group:fs`): the worker persists its report but
        // must not gain the code-patching tools `group:fs` expands to
        // (`edit_file`/`apply_patch`) — fixing is the implementer's job.
        // Explicit `read_file` + `recall_memory` so `group:memory`
        // mutating tools do NOT leak in.
        allowed_tools: &[
            "read_file",
            "write_file",
            "group:search",
            "group:runtime",
            "recall_memory",
        ],
        default_sandbox_mode: SANDBOX_AUTO,
        default_approval_policy: APPROVAL_ASK,
        model_preference: ModelPreference::Analyst,
        prompt_prefix: "You are a verification worker. Run the test, lint, and build commands \
                        implied by the brief. Do NOT edit source files. WRITE your verification \
                        report to the requested file (use write_file). Report concrete failures \
                        with the offending command, exit code, and the most diagnostic 20-40 \
                        lines of output.",
    },
    RoleTemplate {
        name: ROLE_EXPLORER,
        display_name: "Explorer",
        description: "Read-only codebase analyst. Gathers context (files, call sites, prior \
                      art) for an upstream planner and may WRITE its findings to a report file; \
                      does not mutate the code it analyses.",
        // Explorers analyse code read-only and WRITE their context
        // report. `write_file` is listed as an EXACT tool name (never
        // `group:fs`): the explorer persists its summary but must not
        // gain the code-mutating tools `group:fs` expands to. Every
        // OTHER entry is an exact read-only tool name or a group of
        // read-only tools. The explorer writes its REPORT, not the
        // analysed code.
        allowed_tools: &["read_file", "write_file", "group:search", "recall_memory"],
        default_sandbox_mode: SANDBOX_NONE,
        default_approval_policy: APPROVAL_NEVER,
        model_preference: ModelPreference::Cheap,
        prompt_prefix: "You are a codebase explorer. Read files, search, and summarise. Do \
                        not edit the code, do not run commands, do not spawn further agents. \
                        WRITE your findings to the requested report file (use write_file), and \
                        return a bounded summary plus absolute file paths the upstream planner \
                        should consult next.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard: the four canonical role names M14-C promises must remain
    /// the EXACT spelling the wire schema, `TaskListEntry.role` doc
    /// comment, and `BackgroundTask.role` projection comment agreed
    /// on. Drift here breaks every downstream consumer.
    #[test]
    fn registry_exposes_the_four_canonical_role_names() {
        let names: Vec<&'static str> = RoleTemplate::all().iter().map(|tpl| tpl.name).collect();
        assert_eq!(
            names,
            vec!["reviewer", "implementer", "test_worker", "explorer"],
            "M14-C canonical role names drifted; update guard + wire docs together"
        );
    }

    /// Guard: unknown role names return `None` instead of falling back
    /// to a default template. The TaskListEntry.role field is
    /// `Option<String>`; the caller is expected to handle the unknown
    /// case explicitly rather than receive a spoofed reviewer.
    #[test]
    fn for_name_returns_none_for_unknown_role() {
        assert!(RoleTemplate::for_name("review").is_none());
        assert!(RoleTemplate::for_name("Reviewer").is_none());
        assert!(RoleTemplate::for_name("").is_none());
        assert!(RoleTemplate::for_name("planner").is_none());
    }

    /// Guard: a reviewer can WRITE its review report (`write_file`, an
    /// exact tool name) but MUST NOT gain the code-patching tools —
    /// `edit_file` and the `group:fs` expansion (`apply_patch` /
    /// `diff_edit` / `edit_file`) — so it produces a report without
    /// mutating the code under review. This is the deliberate
    /// relaxation of the old strictly-read-only budget: reviewers kept
    /// returning findings as text because they had no way to persist a
    /// deliverable. `group:fs` stays out (its expansion would smuggle
    /// the patching tools back in — the codex P1).
    #[test]
    fn reviewer_writes_report_but_not_code() {
        let tpl = RoleTemplate::for_name(ROLE_REVIEWER).expect("reviewer must be registered");
        // Permitted: explicit read tools, the report writer, and
        // non-mutating groups.
        assert!(tpl.permits("read_file"));
        assert!(tpl.permits("write_file")); // writes its REVIEW REPORT
        assert!(tpl.permits("group:search"));
        assert!(tpl.permits("recall_memory"));
        // Denied: the code-patching tool + every mutating group. A
        // reviewer writes its report, never edits the reviewed code.
        assert!(!tpl.permits("edit_file"));
        assert!(!tpl.permits("group:fs"));
        assert!(!tpl.permits("group:memory"));
        assert!(!tpl.permits("group:runtime"));
        assert!(!tpl.permits("group:sessions"));
        assert!(!tpl.permits("save_memory"));
        assert_eq!(tpl.default_sandbox_mode, SANDBOX_NONE);
        assert_eq!(tpl.default_approval_policy, APPROVAL_NEVER);
        assert_eq!(tpl.model_preference, ModelPreference::Coding);
    }

    /// Safety guard (post codex review): roles whose `prompt_prefix`
    /// promises "do not edit" / "never mutates state" must not
    /// transitively grant a mutating tool through a coarse group.
    /// The set below covers every `tools::policy::TOOL_GROUPS` entry
    /// whose expansion contains at least one tool that writes to
    /// disk, mutates session/profile state, or spawns further work:
    ///
    /// - `group:fs` -> write_file / apply_patch / edit_file / diff_edit
    /// - `group:memory` -> save_memory
    /// - `group:runtime` -> shell / exec_command / write_stdin
    /// - `group:sessions` -> spawn / spawn_agent / send_input / ...
    ///
    /// Read-only roles (`reviewer`, `explorer`) MUST advertise none
    /// of these; `test_worker` is allowed `group:runtime` because
    /// running commands IS the role's job. This catches the codex P1
    /// (smuggling `group:fs` into a non-mutating role).
    #[test]
    fn read_only_roles_do_not_advertise_mutating_groups() {
        const ALL_MUTATING_GROUPS: &[&str] = &[
            "group:fs",
            "group:memory",
            "group:runtime",
            "group:sessions",
            "group:admin",
            "group:media",
        ];
        // Reviewer + explorer are documented as fully read-only.
        for name in [ROLE_REVIEWER, ROLE_EXPLORER] {
            let tpl = RoleTemplate::for_name(name).unwrap();
            for group in ALL_MUTATING_GROUPS {
                assert!(
                    !tpl.permits(group),
                    "{name} advertises mutating group {group:?} but its prompt_prefix \
                     promises not to mutate state",
                );
            }
        }
        // test_worker is allowed `group:runtime` (running commands
        // IS the role's job) but MUST NOT advertise any other
        // mutating group.
        let test_worker = RoleTemplate::for_name(ROLE_TEST_WORKER).unwrap();
        for group in ALL_MUTATING_GROUPS {
            if *group == "group:runtime" {
                continue;
            }
            assert!(
                !test_worker.permits(group),
                "test_worker advertises mutating group {group:?} but the role docs forbid \
                 anything beyond running test/lint/build commands",
            );
        }
    }

    /// Guard: every template advertises a non-empty prompt prefix and
    /// a non-empty tool budget. A template with an empty budget is a
    /// misconfiguration — the role would be unable to do anything.
    #[test]
    fn every_template_has_prefix_and_budget() {
        for tpl in RoleTemplate::all() {
            assert!(
                !tpl.prompt_prefix.is_empty(),
                "{} prompt_prefix must be non-empty",
                tpl.name
            );
            assert!(
                !tpl.display_name.is_empty(),
                "{} display_name must be non-empty",
                tpl.name
            );
            assert!(
                !tpl.description.is_empty(),
                "{} description must be non-empty",
                tpl.name
            );
            assert!(
                !tpl.allowed_tools.is_empty(),
                "{} allowed_tools must be non-empty",
                tpl.name
            );
        }
    }

    #[test]
    fn codex_agent_type_aliases_resolve_to_backend_roles() {
        assert_eq!(
            RoleTemplate::for_codex_agent_type("review").map(|template| template.name),
            Some(ROLE_REVIEWER)
        );
        assert_eq!(
            RoleTemplate::for_codex_agent_type("worker").map(|template| template.name),
            Some(ROLE_IMPLEMENTER)
        );
        assert_eq!(
            RoleTemplate::for_codex_agent_type("tester").map(|template| template.name),
            Some(ROLE_TEST_WORKER)
        );
        assert_eq!(
            RoleTemplate::for_codex_agent_type("read-only").map(|template| template.name),
            Some(ROLE_EXPLORER)
        );
        assert!(RoleTemplate::for_codex_agent_type("planner").is_none());
    }

    /// Issue #971 (M14-C) codex P1 regression: `to_expanded_tool_names`
    /// MUST produce zero `group:*` entries — only concrete tool names.
    /// `SpawnTool::ensure_subagent_tools_available` does exact-name
    /// `tools.get(name).is_none()` lookup, so any `group:*` slipping
    /// through would make every role-based `spawn_agent` call fail
    /// availability with "required tool not available: group:search".
    #[test]
    fn m14_c_wiring_to_expanded_tool_names_emits_no_group_entries_per_971() {
        for tpl in RoleTemplate::all() {
            let expanded = tpl.to_expanded_tool_names();
            for entry in &expanded {
                assert!(
                    !entry.starts_with("group:"),
                    "{} emitted raw group identifier {:?} from to_expanded_tool_names; \
                     the spawn tool's availability check does exact-name lookup and \
                     would treat this as a missing tool",
                    tpl.name,
                    entry
                );
                assert!(!entry.is_empty(), "{} emitted empty tool name", tpl.name);
            }
            // Sanity: expansion preserves at least one of each group's
            // member tools. We pick the static `group:search` =>
            // {glob, grep, list_dir} mapping since every M14-C
            // template advertises group:search.
            if tpl.permits("group:search") {
                for member in ["glob", "grep", "list_dir"] {
                    assert!(
                        expanded.contains(&member.to_string()),
                        "{} permits group:search but expanded set missing {member:?}; \
                         got {expanded:?}",
                        tpl.name
                    );
                }
            }
        }
    }

    /// Issue #971 (M14-C) safety property: when a read-only role's
    /// allow list is interpreted by `tools::policy::ToolPolicy`, the
    /// expanded set must NOT contain any disk-writing or session-
    /// mutating tool. This is the runtime-side complement of the
    /// `read_only_roles_do_not_advertise_mutating_groups` guard above:
    /// even if a future template adds a new `group:` that happens to
    /// expand to a mutating tool, this test fires on the actual
    /// `ToolPolicy::is_allowed` decision.
    #[test]
    fn m14_c_wiring_reviewer_policy_denies_mutating_tools_per_971() {
        use crate::tools::policy::ToolPolicy;
        let tpl = RoleTemplate::for_name(ROLE_REVIEWER).unwrap();
        let policy = ToolPolicy {
            allow: tpl.allowed_tools_vec(),
            ..Default::default()
        };
        // `write_file` is deliberately NOT here — a reviewer writes its
        // report. The CODE-PATCHING tools stay denied so it can't mutate
        // the reviewed code.
        for mutator in [
            "edit_file",
            "diff_edit",
            "apply_patch",
            "save_memory",
            "shell",
            "exec_command",
            "write_stdin",
            "spawn",
            "spawn_agent",
        ] {
            assert!(
                !policy.is_allowed(mutator),
                "reviewer policy must deny mutating tool {mutator:?}; \
                 allow list = {:?}",
                policy.allow
            );
        }
        // Sanity: the read tools the template advertises PLUS its report
        // writer stay allowed once the allow list is piped through ToolPolicy.
        for permitted in [
            "read_file",
            "write_file",
            "glob",
            "grep",
            "list_dir",
            "recall_memory",
        ] {
            assert!(
                policy.is_allowed(permitted),
                "reviewer policy must allow {permitted:?}"
            );
        }
    }
}
