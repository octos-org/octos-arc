//! Guard rules (`arc/guard.py`) and the protected-tree snapshot
//! (`Flow.snapshot_protected/restore_protected`): watch one tool-mode turn's
//! tool events and its final message, produce short corrective sentences for
//! the next prompt, and undo any edit that reached the official tests or
//! requirements.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::prompts::Prompts;

static VERIFY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(npm run build|npm start|npm run start|node \S+\.js|curl\b|playwright|wget\b|node --check)").unwrap()
});
static CLAIM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(implemented|completed?|done|verified|passes|passing|finished|working)\b|✅")
        .unwrap()
});
static REDIRECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:>>?|tee\s+(?:-a\s+)?|cp\s+\S+\s+|mv\s+\S+\s+|sed\s+-i\S*\s+(?:'[^']*'|\S+)\s+)\s*(\S+)"#).unwrap()
});
static SHELL_WRITE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(cat|echo|printf|tee|cp|mv|sed)\b.*(>|tee|-i)").unwrap());
static COPY_MOVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(cp|mv)\s").unwrap());
static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());

const WRITE_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "diff_edit",
    "apply_patch",
    "create_file",
    "append_file",
];
const SHELL_TOOLS: &[&str] = &["bash", "shell", "exec", "run_command", "exec_command"];

/// Observes `tool/started` / `tool/completed` for one turn.
pub struct TurnMonitor {
    protected: Vec<String>,
    allowed: Vec<String>,
    repeat_threshold: u32,
    expect_verification: bool,
    pub wrote_files: bool,
    pub verified: bool,
    pub tool_calls: u32,
    errors_in_a_row: u32,
    last_error: Option<String>,
    max_repeat: u32,
    repeated_error: String,
    pub protected_writes: Vec<String>,
    pub written_paths: Vec<String>,
    final_text: String,
}

impl TurnMonitor {
    pub fn new(protected: Vec<String>, allowed: Vec<String>, expect_verification: bool) -> Self {
        Self {
            protected: protected.into_iter().filter(|p| !p.is_empty()).collect(),
            allowed: allowed.into_iter().filter(|p| !p.is_empty()).collect(),
            repeat_threshold: 3,
            expect_verification,
            wrote_files: false,
            verified: false,
            tool_calls: 0,
            errors_in_a_row: 0,
            last_error: None,
            max_repeat: 0,
            repeated_error: String::new(),
            protected_writes: Vec::new(),
            written_paths: Vec::new(),
            final_text: String::new(),
        }
    }

    pub fn observe(&mut self, method: &str, params: &Value) {
        match method {
            "tool/started" => {
                self.tool_calls += 1;
                let name = params
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                if WRITE_TOOLS.contains(&name) {
                    self.wrote_files = true;
                    let path = args
                        .get("path")
                        .or_else(|| args.get("file_path"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    self.note_path(path);
                } else if SHELL_TOOLS.contains(&name) {
                    let cmd = args
                        .get("cmd")
                        .or_else(|| args.get("command"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if VERIFY.is_match(cmd) {
                        self.verified = true;
                    }
                    if SHELL_WRITE.is_match(cmd) || COPY_MOVE.is_match(cmd) {
                        self.wrote_files = true;
                        for captures in REDIRECT.captures_iter(cmd) {
                            let target = captures[1]
                                .trim_matches(|c| c == '\'' || c == '"')
                                .to_string();
                            self.note_path(&target);
                        }
                    }
                }
            }
            "tool/completed" => {
                let ok = params
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let preview: String = params
                    .get("output_preview")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(300)
                    .collect();
                if !ok {
                    let key = DIGITS.replace_all(&preview, "#").into_owned();
                    if self.last_error.as_deref() == Some(key.as_str()) {
                        self.errors_in_a_row += 1;
                    } else {
                        self.last_error = Some(key);
                        self.errors_in_a_row = 1;
                    }
                    if self.errors_in_a_row > self.max_repeat {
                        self.max_repeat = self.errors_in_a_row;
                        self.repeated_error = preview;
                    }
                } else {
                    self.last_error = None;
                    self.errors_in_a_row = 0;
                }
            }
            _ => {}
        }
    }

    fn note_path(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        self.written_paths.push(path.to_string());
        if self
            .allowed
            .iter()
            .any(|a| path.starts_with(a) || path.contains(&format!("/{a}")))
        {
            return;
        }
        if self
            .protected
            .iter()
            .any(|p| path.starts_with(p) || path.contains(&format!("/{p}")))
        {
            self.protected_writes.push(path.to_string());
        }
    }

    pub fn finish(&mut self, final_text: &str) {
        self.final_text = final_text.to_string();
    }

    /// Corrective sentences for the next prompt.
    pub fn corrections(&self, prompts: &Prompts) -> Vec<String> {
        let mut out = Vec::new();
        if self.expect_verification
            && self.wrote_files
            && !self.verified
            && CLAIM.is_match(&self.final_text)
        {
            out.push(
                prompts
                    .correction("claim_without_verification", &[])
                    .unwrap_or_default(),
            );
        }
        if self.max_repeat >= self.repeat_threshold {
            let error: String = self.repeated_error.chars().take(160).collect();
            out.push(
                prompts
                    .correction(
                        "repeated_error",
                        &[
                            ("count", &self.max_repeat.to_string()),
                            ("error", &format!("{error:?}")),
                        ],
                    )
                    .unwrap_or_default(),
            );
        }
        if !self.protected_writes.is_empty() {
            let mut files: Vec<String> = self.protected_writes.clone();
            files.sort();
            files.dedup();
            files.truncate(5);
            out.push(
                prompts
                    .correction("protected_writes", &[("files", &files.join(", "))])
                    .unwrap_or_default(),
            );
        }
        out.into_iter().filter(|c| !c.is_empty()).collect()
    }
}

/// rel path → sha256 for every regular file under root (node_modules skipped).
pub fn tree_digest(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if entry.file_name() != "node_modules" {
                    stack.push(path);
                }
            } else if path.is_file()
                && let Ok(bytes) = std::fs::read(&path)
            {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, format!("{:x}", Sha256::digest(&bytes)));
            }
        }
    }
    out
}

/// Make `live` match `snapshot` again: rewrite changed/deleted files, remove
/// added ones. Returns the relative paths that had to be fixed.
pub fn restore_tree(
    live: &Path,
    snapshot: &Path,
    expected: &BTreeMap<String, String>,
) -> std::io::Result<Vec<String>> {
    let mut fixed = Vec::new();
    let current = tree_digest(live);
    for (rel, digest) in expected {
        if current.get(rel) != Some(digest) {
            let dest = live.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(snapshot.join(rel), &dest)?;
            fixed.push(rel.clone());
        }
    }
    for rel in current.keys() {
        if !expected.contains_key(rel) {
            let _ = std::fs::remove_file(live.join(rel));
            fixed.push(rel.clone());
        }
    }
    fixed.sort();
    Ok(fixed)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        if entry.file_name() == "node_modules" {
            continue;
        }
        let dest = to.join(entry.file_name());
        if source.is_dir() {
            copy_tree(&source, &dest)?;
        } else {
            std::fs::copy(&source, &dest)?;
        }
    }
    Ok(())
}

/// Copies of the official tests and requirements taken before the first
/// turn, so any edit the model sneaks past the hook (e.g. via a shell
/// redirect) is undone after the turn — the platform grades with THESE files.
pub struct ProtectedTrees {
    snapshots: Vec<(PathBuf, PathBuf, BTreeMap<String, String>)>,
    scratch: Option<tempfile::TempDir>,
}

impl ProtectedTrees {
    pub fn snapshot(dirs: &[PathBuf]) -> std::io::Result<Self> {
        let scratch = tempfile::Builder::new()
            .prefix("octos-protected-")
            .tempdir()?;
        let mut snapshots = Vec::new();
        for (index, live) in dirs.iter().enumerate() {
            if !live.is_dir() {
                continue;
            }
            let snap = scratch.path().join(format!("tree-{index}"));
            copy_tree(live, &snap)?;
            snapshots.push((live.clone(), snap, tree_digest(live)));
        }
        Ok(Self {
            snapshots,
            scratch: Some(scratch),
        })
    }

    /// Restore every protected tree; returns `live/rel` for each fixed file.
    pub fn restore(&self) -> Vec<String> {
        let mut fixed_all = Vec::new();
        for (live, snap, digest) in &self.snapshots {
            if let Ok(fixed) = restore_tree(live, snap, digest) {
                fixed_all.extend(
                    fixed
                        .into_iter()
                        .map(|rel| format!("{}/{rel}", live.display())),
                );
            }
        }
        let _ = &self.scratch;
        fixed_all
    }
}

/// Lexical normalisation (`os.path.realpath` without touching the disk):
/// absolute path with `.` and `..` folded.
fn normalize(path: &Path, cwd: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `before_tool_call` hook body (`arc/hooks/deny_protected.py`): refuse file
/// writes whose target lies inside a protected directory. Returns the reason
/// to deny with, `None` to allow. Unreadable payloads allow — the hook must
/// never block work by accident; the post-turn restore is the second layer.
pub fn deny_protected(payload: &Value, protected: &[PathBuf]) -> Option<String> {
    let args = payload.get("arguments")?.as_object()?;
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    // Resolve the working directory once so relative targets and the
    // protected roots compare on the same (symlink-free) prefix.
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let roots: Vec<PathBuf> = protected
        .iter()
        .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| normalize(root, &cwd)))
        .collect();
    for key in ["path", "file_path", "filename", "file"] {
        let Some(value) = args
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let target = normalize(Path::new(value), &cwd);
        let real_parent = target
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .and_then(|parent| target.file_name().map(|name| parent.join(name)));
        for root in &roots {
            let inside = |candidate: &Path| candidate == root || candidate.starts_with(root);
            if inside(&target) || real_parent.as_deref().is_some_and(inside) {
                return Some(format!(
                    "Denied: {value} is inside the protected directory {} (official tests / requirements are read-only). Change frontend/ or backend/ instead.",
                    root.display()
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn started(name: &str, args: Value) -> Value {
        json!({"tool_call_id": "1", "tool_name": name, "arguments": args})
    }

    #[test]
    fn should_flag_completion_claims_without_verification() {
        let prompts = Prompts::builtin();
        let mut m = TurnMonitor::new(vec![".arc/".into()], vec![".arc/design/".into()], true);
        m.observe(
            "tool/started",
            &started("write_file", json!({"path": "backend/server.js"})),
        );
        m.finish("All done, the feature is implemented.");
        let corrections = m.corrections(&prompts);
        assert_eq!(corrections.len(), 1);
        assert!(corrections[0].contains("claimed completion"));
        let mut verified = TurnMonitor::new(vec![], vec![], true);
        verified.observe("tool/started", &started("write_file", json!({"path": "a"})));
        verified.observe(
            "tool/started",
            &started("shell", json!({"command": "npm run build"})),
        );
        verified.finish("done");
        assert!(verified.corrections(&prompts).is_empty());
    }

    #[test]
    fn should_flag_the_same_error_three_times_and_protected_writes() {
        let prompts = Prompts::builtin();
        let mut m = TurnMonitor::new(
            vec!["/w/tests".into(), ".arc/".into()],
            vec![".arc/design/".into()],
            false,
        );
        for _ in 0..3 {
            m.observe(
                "tool/completed",
                &json!({"success": false, "output_preview": "EADDRINUSE port 3100"}),
            );
        }
        m.observe(
            "tool/started",
            &started("edit_file", json!({"path": "/w/tests/REQ-1.spec.ts"})),
        );
        m.observe(
            "tool/started",
            &started("write_file", json!({"path": ".arc/design/REQ-1.json"})),
        );
        m.observe(
            "tool/started",
            &started(
                "shell",
                json!({"command": "echo x > /w/tests/support/e2e.ts"}),
            ),
        );
        let corrections = m.corrections(&prompts);
        assert_eq!(corrections.len(), 2);
        assert!(corrections[0].contains("3 times in a row"));
        assert!(corrections[1].contains("/w/tests/REQ-1.spec.ts"));
        assert!(corrections[1].contains("/w/tests/support/e2e.ts"));
        assert!(!corrections[1].contains("design"));
        assert_eq!(m.tool_calls, 3);
    }

    #[test]
    fn should_restore_changed_deleted_and_added_files() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("tests");
        std::fs::create_dir_all(live.join("support")).unwrap();
        std::fs::write(live.join("REQ-1.spec.ts"), "original").unwrap();
        std::fs::write(live.join("support/e2e.ts"), "helper").unwrap();
        let protected = ProtectedTrees::snapshot(std::slice::from_ref(&live)).unwrap();
        std::fs::write(live.join("REQ-1.spec.ts"), "tampered").unwrap();
        std::fs::remove_file(live.join("support/e2e.ts")).unwrap();
        std::fs::write(live.join("playwright.config.ts"), "injected").unwrap();
        let fixed = protected.restore();
        assert_eq!(fixed.len(), 3);
        assert_eq!(
            std::fs::read_to_string(live.join("REQ-1.spec.ts")).unwrap(),
            "original"
        );
        assert_eq!(
            std::fs::read_to_string(live.join("support/e2e.ts")).unwrap(),
            "helper"
        );
        assert!(!live.join("playwright.config.ts").exists());
        assert!(protected.restore().is_empty());
    }

    #[test]
    fn should_deny_writes_inside_protected_directories_only() {
        let dir = tempfile::tempdir().unwrap();
        let tests = dir.path().join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        let protected = vec![tests.clone()];
        let cwd = dir.path().to_string_lossy().into_owned();
        let denied = deny_protected(
            &json!({"cwd": cwd, "arguments": {"path": "tests/REQ-1.spec.ts"}}),
            &protected,
        );
        assert!(denied.unwrap().contains("Denied"));
        let nested = deny_protected(
            &json!({"cwd": cwd, "arguments": {"file_path": "frontend/../tests/support/e2e.ts"}}),
            &protected,
        );
        assert!(nested.is_some());
        assert!(
            deny_protected(
                &json!({"cwd": cwd, "arguments": {"path": "frontend/src/index.html"}}),
                &protected
            )
            .is_none()
        );
        assert!(deny_protected(&json!({"arguments": "not an object"}), &protected).is_none());
        assert!(deny_protected(&json!({}), &protected).is_none());
    }
}
