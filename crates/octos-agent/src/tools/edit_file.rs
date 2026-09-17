//! Edit file tool for making precise text replacements.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::write_grant::WritePathGrant;
use super::{ConcurrencyClass, Tool, ToolContext, ToolResult};
use crate::policy::{FileAccessMode, FilesystemScope};

/// Tool for editing files via string replacement.
pub struct EditFileTool {
    /// Base directory for resolving relative paths.
    base_dir: PathBuf,
    /// Effective filesystem scope.
    filesystem_scope: FilesystemScope,
    /// Whether writes are permitted.
    file_access: FileAccessMode,
    /// #1976 — optional per-path write fence. Under `create_only` EVERY edit
    /// is refused (allowlisted paths may only be created); otherwise edits
    /// follow the allowlist. `None` = pre-#1976 behaviour.
    write_grant: Option<WritePathGrant>,
}

impl EditFileTool {
    /// Create a new edit file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadWrite,
            write_grant: None,
        }
    }

    /// Set the effective filesystem scope.
    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    /// Set the effective file access mode.
    pub fn with_file_access(mut self, file_access: FileAccessMode) -> Self {
        self.file_access = file_access;
        self
    }

    /// #1976 — bind a per-path write fence (see
    /// [`WriteFileTool::with_write_grant`](super::WriteFileTool::with_write_grant)).
    pub fn with_write_grant(mut self, write_grant: WritePathGrant) -> Self {
        self.write_grant = Some(write_grant);
        self
    }
}

#[derive(Debug, Deserialize)]
// #1770: unknown keys are usually a typo of a real parameter; rejecting
// them (with a did-you-mean via `args::parse_tool_args`) lets the model
// self-correct instead of silently dropping its intent.
#[serde(deny_unknown_fields)]
struct EditFileInput {
    /// #1767: `filePath` is the industry-convention alias.
    #[serde(alias = "filePath")]
    path: String,
    #[serde(alias = "oldString")]
    old_string: String,
    #[serde(alias = "newString")]
    new_string: String,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing a string with a new string. An exact match of old_string is preferred; when none exists, whitespace-, indentation- and escape-tolerant fuzzy matching is tried as a fallback. The old_string must identify a single location."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        // edit_file rewrites a file in place — same race hazard as
        // write_file. Serialize the whole batch. See M8.8.
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (alias: filePath)"
                },
                "old_string": {
                    "type": "string",
                    "description": "The string to find and replace. An exact match is preferred; minor whitespace/indentation/escape differences are tolerated as a fallback. Must identify a unique location. (alias: oldString)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The string to replace it with (alias: newString)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        // M8.4: legacy entry point routes through the typed path with a
        // zero-value context so out-of-band callers still exercise the same
        // file-state-cache invalidation logic.
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let input: EditFileInput =
            super::args::parse_tool_args(self.name(), &self.input_schema(), args)?;

        if !self.file_access.allows_write() {
            return Ok(ToolResult {
                output: "edit_file is not permitted by read-only filesystem access".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // Phase 2-C of the SessionScope migration: when the host has
        // threaded a scope through `ToolContext`, use it as the single
        // source of truth for base_dir + path classification. Same
        // write policy as `write_file` — `InWorkspace` and
        // `InGrantedDir` allowed; `InSharedZone` and `OutOfScope`
        // refused. The shared helper canonicalizes the candidate before
        // classification so ancestor symlinks can't smuggle an edit
        // out of the workspace.
        let path = match ctx.session_scope.as_ref() {
            Some(scope) => match super::resolve_path_for_session_scope_write(scope, &input.path) {
                Ok(p) => p,
                Err(reason) => {
                    return Ok(ToolResult {
                        output: format!("{reason}: {}", input.path),
                        success: false,
                        ..Default::default()
                    });
                }
            },
            None => match super::resolve_path_with_scope(
                &self.base_dir,
                &input.path,
                self.filesystem_scope,
            ) {
                Ok(p) => p,
                Err(_) => {
                    return Ok(ToolResult {
                        output: format!("Path outside working directory: {}", input.path),
                        success: false,
                        ..Default::default()
                    });
                }
            },
        };

        // #1976 — per-path write fence, BEFORE any file I/O. SECURITY ROUND
        // (codex): a fenced edit must read AND write through ONE confined
        // handle so an ancestor swapped between the read and the write cannot
        // redirect the edit. Under create_only every edit is refused ("created,
        // never modified"); otherwise `check_edit` returns the workspace-
        // relative path and `confined_open_rdwr` opens the leaf `O_RDWR` via a
        // component-wise `O_NOFOLLOW` `openat` walk (symlinked ancestor →
        // refused). The returned handle is reused for the write-back below, so
        // read and write bind to the same walked object. Unfenced edits keep
        // the historical `read_no_follow` + `write_no_follow`.
        let mut fenced: Option<(std::fs::File, std::path::PathBuf)> = None;
        let content = if let Some(grant) = &self.write_grant {
            let workspace_root = ctx
                .session_scope
                .as_ref()
                .map(|scope| scope.workspace().to_path_buf())
                .unwrap_or_else(|| self.base_dir.clone());
            let rel = match grant.check_edit(&workspace_root, &path, &input.path, self.name()) {
                Ok(rel) => rel,
                Err(denied) => {
                    return Ok(ToolResult {
                        output: denied,
                        success: false,
                        ..Default::default()
                    });
                }
            };
            match super::write_grant::confined_open_rdwr(workspace_root.clone(), rel).await {
                Ok((file, content)) => {
                    fenced = Some((file, workspace_root));
                    content
                }
                Err(e) => {
                    return Ok(ToolResult {
                        output: grant.map_confined_error(
                            &e,
                            &workspace_root,
                            &input.path,
                            self.name(),
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
            }
        } else {
            // Read current content (O_NOFOLLOW atomically rejects symlinks)
            match super::read_no_follow(&path).await {
                Ok(c) => c,
                Err(e) => return Ok(super::file_io_error(e, &input.path)),
            }
        };

        if input.old_string.is_empty() {
            return Ok(ToolResult {
                output: "old_string must not be empty".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // #1771: cascading replacer chain — exact match first, then
        // increasingly whitespace/indentation/escape-tolerant fallbacks.
        let (range, replacer_name) = match super::replacer::find_replacement(
            &content,
            &input.old_string,
        ) {
            super::replacer::ChainOutcome::Match { range, replacer } => (range, replacer),
            super::replacer::ChainOutcome::Ambiguous { count, replacer } => {
                return Ok(ToolResult {
                    output: format!(
                        "Found {count} occurrences of the string (via {replacer} replacer). Please provide more context to make the match unique.",
                    ),
                    success: false,
                    ..Default::default()
                });
            }
            super::replacer::ChainOutcome::NoMatch => {
                return Ok(ToolResult {
                    output: format!(
                        "String not found in file. No exact match, and no fuzzy match via the line-trimmed, whitespace-normalized, indentation-flexible, escape-normalized or block-anchor replacers.\n\nSearched for:\n```\n{}\n```",
                        input.old_string
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // When the match came from the escape_normalized replacer, BOTH call
        // strings carry the same double-escaping pathology — interpret
        // new_string (and the guard's needle) with the same unescape rules
        // that made old_string match. Splicing new_string verbatim would
        // write literal `\n` text into the file as code, and guarding
        // against the still-escaped old_string (1 physical line) would
        // falsely reject every legitimate multi-line match (#1771 review).
        let (guard_needle, splice_new) = if replacer_name == "escape_normalized" {
            (
                super::replacer::unescape_find(&input.old_string),
                super::replacer::unescape_find(&input.new_string),
            )
        } else {
            (input.old_string.clone(), input.new_string.clone())
        };

        // Safety guard: a fuzzy matcher must never silently swallow far more
        // of the file than the old_string described.
        let matched_text = &content[range.clone()];
        if super::replacer::is_disproportionate_match(matched_text, &guard_needle) {
            return Ok(ToolResult {
                output: format!(
                    "Fuzzy match rejected as disproportionate: the {replacer_name} replacer matched {} lines / {} bytes for an old_string of {} lines / {} bytes. Provide more context so the match is precise.",
                    matched_text.lines().count(),
                    matched_text.len(),
                    guard_needle.lines().count(),
                    guard_needle.len(),
                ),
                success: false,
                ..Default::default()
            });
        }

        if replacer_name != "exact" {
            tracing::info!(
                replacer = replacer_name,
                path = %input.path,
                "edit_file fuzzy match succeeded"
            );
        }

        // Perform replacement by byte range — the fuzzy-matched span may
        // occur elsewhere as a plain substring, so a string replacen could
        // hit the wrong occurrence.
        let mut new_content = String::with_capacity(content.len() - range.len() + splice_new.len());
        new_content.push_str(&content[..range.start]);
        new_content.push_str(&splice_new);
        new_content.push_str(&content[range.end..]);

        // Write back. A fenced edit rewrites the SAME confined handle opened
        // above (truncate + write from offset 0) — no re-open, so no
        // ancestor-swap window between read and write. Unfenced edits keep the
        // historical lexical `write_no_follow`.
        if let Some((file, workspace_root)) = fenced.take() {
            if let Err(e) =
                super::write_grant::confined_rewrite(file, new_content.as_bytes().to_vec()).await
            {
                // `map_confined_error` records the typed `[denied]`/[io] to the
                // sink; a rewrite failure is not create_only-relevant.
                let grant = self.write_grant.as_ref().expect("fenced implies a grant");
                return Ok(ToolResult {
                    output: grant.map_confined_error(&e, &workspace_root, &input.path, self.name()),
                    success: false,
                    ..Default::default()
                });
            }
        } else if let Err(e) = super::write_no_follow(&path, new_content.as_bytes()).await {
            return Ok(super::file_io_error(e, &input.path));
        }

        // #1976 SECURITY ROUND 2 (codex): under a per-path write fence the
        // post-write processors that re-resolve the LEXICAL path are SKIPPED —
        // a fenced edit is a leaf-file operation, not a project mutation.
        // Formatting, for example, would run an external tool on a lexical
        // filename. `write_grant` Some at this point means the edit went
        // through the confined path above. Cache invalidation stays (pure
        // in-memory, path-keyed).
        let fence_active = self.write_grant.is_some();

        // #1774: opt-in post-edit formatting. Runs BEFORE cache invalidation
        // so both observe the final on-disk content. Best-effort by contract
        // — a formatter failure never fails the edit.
        // Never runs under a fence (see above).
        let format_note = if ctx.format_after_edit && !fence_active {
            crate::format::post_edit_format_note(&path, &new_content).await
        } else {
            None
        };

        // M8.4: invalidate any stale cache entry — the file's contents and
        // mtime just changed.
        if let Some(cache) = ctx.file_state_cache.as_ref() {
            cache.invalidate(&path);
        }

        // Report which replacer produced the match. The exact-match wording
        // is kept identical to the historical output for compatibility.
        let output = if replacer_name == "exact" {
            format!("Successfully edited {}", input.path)
        } else {
            format!(
                "Successfully edited {} (fuzzy match via {replacer_name} replacer)",
                input.path
            )
        };

        Ok(ToolResult {
            output: format!("{output}{}", format_note.unwrap_or_default()),
            success: true,
            file_modified: Some(path),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_edit_file_basic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "code.rs",
                "old_string": "println!(\"hello\")",
                "new_string": "println!(\"world\")"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let content = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert!(content.contains("println!(\"world\")"));
        assert!(!content.contains("println!(\"hello\")"));
    }

    #[tokio::test]
    async fn test_edit_file_string_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "some content").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "file.txt",
                "old_string": "nonexistent string",
                "new_string": "replacement"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("String not found"));
    }

    #[tokio::test]
    async fn test_edit_file_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dup.txt"), "foo bar foo baz foo").unwrap();

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "path": "dup.txt",
                "old_string": "foo",
                "new_string": "qux"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("3 occurrences"));
    }

    #[tokio::test]
    async fn test_edit_file_traversal_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EditFileTool::new(dir.path());

        let result = tool
            .execute(&serde_json::json!({
                "path": "../../etc/passwd",
                "old_string": "root",
                "new_string": "hacked"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("outside working directory"));
    }

    // -----------------------------------------------------------------------
    // Phase 2-C: SessionScope integration tests for EditFileTool.
    // -----------------------------------------------------------------------

    use octos_core::SessionScope;
    use std::sync::Arc;

    fn ctx_with_scope(scope: SessionScope) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "edit-with-scope".to_string();
        ctx.session_scope = Some(Arc::new(scope));
        ctx
    }

    #[tokio::test]
    async fn edit_file_refuses_out_of_scope_path() {
        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("target.txt");
        std::fs::write(&outside_file, "untouched\n").unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = EditFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": outside_file.to_string_lossy(),
                    "old_string": "untouched",
                    "new_string": "owned",
                }),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("outside session scope"),
            "expected scope rejection, got: {}",
            result.output
        );
        // File MUST remain unchanged.
        assert_eq!(
            std::fs::read_to_string(&outside_file).unwrap(),
            "untouched\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_refuses_ancestor_symlink_escape() {
        // Symmetric with the write_file ancestor-symlink test. Even
        // with a real target file pre-staged at the symlink target,
        // the scoped resolver must refuse before O_NOFOLLOW would
        // (correctly) bail on the symlink itself.
        use std::os::unix::fs::symlink;

        let scope_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        std::fs::write(outside_dir.path().join("target.txt"), "secret\n").unwrap();
        let link_path = scope_dir.path().join("link");
        symlink(outside_dir.path(), &link_path).unwrap();

        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = EditFileTool::new(scope_dir.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "link/target.txt",
                    "old_string": "secret",
                    "new_string": "owned",
                }),
            )
            .await
            .unwrap();
        assert!(
            !result.success,
            "ancestor-symlink escape MUST be refused, got: {}",
            result.output
        );
        assert!(
            result.output.contains("outside session scope"),
            "expected scope rejection, got: {}",
            result.output
        );
        // Real file at the symlink target must remain unchanged.
        assert_eq!(
            std::fs::read_to_string(outside_dir.path().join("target.txt")).unwrap(),
            "secret\n",
        );
    }

    // -----------------------------------------------------------------------
    // #1771: cascading fuzzy replacer chain.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_match_via_line_trimmed_when_indentation_differs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    if ready {\n        launch();\n    }\n}\n",
        )
        .unwrap();

        let tool = EditFileTool::new(dir.path());
        // LLM lost the indentation (flush left) but every line's trimmed
        // content is right — the line_trimmed replacer must recover it.
        let result = tool
            .execute(&serde_json::json!({
                "path": "code.rs",
                "old_string": "if ready {\nlaunch();\n}",
                "new_string": "if ready {\n        abort();\n    }"
            }))
            .await
            .unwrap();

        assert!(
            result.success,
            "line-trimmed fuzzy match should succeed: {}",
            result.output
        );
        assert!(
            result.output.contains("line_trimmed"),
            "success output must report which replacer matched: {}",
            result.output
        );
        let content = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert!(content.contains("abort();"));
        assert!(!content.contains("launch();"));
    }

    #[tokio::test]
    async fn should_refuse_block_anchor_span_far_exceeding_old_string() {
        let dir = tempfile::tempdir().unwrap();
        let original = "fn f() {\n    a();\n    b();\n    c();\n    d();\n    e();\n    g();\n}\n";
        std::fs::write(dir.path().join("g.rs"), original).unwrap();

        let tool = EditFileTool::new(dir.path());
        // Anchors would bracket 8 lines for a 3-line old_string. The block
        // similarity score counts the six undescribed middle lines against
        // the match, so block_anchor refuses outright (NoMatch) before the
        // disproportionate guard is even consulted.
        let result = tool
            .execute(&serde_json::json!({
                "path": "g.rs",
                "old_string": "fn f() {\n    a();\n}",
                "new_string": "fn f() {}"
            }))
            .await
            .unwrap();

        assert!(
            !result.success,
            "runaway span must be refused: {}",
            result.output
        );
        assert!(
            result.output.contains("String not found"),
            "expected NoMatch refusal, got: {}",
            result.output
        );
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("g.rs")).unwrap(),
            original
        );
    }

    // -----------------------------------------------------------------------
    // #1767: industry-convention parameter aliases.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_edit_file_tool_invalidate_cache_after_edit() {
        use crate::file_state_cache::{CacheEntry, FileStateCache};
        use std::sync::Arc;
        use std::time::SystemTime;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("code.rs");
        std::fs::write(&file_path, "fn foo() {}\n").unwrap();

        let cache = Arc::new(FileStateCache::new());
        cache.put(CacheEntry::new(
            file_path.clone(),
            SystemTime::now(),
            0xCAFE,
            12,
            false,
            None,
        ));
        assert_eq!(cache.len(), 1);

        let mut ctx = ToolContext::zero();
        ctx.file_state_cache = Some(cache.clone());

        let tool = EditFileTool::new(dir.path());
        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({
                    "path": "code.rs",
                    "old_string": "fn foo() {}",
                    "new_string": "fn bar() {}"
                }),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert!(cache.peek(&file_path).is_none());
    }

    // -----------------------------------------------------------------------
    // #1774: post-edit formatting integration.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // #1976: per-path write-grant enforcement.
    // -----------------------------------------------------------------------

    fn fenced_tool(dir: &std::path::Path, patterns: &[&str], create_only: bool) -> EditFileTool {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        EditFileTool::new(dir).with_write_grant(
            crate::tools::write_grant::WritePathGrant::new(&owned, create_only)
                .expect("test grant compiles"),
        )
    }

    #[tokio::test]
    async fn write_grant_create_only_refuses_edit_even_on_allowlisted_path() {
        // Acceptance (#1976): under create_only, edit_file is refused on ANY
        // path — allowlisted or not ("created, never modified").
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exemplar.card"), "v1\n").unwrap();
        let tool = fenced_tool(dir.path(), &["exemplar.card"], true);

        let result = tool
            .execute(&serde_json::json!({
                "path": "exemplar.card",
                "old_string": "v1",
                "new_string": "v2",
            }))
            .await
            .unwrap();
        assert!(!result.success, "create_only must refuse edits");
        assert!(
            result
                .output
                .contains(crate::tools::write_grant::DENIED_MARKER),
            "typed refusal: {}",
            result.output
        );
        assert!(result.output.contains("create-only"), "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("exemplar.card")).unwrap(),
            "v1\n",
            "refused edit must leave the file untouched"
        );
    }
}
