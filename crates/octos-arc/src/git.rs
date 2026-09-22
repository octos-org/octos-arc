//! Git operations the harness needs (`arcbench_agent_runtime/gitops.py` and
//! the worktree snapshot helpers of `arc/acceptance.py`): commits on
//! improvement, restore of the best state, undo of test-run mutations.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eyre::{Result, bail};

use crate::envs::{self, EnvVec};
use crate::process::{self, Output};

const DEFAULT_USER_NAME: &str = "ARC Bench Agent";
const DEFAULT_USER_EMAIL: &str = "arcbench@example.com";
const GITIGNORE_START: &str = "# >>> arcbench-agent-runtime >>>";
const GITIGNORE_END: &str = "# <<< arcbench-agent-runtime <<<";

pub struct Git {
    root: PathBuf,
    env: EnvVec,
}

impl Git {
    pub fn new(root: &Path) -> Self {
        let name = ["ARC_GIT_USER_NAME", "GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
            .unwrap_or_else(|| DEFAULT_USER_NAME.into());
        let email = [
            "ARC_GIT_USER_EMAIL",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_EMAIL",
        ]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_USER_EMAIL.into());
        let env = envs::inherited(&[
            ("GIT_AUTHOR_NAME", Some(name.trim())),
            ("GIT_AUTHOR_EMAIL", Some(email.trim())),
            ("GIT_COMMITTER_NAME", Some(name.trim())),
            ("GIT_COMMITTER_EMAIL", Some(email.trim())),
        ]);
        Self {
            root: root.to_path_buf(),
            env,
        }
    }

    pub fn run(&self, args: &[&str]) -> Result<Output> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        process::run(
            Path::new("git"),
            &args,
            &self.root,
            &self.env,
            Instant::now() + Duration::from_secs(120),
        )
    }

    fn checked(&self, args: &[&str]) -> Result<Output> {
        let output = self.run(args)?;
        if !output.succeeded() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                if output.stderr.trim().is_empty() {
                    output.stdout.trim()
                } else {
                    output.stderr.trim()
                }
            );
        }
        Ok(output)
    }

    /// Init when needed, configure the identity, write the managed .gitignore
    /// block and make the initial commit (nothing-to-commit tolerated).
    pub fn ensure_repo(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        if !self.root.join(".git").exists() {
            self.checked(&["init"])?;
        }
        let name = envs::lookup(&self.env, "GIT_AUTHOR_NAME")
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_default();
        let email = envs::lookup(&self.env, "GIT_AUTHOR_EMAIL")
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.checked(&["config", "user.name", &name])?;
        self.checked(&["config", "user.email", &email])?;
        self.ensure_gitignore()?;
        self.checked(&["add", "."])?;
        let commit = self.run(&["commit", "-m", "init"])?;
        if !commit.succeeded()
            && !format!("{}{}", commit.stdout, commit.stderr).contains("nothing to commit")
        {
            bail!("git init commit failed: {}", commit.stderr.trim());
        }
        Ok(())
    }

    fn ensure_gitignore(&self) -> Result<()> {
        let path = self.root.join(".gitignore");
        let block = [
            GITIGNORE_START,
            "backend/node_modules/",
            "frontend/node_modules/",
            "backend/coverage/",
            "frontend/dist/",
            "frontend/dist-ssr/",
            "*.db",
            ".env",
            ".arc/*",
            "!.arc/traceability/",
            "!.arc/traceability/**",
            GITIGNORE_END,
        ]
        .join("\n");
        let old = std::fs::read_to_string(&path).unwrap_or_default();
        let content = match (old.find(GITIGNORE_START), old.find(GITIGNORE_END)) {
            (Some(start), Some(end)) if end > start => {
                let before = old[..start].trim_end();
                let after = old[end + GITIGNORE_END.len()..].trim_start();
                let mut merged = String::new();
                if !before.is_empty() {
                    merged.push_str(before);
                    merged.push_str("\n\n");
                }
                merged.push_str(&block);
                if !after.is_empty() {
                    merged.push_str("\n\n");
                    merged.push_str(after);
                }
                format!("{}\n", merged.trim())
            }
            _ if !old.trim().is_empty() => format!("{}\n\n{block}\n", old.trim_end()),
            _ => format!("{block}\n"),
        };
        std::fs::write(path, content)?;
        Ok(())
    }

    pub fn head(&self) -> Option<String> {
        let output = self.run(&["rev-parse", "HEAD"]).ok()?;
        output
            .succeeded()
            .then(|| output.stdout.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// `git add . && git commit`; false when there was nothing to commit.
    pub fn commit(&self, message: &str) -> Result<bool> {
        self.checked(&["add", "."])?;
        let output = self.run(&["commit", "-m", message])?;
        if output.succeeded() {
            return Ok(true);
        }
        let text = format!("{}{}", output.stdout, output.stderr).to_lowercase();
        if text.contains("nothing to commit") {
            return Ok(false);
        }
        bail!("git commit failed: {}", output.stderr.trim());
    }

    /// Stage everything so `restore_worktree` can undo what a test run
    /// mutates (persisted JSON stores, uploaded files) without losing the
    /// model's edits.
    pub fn snapshot_worktree(&self) {
        let _ = self.run(&["add", "-A"]);
    }

    /// Return tracked files to the staged snapshot and drop files a test run
    /// created; ignored build outputs (node_modules, dist) are left alone.
    pub fn restore_worktree(&self) {
        let _ = self.run(&["checkout", "--", "."]);
        let _ = self.run(&[
            "clean",
            "-fdq",
            "-e",
            "node_modules",
            "-e",
            "dist",
            "--",
            "frontend",
            "backend",
        ]);
    }

    /// Bring frontend/ and backend/ back to `sha` (the best acceptance state):
    /// files the commit has are checked out, files it lacks (added by a
    /// regressing repair, staged by the worktree snapshot) are removed, and
    /// untracked leftovers are cleaned.
    pub fn restore_app(&self, sha: &str) {
        let parts: Vec<&str> = ["frontend", "backend"]
            .into_iter()
            .filter(|p| self.root.join(p).exists())
            .collect();
        if parts.is_empty() {
            return;
        }
        let mut checkout = vec!["checkout", sha, "--"];
        checkout.extend(&parts);
        let _ = self.run(&checkout);
        let mut tree_args = vec!["ls-tree", "-r", "--name-only", sha, "--"];
        tree_args.extend(&parts);
        let in_commit: std::collections::BTreeSet<String> = self
            .run(&tree_args)
            .map(|o| o.stdout.lines().map(str::to_string).collect())
            .unwrap_or_default();
        let mut index_args = vec!["ls-files", "--"];
        index_args.extend(&parts);
        let in_index: Vec<String> = self
            .run(&index_args)
            .map(|o| o.stdout.lines().map(str::to_string).collect())
            .unwrap_or_default();
        for path in in_index.iter().filter(|p| !in_commit.contains(*p)) {
            let _ = self.run(&["rm", "-f", "-q", "--", path]);
        }
        let mut clean = vec!["clean", "-fd", "-e", "node_modules", "-e", "dist", "--"];
        clean.extend(&parts);
        let _ = self.run(&clean);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_undo_test_run_mutations_but_keep_uncommitted_edits() {
        let dir = tempfile::tempdir().unwrap();
        let git = Git::new(dir.path());
        std::fs::create_dir_all(dir.path().join("backend")).unwrap();
        std::fs::write(dir.path().join("backend/db.json"), "{\"count\": 0}").unwrap();
        std::fs::write(dir.path().join("backend/server.js"), "v1").unwrap();
        git.ensure_repo().unwrap();
        assert!(git.head().is_some());
        assert!(
            std::fs::read_to_string(dir.path().join(".gitignore"))
                .unwrap()
                .contains("!.arc/traceability/")
        );
        std::fs::write(
            dir.path().join("backend/server.js"),
            "v2 (repair edit, uncommitted)",
        )
        .unwrap();
        git.snapshot_worktree();
        std::fs::write(dir.path().join("backend/db.json"), "{\"count\": -1}").unwrap();
        std::fs::write(dir.path().join("backend/uploads.json"), "[]").unwrap();
        git.restore_worktree();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("backend/db.json")).unwrap(),
            "{\"count\": 0}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("backend/server.js")).unwrap(),
            "v2 (repair edit, uncommitted)"
        );
        assert!(!dir.path().join("backend/uploads.json").exists());
        assert!(git.commit("repair").unwrap());
        assert!(!git.commit("nothing").unwrap());
    }

    #[test]
    fn should_restore_app_to_a_previous_commit() {
        let dir = tempfile::tempdir().unwrap();
        let git = Git::new(dir.path());
        std::fs::create_dir_all(dir.path().join("frontend")).unwrap();
        std::fs::write(dir.path().join("frontend/index.html"), "best").unwrap();
        git.ensure_repo().unwrap();
        let best = git.head().unwrap();
        // A regressing repair: edits and new files stay uncommitted (the flow
        // only commits improvements), so restoring the best commit drops them.
        std::fs::write(dir.path().join("frontend/index.html"), "worse").unwrap();
        std::fs::write(dir.path().join("frontend/extra.js"), "junk").unwrap();
        git.snapshot_worktree();
        git.restore_app(&best);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("frontend/index.html")).unwrap(),
            "best"
        );
        assert!(!dir.path().join("frontend/extra.js").exists());
    }
}
