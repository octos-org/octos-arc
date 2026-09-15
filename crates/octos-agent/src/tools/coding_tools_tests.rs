use super::*;
use crate::tools::ToolRegistry;

#[test]
fn normalize_plan_maps_codex_shape_and_assigns_ids() {
    use octos_core::ui_protocol::PlanItemStatus;
    let args = json!({
        "explanation": "Building memory panel…",
        "plan": [
            { "step": "web P3: PWA manifest", "status": "completed" },
            { "step": "memory panel", "status": "in_progress" },
            { "step": "docs review", "status": "pending", "priority": "P3" },
            { "step": "no status → pending" }
        ]
    });
    let record = normalize_plan(&args, 42);
    assert_eq!(record.title.as_deref(), Some("Building memory panel…"));
    assert_eq!(record.updated_at_ms, 42);
    assert_eq!(record.items.len(), 4);
    // 1-based ids assigned when the caller omits them.
    assert_eq!(record.items[0].id, "1");
    assert_eq!(record.items[3].id, "4");
    assert_eq!(record.items[0].status, PlanItemStatus::Completed);
    assert_eq!(record.items[1].status, PlanItemStatus::InProgress);
    assert_eq!(record.items[2].status, PlanItemStatus::Pending);
    assert_eq!(record.items[2].priority.as_deref(), Some("P3"));
    // Unknown/absent status defaults to Pending.
    assert_eq!(record.items[3].status, PlanItemStatus::Pending);
    assert_eq!(record.items[0].title, "web P3: PWA manifest");
}

struct CapturingReporter {
    events: Arc<std::sync::Mutex<Vec<crate::progress::ProgressEvent>>>,
}
impl crate::progress::ProgressReporter for CapturingReporter {
    fn report(&self, event: crate::progress::ProgressEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn update_plan_tool_emits_plan_updated_event() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ctx = ToolContext::zero();
    ctx.reporter = Arc::new(CapturingReporter {
        events: Arc::clone(&events),
    });
    let args = json!({ "plan": [{ "step": "do a thing", "status": "in_progress" }] });

    let result = UpdatePlanTool
        .execute_with_context(&ctx, &args)
        .await
        .expect("update_plan executes");
    assert!(result.success);
    // Back-compat: the legacy structured_metadata path is preserved.
    assert!(result.structured_metadata.is_some());

    let captured = events.lock().unwrap();
    let plan = captured
        .iter()
        .find_map(|e| match e {
            crate::progress::ProgressEvent::PlanUpdated { plan } => Some(plan.clone()),
            _ => None,
        })
        .expect("a PlanUpdated event was emitted");
    assert_eq!(plan["items"][0]["title"], "do a thing");
    assert_eq!(plan["items"][0]["status"], "in_progress");
}

#[test]
fn truncate_capture_does_not_panic_on_multibyte_boundary() {
    // Regression: `guard[len - MAX_CAPTURE_BYTES..]` sliced at a raw byte
    // offset. A multibyte char straddling that offset panicked, silently
    // killing the spawned reader task. Build a buffer whose cut point falls
    // mid-'世' (3 bytes) and confirm the trim succeeds and stays valid UTF-8.
    // An all-3-byte-char buffer: char boundaries are multiples of 3, and
    // MAX_CAPTURE_BYTES (50_000) is not ≡ 0 (mod 3), so the raw cut offset
    // `len - MAX_CAPTURE_BYTES` is guaranteed to fall mid-char.
    let mut guard = "世".repeat(40_000); // 120_000 bytes, over the 2× trigger
    let cut = guard.len().saturating_sub(MAX_CAPTURE_BYTES);
    assert!(
        !guard.is_char_boundary(cut),
        "test precondition: the raw cut offset must fall mid-char"
    );

    truncate_capture_in_place(&mut guard); // must not panic
    assert!(guard.starts_with("... (earlier output truncated)\n"));
    assert!(
        guard.contains('世'),
        "the kept tail must remain valid UTF-8"
    );
}

const CODEX_P0: &[&str] = &[
    "apply_patch",
    "exec_command",
    "write_stdin",
    "update_plan",
    "spawn_agent",
    "send_input",
    "resume_agent",
    "wait_agent",
    "close_agent",
];

#[test]
fn builtins_expose_codex_p0_tool_names() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let names: std::collections::HashSet<_> =
        registry.specs().into_iter().map(|spec| spec.name).collect();
    for name in CODEX_P0 {
        assert!(names.contains(*name), "{name} should be model-visible");
    }
}
struct FakeSpawnTool;

#[async_trait::async_trait]
impl Tool for FakeSpawnTool {
    fn name(&self) -> &str {
        "spawn"
    }

    fn description(&self) -> &str {
        "fake spawn"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let supervisor = ctx.task_supervisor.as_ref().expect("supervisor");
        let task_id = supervisor.register_with_input(
            "spawn",
            "fake-call",
            ctx.parent_session_key.as_deref(),
            Some(args.clone()),
        );
        supervisor.mark_running(&task_id);
        Ok(ToolResult {
            output: "spawned fake worker".to_string(),
            success: true,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn spawn_agent_delegates_to_registered_spawn_tool() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "inspect parity",
                "agent_type": "worker",
                "reasoning_effort": "high"
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    let input = task.tool_input.expect("tool input");
    assert_eq!(input["task"], "inspect parity");
    assert_eq!(input["label"], "codex-worker");
    assert_eq!(input["role"], crate::ROLE_IMPLEMENTER);
    assert!(
        input["additional_instructions"]
            .as_str()
            .unwrap()
            .contains("reasoning_effort: high")
    );
}

/// Issue #971 (M14-C wiring contract): an unknown `role` value MUST
/// fail at the tool boundary with a structured error rather than
/// silently defaulting to a template the LLM did not ask for. The
/// `TaskListEntry.role` field is `Option<String>` precisely because
/// the caller is expected to handle the unknown case explicitly.
#[tokio::test]
async fn spawn_agent_rejects_unknown_role_per_971() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "audit PR #1234",
                "role": "review",
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(
        !result.success,
        "spawn_agent must refuse unknown role; output={:?}",
        result.output
    );
    assert!(
        result.output.contains("unknown role")
            || (result.output.contains("role") && result.output.contains("review")),
        "error must mention the offending role name; got {:?}",
        result.output
    );
}

#[tokio::test]
async fn codex_agent_aliases_operate_on_supervisor_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let supervisor = registry.supervisor();
    let agent_id = supervisor.register_with_input(
        "spawn",
        "call-alias",
        Some("api:alias-test"),
        Some(json!({ "task": "initial" })),
    );
    supervisor.mark_running(&agent_id);
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:alias-test".to_owned()),
        ..ToolContext::zero()
    };

    let sent = registry
        .execute_with_context(
            &ctx,
            "send_input",
            &json!({
                "agent_id": agent_id.clone(),
                "message": "continue with reviewer notes"
            }),
        )
        .await
        .expect("send_input");
    assert!(sent.success, "{}", sent.output);
    let updated = supervisor.get_task(&agent_id).expect("task");
    let tool_input = updated.tool_input.expect("tool input");
    assert_eq!(
        tool_input["last_codex_send_input"]["request"]["message"],
        "continue with reviewer notes"
    );

    let waited = registry
        .execute_with_context(
            &ctx,
            "wait_agent",
            &json!({ "agent_id": agent_id.clone(), "timeout_ms": 0 }),
        )
        .await
        .expect("wait_agent");
    assert!(waited.success, "{}", waited.output);
    let waited_payload: Value = serde_json::from_str(&waited.output).expect("wait json");
    assert_eq!(waited_payload["agents"][0]["agent_id"], agent_id);
    assert_eq!(waited_payload["agents"][0]["status"], "running");

    let closed = registry
        .execute_with_context(&ctx, "close_agent", &json!({ "target": agent_id.clone() }))
        .await
        .expect("close_agent");
    assert!(closed.success, "{}", closed.output);
    let closed_task = supervisor.get_task(&agent_id).expect("task");
    assert_eq!(closed_task.status, crate::TaskStatus::Cancelled);
}

// NOTE (#1773): `apply_patch_adds_and_updates_file` moved to
// `crate::tools::apply_patch::tests` alongside the relocated tool.

#[tokio::test]
async fn exec_command_runs_to_completion() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let result = registry
        .execute("exec_command", &json!({"cmd": "printf codex"}))
        .await
        .expect("exec command");
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("codex"));
}

/// #2128 acceptance: execute a command DENIED by a real sandbox and assert
/// the tool response carries the [sandbox] explanation (macOS only — needs
/// a live seatbelt profile).
#[cfg(target_os = "macos")]
#[tokio::test]
async fn denied_command_response_carries_sandbox_hint() {
    use crate::sandbox::{SandboxConfig, create_sandbox};
    let temp = tempfile::tempdir().expect("tempdir");
    // Real seatbelt sandbox, workspace = temp; writing OUTSIDE it is denied.
    let sandbox = create_sandbox(&SandboxConfig::default());
    let registry = ToolRegistry::with_builtins_and_sandbox(temp.path(), sandbox);
    // Target a path guaranteed outside the workspace and not otherwise
    // writable; the shell's own error carries the kernel EPERM phrase.
    let result = registry
        .execute(
            "exec_command",
            &json!({ "cmd": "echo x > /etc/octos_denied_probe" }),
        )
        .await
        .expect("exec command");
    assert!(
        result.output.contains("[sandbox]"),
        "denied command must surface the sandbox hint, got: {}",
        result.output
    );
}

// -----------------------------------------------------------------------
// #972 / M14-B P1 tests — `view_image`, `tool_search`, `tool_suggest`.
// -----------------------------------------------------------------------

/// 8-byte PNG header (the only part the format detector cares about) plus
/// a zero-IHDR-length marker; enough to make `view_image` happy without
/// pulling in the `image` crate.
const PNG_MAGIC: [u8; 12] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];

#[tokio::test]
async fn view_image_reports_format_and_size_for_png() {
    let temp = tempfile::tempdir().expect("tempdir");
    let png = temp.path().join("logo.png");
    std::fs::write(&png, PNG_MAGIC).expect("write png");
    let tool = ViewImageTool::new(temp.path());
    let result = tool
        .execute(&json!({ "path": "logo.png" }))
        .await
        .expect("view_image ok");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    assert_eq!(payload["format"], json!("png"));
    assert_eq!(payload["mime_type"], json!("image/png"));
    assert_eq!(payload["byte_length"], json!(PNG_MAGIC.len()));
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["codex_tool"], json!("view_image"));
    assert_eq!(meta["format"], json!("png"));
}

#[tokio::test]
async fn view_image_fails_when_path_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let tool = ViewImageTool::new(temp.path());
    let result = tool
        .execute(&json!({ "path": "absent.png" }))
        .await
        .expect("view_image runs");
    assert!(!result.success);
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["codex_tool"], json!("view_image"));
    assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
}

/// #1148 codex P2 acceptance: view_image MUST refuse to follow
/// symlinks (Unix O_NOFOLLOW) so a malicious repo can't trick
/// the tool into reading a file outside the workspace via a
/// symlinked image entry.
#[cfg(unix)]
#[tokio::test]
async fn view_image_rejects_symlinked_target() {
    const PNG_MAGIC: [u8; 12] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("real_image.png");
    std::fs::write(&target, PNG_MAGIC).expect("write png");
    let symlink = temp.path().join("link.png");
    std::os::unix::fs::symlink(&target, &symlink).expect("symlink");

    let tool = ViewImageTool::new(temp.path());
    let result = tool
        .execute(&json!({ "path": "link.png" }))
        .await
        .expect("view_image runs");
    assert!(
        !result.success,
        "view_image must reject symlinked targets (O_NOFOLLOW); got success result"
    );
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
}

fn sample_catalog_cell() -> Arc<std::sync::Mutex<Vec<ToolCatalogEntry>>> {
    Arc::new(std::sync::Mutex::new(sample_catalog()))
}

fn sample_catalog() -> Vec<ToolCatalogEntry> {
    vec![
        ToolCatalogEntry::new(
            "apply_patch",
            "Apply a Codex-style patch to files in the workspace",
            vec!["fs".to_string(), "code".to_string()],
        ),
        ToolCatalogEntry::new(
            "exec_command",
            "Run a shell command. Supports long-running sessions.",
            vec!["runtime".to_string(), "code".to_string()],
        ),
        ToolCatalogEntry::new(
            "update_plan",
            "Update the visible task plan",
            vec!["code".to_string()],
        ),
        ToolCatalogEntry::new(
            "web_search",
            "Search the web for an arbitrary query",
            vec!["search".to_string(), "web".to_string()],
        ),
    ]
}

#[tokio::test]
async fn tool_search_returns_matching_tools_for_substring() {
    let tool = ToolSearchTool::new(sample_catalog_cell());
    let result = tool
        .execute(&json!({ "query": "patch" }))
        .await
        .expect("tool_search ok");
    assert!(result.success);
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    let matches = payload["matches"].as_array().expect("matches");
    assert!(!matches.is_empty(), "expected at least one match");
    assert_eq!(matches[0]["name"], json!("apply_patch"));
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["codex_tool"], json!("tool_search"));
}

/// #1172 acceptance: `tool_search("bash")` must surface the new
/// alias on first call. Without the alias the canonical `shell`
/// would dominate even though a Codex-trained model is emitting
/// `bash(...)`.
#[tokio::test]
async fn tool_search_returns_bash_alias() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let search = registry
        .get_tool("tool_search")
        .expect("tool_search registered");
    let result = search
        .execute(&json!({ "query": "bash" }))
        .await
        .expect("tool_search ok");
    assert!(result.success);
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    let matches = payload["matches"].as_array().expect("matches");
    let names: Vec<&str> = matches.iter().filter_map(|m| m["name"].as_str()).collect();
    assert!(
        names.contains(&"bash"),
        "tool_search must surface bash alias; got {names:?}"
    );
}

/// #1172 happy path: `bash` runs a simple command to completion
/// and returns the captured stdout.
#[tokio::test]
async fn bash_runs_simple_command() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let result = registry
        .execute("bash", &json!({ "cmd": "printf hello-bash" }))
        .await
        .expect("bash runs");
    assert!(result.success, "{}", result.output);
    assert!(
        result.output.contains("hello-bash"),
        "bash output must contain captured stdout; got: {}",
        result.output
    );
    let meta = result
        .structured_metadata
        .as_ref()
        .expect("bash must emit structured metadata");
    assert_eq!(meta["codex_tool"], json!("bash"));
}

/// #1172 denial path: dangerous commands are rejected by the
/// shared `SafePolicy`, the same gate `shell` and `exec_command`
/// use. The error path returns `success=false` with a denial
/// message — no command is spawned.
#[tokio::test]
async fn bash_denies_dangerous_command_via_safe_policy() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let result = registry
        .execute("bash", &json!({ "cmd": "rm -rf /" }))
        .await
        .expect("bash runs");
    assert!(
        !result.success,
        "bash must reject `rm -rf /` via SafePolicy; got: {}",
        result.output
    );
    assert!(
        result.output.to_lowercase().contains("denied")
            || result.output.to_lowercase().contains("approval"),
        "bash denial message must be readable; got: {}",
        result.output
    );
}

/// #1172 happy path: `delegate(role, task)` spawns a child task
/// through the registered spawn delegate, waits for it to reach a
/// terminal state, and surfaces the artifacts list. We background
/// a completer that flips the supervisor's task to `Completed` so
/// the wait loop exits with a terminal payload instead of a
/// timeout.
#[tokio::test]
async fn delegate_spawns_waits_and_returns_artifacts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    // Spawn a background completer that polls the supervisor and
    // flips the first new task to Completed so the wait loop
    // terminates instead of timing out. Uses an unbounded sleep
    // budget but the outer 5s `timeout_ms` is the hard cap.
    let supervisor_for_completer = supervisor.clone();
    let completer = tokio::spawn(async move {
        for _ in 0..200 {
            if let Some(task) = supervisor_for_completer
                .get_all_tasks()
                .into_iter()
                .find(|task| task.tool_name == "spawn" || task.tool_name == "spawn_agent")
            {
                supervisor_for_completer
                    .mark_completed(&task.id, vec!["delegate-output.txt".to_string()]);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    });
    let result = registry
        .execute_with_context(
            &ctx,
            "delegate",
            &json!({
                "role": "reviewer",
                "task": "review the diff for unsafe regressions",
                "timeout_ms": 5_000,
            }),
        )
        .await
        .expect("delegate runs");
    let _ = completer.await;
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    assert_eq!(payload["role"], json!("reviewer"));
    assert!(payload["agent_id"].is_string(), "{payload}");
    assert!(
        payload["terminal"].as_bool().unwrap_or(false),
        "delegate must report terminal=true once the child completes: {payload}"
    );
    assert_eq!(payload["status"], json!("completed"));
    assert!(
        payload["artifacts"].is_array(),
        "artifacts must be an array even when empty"
    );
    let meta = result
        .structured_metadata
        .expect("delegate must emit structured metadata");
    assert_eq!(meta["codex_tool"], json!("delegate"));
    assert_eq!(meta["role"], json!("reviewer"));
}

/// #1172 denial path: unknown role names must be rejected at the
/// tool boundary so a typo (`"review"` vs `"reviewer"`) surfaces
/// immediately instead of silently smuggling an unbounded prompt.
#[tokio::test]
async fn delegate_rejects_unknown_role() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let result = registry
        .execute(
            "delegate",
            &json!({ "role": "review", "task": "do a review" }),
        )
        .await
        .expect("delegate runs");
    assert!(
        !result.success,
        "delegate must reject `review` (canonical is `reviewer`); got: {}",
        result.output
    );
    let meta = result
        .structured_metadata
        .as_ref()
        .expect("delegate must emit denial metadata");
    assert_eq!(meta["error_kind"], json!("coding_tool_denied"));
    assert_eq!(meta["reason"], json!("unknown_role"));
    assert_eq!(meta["role"], json!("review"));
}

/// #1172 codex review P1: `bash` must be covered by the
/// `group:runtime` tool policy group so a profile denying runtime
/// commands cannot be bypassed via the Codex naming alias.
/// `delegate` likewise must be covered by `group:sessions` so a
/// policy denying subagent spawn cannot be bypassed via the
/// one-call wrapper.
#[test]
fn codex_naming_aliases_are_covered_by_policy_groups() {
    use crate::tools::policy::tool_group_info;
    let runtime = tool_group_info("group:runtime").expect("group:runtime registered");
    assert!(
        runtime.tools.contains(&"bash"),
        "group:runtime must include `bash` so the alias respects \
             runtime-denying policies: {tools:?}",
        tools = runtime.tools,
    );
    let sessions = tool_group_info("group:sessions").expect("group:sessions registered");
    assert!(
        sessions.tools.contains(&"delegate"),
        "group:sessions must include `delegate` so the alias respects \
             session-denying policies: {tools:?}",
        tools = sessions.tools,
    );
}

/// #1172 codex review P2 acceptance (follow-up): a `bash` command
/// that backgrounds a grandchild and `wait`s on it must still have
/// the grandchild killed when the bash timeout fires. Without
/// `process_group(0)` before spawn, the negative-PID kill targets
/// a process group that the child was never put in, so the
/// backgrounded `sleep` survives the timeout and the workspace
/// mutation happens after the tool reports failure.
#[cfg(unix)]
#[tokio::test]
async fn bash_kills_grandchildren_via_process_group_on_timeout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sentinel = temp.path().join("grandchild-late.txt");
    let sentinel_path = sentinel.to_string_lossy().into_owned();
    // Backgrounded grandchild that touches the sentinel after a sleep
    // longer than the timeout. The `wait` keeps the outer bash alive
    // so the timeout path is forced to walk the process group.
    let cmd = format!("(sleep 3; touch {sentinel_path}) & wait");
    let registry = ToolRegistry::with_builtins(temp.path());
    let started = std::time::Instant::now();
    let result = registry
        .execute("bash", &json!({ "cmd": cmd, "timeout_ms": 1_000 }))
        .await
        .expect("bash runs");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "bash must return within the timeout window (got {:?})",
        started.elapsed()
    );
    assert!(!result.success, "{}", result.output);
    // Wait past when the orphaned grandchild's `touch` would fire.
    tokio::time::sleep(std::time::Duration::from_millis(4_000)).await;
    assert!(
        !sentinel.exists(),
        "grandchild process must be killed via the bash process group on \
             timeout — sentinel at {} should NOT exist (negative-PID kill \
             didn't reach the grandchild)",
        sentinel.display(),
    );
}

/// #1172 codex review P2 acceptance: when a `bash` command exceeds
/// `timeout_ms`, the child process must be killed instead of left
/// alive in the background. We start a child that touches a sentinel
/// file after a sleep that's longer than the timeout. If the kill
/// fires correctly the sentinel never appears; if the child is
/// orphaned it will appear after the timeout returns to the caller.
#[cfg(unix)]
#[tokio::test]
async fn bash_kills_child_process_on_timeout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sentinel = temp.path().join("late-write.txt");
    let sentinel_path = sentinel.to_string_lossy().into_owned();
    // sleep 3 seconds then touch the sentinel; bash timeout fires
    // at ~1s so the touch must NOT execute if the kill works.
    let cmd = format!("sleep 3; touch {sentinel_path}");
    let registry = ToolRegistry::with_builtins(temp.path());
    let started = std::time::Instant::now();
    let result = registry
        .execute("bash", &json!({ "cmd": cmd, "timeout_ms": 1_000 }))
        .await
        .expect("bash runs");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "bash must return within the timeout window (got {:?})",
        started.elapsed()
    );
    assert!(!result.success, "{}", result.output);
    assert!(
        result.output.contains("timed out"),
        "bash must report timeout in the output; got: {}",
        result.output,
    );
    // Wait past when the orphaned `touch` would have fired had the
    // kill failed (3s sleep + 1s slack).
    tokio::time::sleep(std::time::Duration::from_millis(4_000)).await;
    assert!(
        !sentinel.exists(),
        "child process must be killed on timeout — sentinel file at {} should NOT exist",
        sentinel.display(),
    );
}

// ---------------------------------------------------------------------------
// Cancellation must not orphan a child's process group.
//
// The kill ladder only runs on the TIMEOUT arm. A user interrupt drops the
// whole future instead (`agent_task.abort()`), and `tokio::process::Child` does
// not kill on drop — so before `ChildGroupGuard` an Esc'd `bash("npm run dev")`
// kept running, holding ports and able to write to the workspace.
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod cancellation_kills_child_group {
    use super::*;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    /// Spawn a long-lived child in its OWN process group, exactly as the tools
    /// do, and hand back its pid.
    fn spawn_group_leader() -> (tokio::process::Child, u32) {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 300")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = cmd.spawn().expect("spawn sleeper");
        let pid = child.id().expect("child pid");
        (child, pid)
    }

    /// `kill -0` the group until it disappears, or give up.
    ///
    /// Deliberately `async` with `tokio::time::sleep`: the guard hands its kill
    /// ladder to a spawned task, and a blocking `std::thread::sleep` here would
    /// starve the current-thread test runtime so that task never runs — the
    /// test would fail against a working fix.
    async fn wait_until_group_gone(pid: u32, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if !process_group_exists(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        !process_group_exists(pid)
    }

    #[tokio::test]
    async fn dropping_an_armed_guard_kills_the_process_group() {
        let (child, pid) = spawn_group_leader();
        assert!(process_group_exists(pid), "sleeper should be running");

        {
            let _guard = ChildGroupGuard::new(Some(pid));
            drop(child); // tokio does NOT kill on drop — the guard must.
        }

        assert!(
            wait_until_group_gone(pid, Duration::from_secs(5)).await,
            "process group {pid} survived a dropped guard — an interrupted \
             command would keep running and mutating the workspace"
        );
    }
}

// ---------------------------------------------------------------------------
// #28c — file-change receipt on the CODING-session bash path (BashTool),
// reusing the shared 28a module. These tests pin the 28a acceptance set
// on this link: real edit ⇒ receipt; phantom edit ⇒ 0; non-git ⇒ omitted;
// default (no knob involvement here) ⇒ unchanged when nothing changed.
// ---------------------------------------------------------------------------
mod bash_change_receipt_28c {
    use super::*;
    use crate::policy::AllowAllPolicy;
    use crate::tools::coding_tools::BashTool;
    use std::sync::Arc;

    fn tool(dir: &std::path::Path) -> BashTool {
        BashTool::new(dir, Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(AllowAllPolicy))
    }

    #[cfg(unix)]
    // #34d: POSIX shell spawn semantics (echo > file, cd &&) — repo convention: gate like bash_kills_grandchildren_via_process_group_on_timeout; the pure-function receipts (diff_to_receipt unit tests in shell.rs) stay ungated.
    #[tokio::test]
    async fn real_edit_in_git_repo_appends_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(cwd)
            .status()
            .expect("git init");
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(cwd)
            .status()
            .expect("git commit");
        let target = cwd.join("receipt-28c.txt");
        let out = tool(cwd)
            .execute(&json!({
                "cmd": format!("echo real-edit > {:?}", target),
            }))
            .await
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            out.output.contains("files_changed: 1"),
            "receipt missing on the coding bash path: {}",
            out.output
        );
    }

    #[cfg(unix)]
    // #34d: POSIX shell spawn semantics (echo > file, cd &&) — repo convention: gate like bash_kills_grandchildren_via_process_group_on_timeout; the pure-function receipts (diff_to_receipt unit tests in shell.rs) stay ungated.
    #[tokio::test]
    async fn non_git_dir_omits_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path(); // never git-init'd
        let target = cwd.join("plain.txt");
        let out = tool(cwd)
            .execute(&json!({
                "cmd": format!("echo plain > {:?}", target),
            }))
            .await
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            !out.output.contains("files_changed"),
            "non-git fail-open must omit the receipt: {}",
            out.output
        );
    }
}

mod exec_change_receipt_28c {
    use super::*;
    use crate::policy::AllowAllPolicy;
    use crate::tools::coding_tools::ExecCommandTool;
    use std::sync::Arc;

    fn tool(dir: &std::path::Path) -> ExecCommandTool {
        ExecCommandTool::new(dir, Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(AllowAllPolicy))
    }

    #[cfg(unix)] // #34d: POSIX shell spawn semantics — repo convention (see bash_change_receipt_28c).
    #[tokio::test]
    async fn real_edit_in_git_repo_appends_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(cwd)
            .status()
            .expect("git init");
        let target = cwd.join("exec-receipt-28c.txt");
        let out = tool(cwd)
            .execute(&json!({
                "command": format!("echo real-edit > {:?}", target),
            }))
            .await
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            out.output.contains("files_changed: 1"),
            "receipt missing on the exec_command path: {}",
            out.output
        );
    }

    #[cfg(unix)] // #34d: POSIX shell spawn semantics — repo convention (see bash_change_receipt_28c).
    #[tokio::test]
    async fn non_git_dir_omits_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        let target = cwd.join("plain.txt");
        let out = tool(cwd)
            .execute(&json!({
                "command": format!("echo plain > {:?}", target),
            }))
            .await
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            !out.output.contains("files_changed"),
            "non-git fail-open must omit the receipt: {}",
            out.output
        );
    }
}

mod receipt_scope_root_28c_r1 {

    use crate::tools::coding_tools::receipt_scope_root;

    // Resolver unit pins (ruling ②): literal cd prefix wins; ambiguous
    // shapes fall back to workdir.
    // #34e — portable fixtures: the RESOLVER is pure string logic (no POSIX
    // dependency), but absolute-path literals like "/tmp/x" are NOT absolute
    // on Windows (no drive prefix), so `is_absolute()` flips the join arm
    // and the scope assertion fails. Building the literals from a platform
    // absolute base keeps this test covering the judgment on BOTH platforms
    // (per the 34e ruling's first option) instead of gating it off.
    #[test]
    fn resolver_literal_cd_prefix_wins_and_var_falls_back() {
        // #34f — the fixture text and the expected path must be built from
        // the SAME string: on Windows `PathBuf::display()` renders
        // backslashes, the resolver's literal screen rejects `\` (falls
        // back to workdir), and the assertion pairs then cross (34e's own
        // mistake — expected Temp\ws, got Temp\tmp\x). Building both the
        // command text and the expectation from one forward-slash string
        // keeps the pairs aligned on every platform; `Path::new` on a
        // forward-slash string is still absolute on Windows (drive prefix).
        let base = std::env::temp_dir();
        let ws = base.join("ws");
        let base_text = base.to_string_lossy().replace('\\', "/");
        let target_text = format!("{base_text}/tmp/x");
        let cd_cmd = format!("cd {target_text} && echo hi > f");
        let (root, scope) = receipt_scope_root(&ws, &cd_cmd);
        assert_eq!(root, std::path::PathBuf::from(&target_text));
        assert_eq!(scope, "cd-target");

        let (root, scope) = receipt_scope_root(&ws, "cd ~/proj && echo hi > f");
        assert_eq!(scope, "cd-target");
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .expect("HOME/USERPROFILE set");
        assert!(
            root.starts_with(std::path::Path::new(&home)),
            "~ expansion anchors at the platform home: {root:?}"
        );

        // No cd prefix ⇒ workdir.
        let (root, scope) = receipt_scope_root(&ws, "echo hi > f");
        assert_eq!(root, ws.clone());
        assert_eq!(scope, "workdir");

        // Variable path ⇒ ambiguous ⇒ workdir.
        let (root, scope) = receipt_scope_root(&ws, "cd $TARGET && echo hi > f");
        assert_eq!(root, ws.clone());
        assert_eq!(scope, "workdir");

        // cd without && ⇒ workdir.
        let (_root, scope) = receipt_scope_root(&ws, "cd /tmp/x");
        assert_eq!(scope, "workdir");

        // Semicolon chain ⇒ ambiguous ⇒ workdir.
        let (root, scope) = receipt_scope_root(&ws, "cd /tmp/x; cd /tmp/y && echo hi > f");
        assert_eq!(scope, "workdir");
        assert_eq!(root, ws.clone());
    }
}

mod bash_file_writes_28d {
    use super::*;
    use crate::policy::AllowAllPolicy;
    use crate::tools::coding_tools::{BashTool, ExecCommandTool};
    use crate::tools::policy::BashFileWrites;
    use std::sync::Arc;

    fn bash(dir: &std::path::Path, mode: BashFileWrites) -> BashTool {
        BashTool::new(dir, Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(AllowAllPolicy))
            .with_bash_file_writes(mode)
    }

    fn exec(dir: &std::path::Path, mode: BashFileWrites) -> ExecCommandTool {
        ExecCommandTool::new(dir, Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(AllowAllPolicy))
            .with_bash_file_writes(mode)
    }

    // deny: write-shaped command refused, escape hatch honored — on BOTH
    // coding tools.
    #[cfg(unix)]
    // #34d: POSIX shell spawn semantics (echo > file, cd &&) — repo convention: gate like bash_kills_grandchildren_via_process_group_on_timeout; the pure-function receipts (diff_to_receipt unit tests in shell.rs) stay ungated.
    #[tokio::test]
    async fn deny_refuses_write_and_escape_hatch_runs_bash_and_exec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = bash(dir.path(), BashFileWrites::Deny)
            .execute(&json!({ "cmd": "echo x > /tmp/never-28d" }))
            .await
            .expect("execute");
        assert!(!out.success);
        assert!(out.output.contains("bash_file_writes=deny"));
        // Refusal text only — the command must not have run.
        let _ = std::path::Path::new("/tmp/never-28d");

        let out = exec(dir.path(), BashFileWrites::Deny)
            .execute(&json!({ "command": "echo x > /tmp/never-28d-e" }))
            .await
            .expect("execute");
        assert!(!out.success, "exec deny: {}", out.output);
        assert!(out.output.contains("bash_file_writes=deny"));

        // Escape hatch: trailing `# octos:allow-write` runs the write.
        let hatch = dir.path().join("hatch.txt");
        let out = bash(dir.path(), BashFileWrites::Deny)
            .execute(&json!({ "cmd": format!("echo h > {:?} # octos:allow-write", hatch) }))
            .await
            .expect("execute");
        assert!(out.success, "hatch: {}", out.output);
        assert!(hatch.exists());
    }

    // allow (default): zero difference — no policy text anywhere.
    #[cfg(unix)]
    // #34d: POSIX shell spawn semantics (echo > file, cd &&) — repo convention: gate like bash_kills_grandchildren_via_process_group_on_timeout; the pure-function receipts (diff_to_receipt unit tests in shell.rs) stay ungated.
    #[tokio::test]
    async fn allow_is_zero_difference_on_both_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        for out in [
            bash(dir.path(), BashFileWrites::default())
                .execute(&json!({ "cmd": "echo zd" }))
                .await
                .expect("bash"),
            exec(dir.path(), BashFileWrites::default())
                .execute(&json!({ "command": "echo zd" }))
                .await
                .expect("exec"),
        ] {
            assert!(out.success);
            assert!(out.output.contains("zd"));
            assert!(!out.output.contains("bash_file_writes"));
            assert!(!out.output.contains("edit_file / diff_edit"));
        }
    }
}
