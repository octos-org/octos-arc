//! Tool policy system with allow/deny lists, groups, and wildcards.

use metrics::counter;
use serde::{Deserialize, Serialize};

/// Outcome of `ToolPolicy::evaluate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Tool is permitted.
    Allow,
    /// Tool is denied. `reason` is the metric label emitted for observability.
    Deny { reason: &'static str },
}

/// Metric counter name for policy denials.
pub const POLICY_DENIAL_COUNTER: &str = "octos_tool_policy_denial_total";

/// Deny reason label used when a robot-tier group gates a tool.
pub const ROBOT_TIER_GATE_REASON: &str = "robot_tier_gate";

/// Deny reason label used for a non-robot policy deny.
pub const GENERIC_DENY_REASON: &str = "policy_deny";

/// Tool policy with allow/deny lists and tag-based filtering. Deny always wins over allow.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolPolicy {
    /// Tools, groups, or wildcards to allow. Empty = allow all.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tools, groups, or wildcards to deny. Always wins over allow.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Required tags: only tools declaring at least one matching tag are
    /// visible. Empty = no tag filtering. Composable with allow/deny (deny
    /// still wins). Untagged tools FAIL a non-empty filter (fail closed) —
    /// see [`ToolPolicy::is_allowed_with_tags`].
    #[serde(default)]
    pub require_tags: Vec<String>,
    /// #28b — policy for FILE-WRITING via the bash/shell tool. `allow`
    /// (default) = zero behavior change; `warn` = the change receipt's tail
    /// appends a nudge to prefer `edit_file`/`diff_edit` when the command
    /// actually changed files; `deny` = a heuristic pre-screen refuses
    /// write-shaped commands and points at `edit_file` (escape hatch: a
    /// trailing `# octos:allow-write` comment on the command line).
    /// Loaded ONCE with the policy (never re-read per call).
    #[serde(default)]
    pub bash_file_writes: BashFileWrites,
}

/// #28b — the three-position knob for bash-driven file writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BashFileWrites {
    /// Default: the shell tool behaves exactly as before (28a receipt only).
    #[default]
    Allow,
    /// Receipt tail appends a nudge when files actually changed.
    Warn,
    /// Write-shaped commands are refused before execution; heuristic-based,
    /// false-negatives tolerated (the 28a receipt still backs it up),
    /// false-positives escape via the `# octos:allow-write` comment.
    Deny,
}

impl ToolPolicy {
    /// Check if a tool name is permitted by this policy (name-based only).
    pub fn is_allowed(&self, tool_name: &str) -> bool {
        matches!(self.evaluate(tool_name), PolicyDecision::Allow)
    }

    /// Full evaluation that returns an allow / deny decision plus a metric
    /// label on deny. Emits `octos_tool_policy_denial_total` with
    /// `reason="robot_tier_gate"` when the deny was driven by a
    /// `group:robot:*` entry, otherwise `reason="policy_deny"`.
    pub fn evaluate(&self, tool_name: &str) -> PolicyDecision {
        // Deny-wins: explicit deny entries take precedence.
        for entry in &self.deny {
            if entry_matches(entry, tool_name) {
                let reason = GENERIC_DENY_REASON;
                counter!(POLICY_DENIAL_COUNTER, "reason" => reason).increment(1);
                return PolicyDecision::Deny { reason };
            }
        }

        // Empty allow list = allow everything not denied.
        if self.allow.is_empty() {
            return PolicyDecision::Allow;
        }

        for entry in &self.allow {
            if entry_matches(entry, tool_name) {
                return PolicyDecision::Allow;
            }
        }

        let reason = GENERIC_DENY_REASON;
        counter!(POLICY_DENIAL_COUNTER, "reason" => reason).increment(1);
        PolicyDecision::Deny { reason }
    }

    /// Check if a tool is permitted by both name policy and tag requirements.
    /// When `require_tags` is non-empty, the tool must have at least one matching tag.
    ///
    /// SECURITY / BEHAVIOR CHANGE (peer-review fix): untagged tools FAIL a
    /// non-empty `require_tags` gate. The previous rule ("tools with no tags
    /// are universal") made the gate fail open on exactly the inputs least
    /// likely to be audited — plugin/skill binaries, MCP server tools, and
    /// newly added builtins all default to `tags() == &[]`, so a profile
    /// confined to `require_tags: [..]` silently exposed all of them. Any
    /// tool that should pass a tag filter must now declare a matching tag.
    /// No compat shim: this deployment does not label tools at all (every
    /// agent is a universal replica of the master, so `require_tags` goes
    /// unused and the blast radius is zero here), and fail-closed is the
    /// defensible default for anyone who does opt in.
    pub fn is_allowed_with_tags(&self, tool_name: &str, tool_tags: &[&str]) -> bool {
        if !self.is_allowed(tool_name) {
            return false;
        }

        // If no tag requirements, pass
        if self.require_tags.is_empty() {
            return true;
        }

        // Tool must have at least one matching required tag. An empty
        // `tool_tags` therefore never matches — fail closed, per above.
        tool_tags
            .iter()
            .any(|tag| self.require_tags.iter().any(|req| req == tag))
    }

    /// True if the policy has no restrictions.
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty() && self.require_tags.is_empty()
    }
}

/// Check if a policy entry (group, wildcard, or exact name) matches a tool name.
pub(crate) fn entry_matches(entry: &str, tool_name: &str) -> bool {
    // Static named groups (group:fs, group:runtime, ...)
    if let Some(tools) = expand_group(entry) {
        return tools.contains(&tool_name);
    }
    // Wildcard: suffix `*` means prefix match
    if let Some(prefix) = entry.strip_suffix('*') {
        return tool_name.starts_with(prefix);
    }
    // Exact match
    entry == tool_name
}

/// Metadata about a named tool group (`group:fs`, `group:runtime`, …) used
/// by [`ToolPolicy`] matching and profile/role tool declarations.
#[derive(Debug, Clone)]
pub struct ToolGroupInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub tools: &'static [&'static str],
}

/// All known tool groups with metadata.
pub const TOOL_GROUPS: &[ToolGroupInfo] = &[
    ToolGroupInfo {
        name: "group:fs",
        description: "File operations: read, write, edit, and diff-edit files",
        tools: &[
            "read_file",
            "write_file",
            "apply_patch",
            "edit_file",
            "diff_edit",
        ],
    },
    ToolGroupInfo {
        name: "group:runtime",
        // #1172: include the Codex-compatible `bash` alias so a policy
        // denying / deferring `group:runtime` covers every shell entry
        // point. Otherwise a profile that disabled runtime execution
        // would still be reachable via `bash(cmd=…)`.
        description: "Shell command execution",
        tools: &["shell", "exec_command", "write_stdin", "bash"],
    },
    ToolGroupInfo {
        name: "group:search",
        description: "File and content search: glob patterns, grep, directory listing",
        tools: &["glob", "grep", "list_dir"],
    },
    ToolGroupInfo {
        name: "group:sessions",
        // #1172: include the Codex-compatible `delegate` one-call
        // wrapper so a policy denying / deferring `group:sessions`
        // covers every subagent entry point. The wrapper keeps an
        // Arc<dyn Tool> for the bound `spawn_agent`, so a policy that
        // removes `spawn` / `spawn_agent` from the visible registry
        // would still leave `delegate` capable of spawning a child
        // without this entry.
        description: "Spawn background subagents for parallel tasks",
        tools: &[
            "spawn",
            "spawn_agent",
            "send_input",
            "resume_agent",
            "wait_agent",
            "close_agent",
            "delegate",
        ],
    },
    ToolGroupInfo {
        name: "group:memory",
        description: "Long-term memory: save and recall knowledge across sessions",
        tools: &[
            "recall_memory",
            "save_memory",
            "memory_note",
            "record_memory_use",
        ],
    },
];

/// Look up group info by name.
pub fn tool_group_info(name: &str) -> Option<&'static ToolGroupInfo> {
    TOOL_GROUPS.iter().find(|g| g.name == name)
}

/// Expand a group name to its tool names. Returns None if not a group.
fn expand_group(name: &str) -> Option<&'static [&'static str]> {
    tool_group_info(name).map(|g| g.tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_policy_allows_all() {
        let policy = ToolPolicy::default();
        assert!(policy.is_allowed("shell"));
        assert!(policy.is_allowed("read_file"));
        assert!(policy.is_allowed("anything"));
        assert!(policy.is_empty());
    }

    #[test]
    fn test_deny_wins_over_allow() {
        let policy = ToolPolicy {
            allow: vec!["shell".into(), "read_file".into()],
            deny: vec!["shell".into()],
            ..Default::default()
        };
        assert!(!policy.is_allowed("shell"));
        assert!(policy.is_allowed("read_file"));
        assert!(!policy.is_allowed("write_file")); // not in allow list
    }

    #[test]
    fn test_group_expansion() {
        let policy = ToolPolicy {
            allow: vec!["group:fs".into()],
            ..Default::default()
        };
        assert!(policy.is_allowed("read_file"));
        assert!(policy.is_allowed("write_file"));
        assert!(policy.is_allowed("edit_file"));
        assert!(policy.is_allowed("diff_edit"));
        assert!(!policy.is_allowed("shell"));
        assert!(!policy.is_allowed("glob"));
    }

    #[test]
    fn test_wildcard_matching() {
        let policy = ToolPolicy {
            deny: vec!["web_*".into()],
            ..Default::default()
        };
        assert!(!policy.is_allowed("web_search"));
        assert!(!policy.is_allowed("web_fetch"));
        assert!(policy.is_allowed("shell"));
        assert!(policy.is_allowed("read_file"));
    }

    #[test]
    fn test_allow_list_filters() {
        let policy = ToolPolicy {
            allow: vec!["group:fs".into(), "group:search".into()],
            ..Default::default()
        };
        assert!(policy.is_allowed("read_file"));
        assert!(policy.is_allowed("glob"));
        assert!(policy.is_allowed("grep"));
        assert!(!policy.is_allowed("shell"));
        assert!(!policy.is_allowed("spawn"));
        assert!(!policy.is_allowed("web_fetch"));
    }

    #[test]
    fn test_deny_group() {
        let policy = ToolPolicy {
            deny: vec!["group:runtime".into()],
            ..Default::default()
        };
        assert!(!policy.is_allowed("shell"));
        assert!(policy.is_allowed("read_file"));
    }

    #[test]
    fn test_serde_roundtrip() {
        let policy = ToolPolicy {
            allow: vec!["group:fs".into()],
            deny: vec!["shell".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: ToolPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.allow, policy.allow);
        assert_eq!(parsed.deny, policy.deny);
    }

    #[test]
    fn test_require_tags_filters_by_tag() {
        let policy = ToolPolicy {
            require_tags: vec!["code".into()],
            ..Default::default()
        };
        // Tool with matching tag passes
        assert!(policy.is_allowed_with_tags("shell", &["runtime", "code"]));
        // Tool without matching tag fails
        assert!(!policy.is_allowed_with_tags("web_search", &["web"]));
        // Tool with no tags fails a non-empty gate (fail closed — this
        // assertion previously locked in the fail-open bypass; see
        // should_fail_closed_for_untagged_tool_when_require_tags_set).
        assert!(!policy.is_allowed_with_tags("custom_tool", &[]));
    }

    #[test]
    fn test_require_tags_deny_still_wins() {
        let policy = ToolPolicy {
            deny: vec!["shell".into()],
            require_tags: vec!["code".into()],
            ..Default::default()
        };
        // Shell has matching tag but is denied
        assert!(!policy.is_allowed_with_tags("shell", &["runtime", "code"]));
        // read_file has matching tag and is not denied
        assert!(policy.is_allowed_with_tags("read_file", &["fs", "code"]));
    }

    #[test]
    fn test_empty_require_tags_allows_all() {
        let policy = ToolPolicy::default();
        assert!(policy.is_allowed_with_tags("anything", &["web"]));
        assert!(policy.is_allowed_with_tags("anything", &[]));
    }

    /// SECURITY (peer-review finding: untagged-tool tag bypass): an untagged
    /// tool must FAIL a non-empty `require_tags` gate. The old "no tags are
    /// universal" rule made the confinement gate fail open on exactly the
    /// inputs least likely to be audited — every plugin/skill binary, every
    /// MCP server tool, and any newly added builtin ships with `tags() ==
    /// &[]`, so a profile confined to `require_tags: ["code"]` still exposed
    /// all of them. This test prevents that bypass from returning.
    #[test]
    fn should_fail_closed_for_untagged_tool_when_require_tags_set() {
        let policy = ToolPolicy {
            require_tags: vec!["code".into()],
            ..Default::default()
        };
        assert!(
            !policy.is_allowed_with_tags("unaudited_plugin_tool", &[]),
            "untagged tools must fail a non-empty require_tags gate (fail closed)"
        );
        // No gate at all (empty require_tags) still passes untagged tools.
        assert!(ToolPolicy::default().is_allowed_with_tags("unaudited_plugin_tool", &[]));
    }
}
