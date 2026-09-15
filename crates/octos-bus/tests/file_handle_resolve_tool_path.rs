//! Empirical resolution table for the unified [`resolve_tool_path`].
//!
//! Every row in the "Input forms the LLM produces" table from
//! `refactor: unified file-path resolver` is exercised here. If any
//! row regresses, ONE of these tests fails — kills the upload-handle
//! bug class (#586, #857, #930, #931, #932, #933) at source.

use std::path::{Path, PathBuf};

use octos_bus::file_handle::{
    ResolvedToolPath, ToolPathError, ToolPathScope, resolve_tool_path, temp_upload_root,
};
use tempfile::TempDir;

/// Stand-up rig for the row-by-row table: a fake workspace, a fake
/// profile root, and a real per-test directory inside the global
/// `temp_upload_root()` (so the canonicalize-based existence check
/// passes without leaking across tests).
struct Rig {
    workspace: TempDir,
    profile: TempDir,
    /// Directory inside `temp_upload_root()` that holds the test's
    /// fake upload payload. Drop cleans it up.
    upload_dir: PathBuf,
}

impl Rig {
    fn new(tag: &str) -> Self {
        let workspace = tempfile::tempdir().expect("workspace tmpdir");
        let profile = tempfile::tempdir().expect("profile tmpdir");

        // Real subdirectory under temp_upload_root() so the
        // canonicalize-based existence checks succeed.
        let upload_root = temp_upload_root();
        std::fs::create_dir_all(&upload_root).expect("upload root");
        let upload_dir = upload_root.join(format!(
            "resolve-tool-path-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&upload_dir).expect("upload dir");

        Self {
            workspace,
            profile,
            upload_dir,
        }
    }

    fn workspace_root(&self) -> &Path {
        self.workspace.path()
    }

    fn profile_root(&self) -> &Path {
        self.profile.path()
    }

    /// Build a real workspace-relative file and return both its
    /// relative form and the canonicalised absolute path.
    fn make_workspace_file(&self, relative: &str, body: &[u8]) -> PathBuf {
        let abs = self.workspace.path().join(relative);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).expect("workspace parent dir");
        }
        std::fs::write(&abs, body).expect("write workspace file");
        std::fs::canonicalize(&abs).expect("canonicalise workspace file")
    }

    /// Build a real file inside the rig's upload directory and return
    /// its canonicalised absolute path.
    fn make_upload_file(&self, name: &str, body: &[u8]) -> PathBuf {
        let abs = self.upload_dir.join(name);
        std::fs::write(&abs, body).expect("write upload file");
        std::fs::canonicalize(&abs).expect("canonicalise upload file")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.upload_dir);
    }
}

fn expect_resolved(result: Result<ResolvedToolPath, ToolPathError>) -> ResolvedToolPath {
    result.unwrap_or_else(|err| panic!("resolution must succeed, got {err}"))
}

fn expect_err(result: Result<ResolvedToolPath, ToolPathError>) -> ToolPathError {
    result.expect_err("resolution must fail")
}

#[test]
fn row_1_workspace_relative_input() {
    let rig = Rig::new("row1");
    let _abs = rig.make_workspace_file("foo/bar.txt", b"hi");

    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        "foo/bar.txt",
    ));
    assert_eq!(resolved.scope, ToolPathScope::Workspace);
    // Workspace-relative resolution returns the LEXICAL workspace path
    // on purpose — file tools layer `O_NOFOLLOW` over the resolved
    // path, and canonicalising here would silently follow symlinks
    // before that gate gets to run. See `row_workspace_keeps_lexical_for_symlink_safety`.
    assert_eq!(resolved.absolute, rig.workspace_root().join("foo/bar.txt"));
}

#[test]
fn row_3_absolute_inside_upload_tmpdir_kept() {
    let rig = Rig::new("row3");
    let abs = rig.make_upload_file("019e22ab-cd-real-upload.wav", b"WAV");

    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        &abs.to_string_lossy(),
    ));
    assert_eq!(resolved.scope, ToolPathScope::UploadTmpdir);
    assert!(
        resolved
            .absolute
            .starts_with(std::fs::canonicalize(temp_upload_root()).unwrap()),
        "resolved {:?} must canonicalise under {:?}",
        resolved.absolute,
        temp_upload_root()
    );
}

#[test]
fn row_9_parent_traversal_rejected() {
    let rig = Rig::new("row9");
    let err = expect_err(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        "../../etc/passwd",
    ));
    assert_eq!(err, ToolPathError::Traversal);
}

#[test]
fn row_10_absolute_outside_all_allowed_roots_rejected() {
    let rig = Rig::new("row10");
    // A named file directly under the system temp directory is a
    // sibling of the workspace/profile tempdirs and `octos-uploads`,
    // so it is a portable absolute target outside every allowed root.
    let outside = tempfile::NamedTempFile::new().expect("outside temp file");
    let err = expect_err(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        &outside.path().to_string_lossy(),
    ));
    assert_eq!(err, ToolPathError::OutsideAllowedRoots);
}

// ---------- Extra contract pins ----------

#[test]
fn workspace_root_absolute_without_profile_still_works() {
    // The most common live call path passes `profile_root = None`
    // (read_file/write_file/etc. don't track the profile root). The
    // resolver must still operate on workspace + upload tmpdir.
    let rig = Rig::new("no-profile");
    let _abs = rig.make_workspace_file("hello.txt", b"x");

    let resolved = expect_resolved(resolve_tool_path(rig.workspace_root(), None, "hello.txt"));
    assert_eq!(resolved.scope, ToolPathScope::Workspace);
    // Lexical workspace path — same reason as `row_1_workspace_relative_input`.
    assert_eq!(resolved.absolute, rig.workspace_root().join("hello.txt"));
}

#[cfg(unix)]
#[test]
fn workspace_relative_symlink_resolution_is_lexical_not_canonical() {
    // Security regression pin (codex review, 2026-05-13): the
    // workspace-relative branch must NOT canonicalise the result.
    // File tools layer `O_NOFOLLOW` over the resolved path; if the
    // resolver canonicalised it would silently translate
    // `workspace/secret -> /etc/passwd` into `/etc/passwd` and the
    // open-time gate would then see a plain file instead of a
    // symlink. The contract is: workspace scope returns the lexical
    // workspace path, the tool's open-time gate is responsible for
    // refusing symlinks.
    let rig = Rig::new("sym-safety");
    let workspace = rig.workspace_root();
    let outside = tempfile::tempdir().expect("outside tmpdir");
    std::fs::write(outside.path().join("passwd"), b"root:x:0:0").unwrap();
    // workspace/secret -> outside/passwd
    let link = workspace.join("secret");
    std::os::unix::fs::symlink(outside.path().join("passwd"), &link).unwrap();

    let resolved = expect_resolved(resolve_tool_path(workspace, None, "secret"));
    assert_eq!(resolved.scope, ToolPathScope::Workspace);
    // The resolver must NOT have followed the symlink — the resolved
    // absolute must still be the workspace path (lexical), not the
    // outside target.
    assert_eq!(resolved.absolute, workspace.join("secret"));
    let outside_path = outside.path().join("passwd");
    assert_ne!(
        resolved.absolute, outside_path,
        "resolver must not follow symlinks for workspace-relative paths"
    );
}
