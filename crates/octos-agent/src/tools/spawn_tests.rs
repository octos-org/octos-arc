use super::*;
#[cfg(unix)]
use crate::{HookConfig, HookEvent};

/// Runaway guard for the "poll until the background task reaches state X"
/// loops below. It is NOT a latency assertion: on a passing run the loop
/// returns as soon as the state arrives, so a generous ceiling costs nothing,
/// and on a genuinely broken run the test still fails — just later.
///
/// It used to be 5s, which sits under the noise floor of a loaded CI runner:
/// `test_background_spawn_fails_when_contract_owned_workflow_is_not_ready`
/// failed with "did not fail in time" on ubuntu-latest in run 30387949499
/// while the only change under test was one line of `.github/workflows/ci.yml`.
/// These loops spawn real subprocesses, so their wall-clock cost tracks host
/// load rather than anything the test controls.
///
/// #2053: the Windows runners miss fixed-duration waits that pass everywhere
/// else — `test_background_spawn_persists_workflow_phase_transitions` failed
/// this ceiling on `check-windows` while an in-job retry of the SAME job
/// produced a different failure set, and a plain re-run went green. These
/// loops break on content the instant it appears, so a larger Windows ceiling
/// costs a passing run nothing.
#[cfg(not(windows))]
const BACKGROUND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(windows)]
const BACKGROUND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(240);

#[test]
fn frame_subagent_task_leads_with_identity_and_directive() {
    let out = frame_subagent_task("review-octos-web", "Clone and review the repo.");
    assert!(out.starts_with("You are a delegated SUB-AGENT named \"review-octos-web\""));
    assert!(out.contains("do NOT respond to it"));
    // The real task is present and clearly delimited AFTER the framing.
    let task_pos = out.find("=== YOUR TASK ===").expect("task delimiter");
    assert!(out[task_pos..].contains("Clone and review the repo."));
}

#[test]
fn role_task_warning_fires_for_readonly_role_with_clone_and_write_task() {
    // The mini4 reviewer case: read-only allow-list + "clone and write".
    let reviewer = [
        "read_file".to_string(),
        "group:search".to_string(),
        "web_fetch".to_string(),
    ];
    let note = role_task_capability_warning(
        &reviewer,
        "Clone the repo and write a review to octos-web-review.md",
    )
    .expect("mismatch must warn");
    assert!(
        note.contains("git clone"),
        "flags the missing shell: {note}"
    );
    assert!(
        note.contains("write_file"),
        "flags the missing writer: {note}"
    );
    assert!(note.contains("FINAL TEXT ANSWER"));
}

#[test]
fn derive_deliverable_filename_matches_the_declared_glob() {
    // The single-* review glob → slug from label's first word.
    assert_eq!(
        derive_deliverable_filename("*-review.md", "octos-web review"),
        "octos-web-review.md"
    );
    assert_eq!(
        derive_deliverable_filename("*.md", "octos-one review"),
        "octos-one.md"
    );
    // Literal filename → verbatim.
    assert_eq!(
        derive_deliverable_filename("report.md", "anything"),
        "report.md"
    );
    // Odd/multi-* glob → sensible fallback that matches *-review.md / *.md.
    let fb = derive_deliverable_filename("**/*.md", "octos-web review");
    assert_eq!(fb, "octos-web-review.md");
    // Non-alnum label sanitized; empty → output.
    assert_eq!(
        derive_deliverable_filename("*-review.md", "  "),
        "output-review.md"
    );
}

#[tokio::test]
async fn background_deliverable_auto_materializes_inline_final_output() {
    // Live-soak fix: a child that declared a deliverable but returned its
    // work as FINAL TEXT (no file) must have that text written to the
    // deliverable path so it surfaces in output_files instead of being
    // lost. Uses a mock provider that ends with a long text answer and
    // never writes a file.
    struct InlineReviewProvider;
    #[async_trait]
    impl LlmProvider for InlineReviewProvider {
        async fn chat(
            &self,
            _m: &[octos_core::Message],
            _t: &[octos_llm::ToolSpec],
            _c: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            Ok(octos_llm::ChatResponse {
                content: Some(format!(
                    "# Code Review\n\n{}",
                    "detailed finding. ".repeat(60)
                )),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
        fn model_id(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let tool = SpawnTool::new(
        Arc::new(InlineReviewProvider),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone())
    .with_sandbox(SandboxConfig {
        mode: crate::sandbox::SandboxMode::None,
        ..Default::default()
    });

    let result = tool
        .execute(&serde_json::json!({
            "task": "review the repo and write octos-web-review.md",
            "label": "octos-web review",
            "mode": "background",
            "allowed_tools": ["read_file"],
            "deliverable": "*-review.md"
        }))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);

    let started = std::time::Instant::now();
    let task = loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(t) = tasks.first() {
            if t.status == crate::task_supervisor::TaskStatus::Completed {
                break t.clone();
            }
            if t.status == crate::task_supervisor::TaskStatus::Failed {
                panic!("spawn failed: {:?}", t.error);
            }
        }
        if started.elapsed() >= BACKGROUND_DEADLINE {
            let tasks = supervisor.get_tasks_for_session("api:test-session");
            panic!(
                "did not complete in {BACKGROUND_DEADLINE:?}; tasks = {:?}",
                tasks
                    .iter()
                    .map(|t| (t.status.as_str(), t.error.clone()))
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(
        task.output_files.len(),
        1,
        "inline review must be auto-materialized into a deliverable file: {:?}",
        task.output_files
    );
    assert!(task.output_files[0].ends_with("octos-web-review.md"));
    let written = std::fs::read_to_string(&task.output_files[0]).unwrap();
    assert!(written.contains("# Code Review"));
}

#[test]
fn role_template_selection_applies_prompt_and_tool_budget() {
    let mut input: Input = serde_json::from_value(serde_json::json!({
        "task": "review this diff",
        "role": crate::ROLE_REVIEWER,
        "additional_instructions": "Focus on API behavior."
    }))
    .expect("input parses");

    let template = apply_role_template(&mut input)
        .expect("role template resolves")
        .expect("template");

    assert_eq!(template.name, crate::ROLE_REVIEWER);
    assert_eq!(
        input.allowed_tools,
        template.allowed_tools_vec(),
        "empty allowed_tools should resolve from the backend-owned role template"
    );
    let instructions = input
        .additional_instructions
        .as_deref()
        .expect("instructions");
    assert!(
        instructions.starts_with(template.prompt_prefix),
        "role prompt prefix must be prepended by the runtime factory"
    );
    assert!(instructions.contains("Focus on API behavior."));
}

#[cfg(unix)]
fn rewrite_output_files_hook(replacement_path: &std::path::Path) -> HookConfig {
    HookConfig {
        event: HookEvent::BeforeSpawnVerify,
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"cat >/dev/null; printf '{"output_files":["%s"]}\n' "$1"; exit 2"#.into(),
            "sh".into(),
            replacement_path.to_string_lossy().into_owned(),
        ],
        timeout_ms: 5000,
        tool_filter: vec![],
        path_filter: vec![],
        requires_bin: None,
    }
}

/// #1607 (codex-review follow-up): the spawn/agent_mcp child completion
/// path builds its validator registries with
/// `ToolRegistry::with_builtins_and_sandbox(&self.working_dir,
/// create_sandbox(&self.sandbox))`. This test locks in that the sandbox
/// threaded via `with_sandbox` actually reaches that registry (i.e. the
/// two construction sites are NOT the pre-fix hardcoded `with_builtins` /
/// `NoSandbox`). Docker mode is chosen because `create_sandbox` returns a
/// `DockerSandbox` unconditionally (no docker binary required), so the
/// assertion is host-independent: a hardcoded `NoSandbox` would report
/// `is_noop() == true` / `is_docker() == false`, which would fail here.
#[tokio::test]
async fn spawn_threads_configured_sandbox_into_validator_registry() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_sandbox(SandboxConfig {
        mode: crate::sandbox::SandboxMode::Docker,
        ..SandboxConfig::default()
    });

    // Reconstruct exactly what the two `execute_with_context` validator
    // blocks build (`with_builtins_and_sandbox(&self.working_dir,
    // create_sandbox(&self.sandbox))`) and assert the backend is the one
    // we configured, not a hardcoded no-op.
    let registry =
        ToolRegistry::with_builtins_and_sandbox(&tool.working_dir, create_sandbox(&tool.sandbox));
    let sandbox = registry.sandbox();
    assert!(
        sandbox.is_docker(),
        "spawn validator registry must inherit the SpawnTool sandbox \
             (Docker here), not the pre-#1607 hardcoded NoSandbox"
    );
    assert!(
        !sandbox.is_noop(),
        "a real backend threaded via with_sandbox must not be a no-op"
    );
}

#[tokio::test]
async fn test_background_spawn_tracks_supervisor_lifecycle() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let supervisor = Arc::new(TaskSupervisor::new());
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_task_supervisor(
        supervisor.clone(),
        "api:test-session",
        PathBuf::from("/tmp/tasks.jsonl"),
    );

    let result = tool
        .execute(&serde_json::json!({
            "task": "Write a short answer",
            "label": "Deep research",
            "mode": "background",
            "allowed_tools": []
        }))
        .await
        .unwrap();

    assert!(result.success);

    let started = std::time::Instant::now();
    loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            if task.status == crate::task_supervisor::TaskStatus::Completed {
                assert_eq!(task.tool_name, "Deep research");
                break;
            }
        }
        assert!(
            started.elapsed() < BACKGROUND_DEADLINE,
            "background spawn task did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[cfg(unix)]
async fn test_before_spawn_verify_hook_can_replace_output_files() {
    let temp = tempfile::tempdir().unwrap();
    let replacement = temp.path().join("final-reviewed.pptx");
    std::fs::write(&replacement, "reviewed").unwrap();

    let hooks = Arc::new(HookExecutor::new(vec![rewrite_output_files_hook(
        &replacement,
    )]));
    let payload = HookPayload::before_spawn_verify(
        "task-1",
        "Slides deliverable",
        "api:test-session",
        "api:test-session:child",
        Some("slides"),
        Some("verify_outputs"),
        Some("candidate terminal outputs resolved"),
        vec!["/tmp/original-deck.pptx".to_string()],
        Some(&HookContext {
            session_id: Some("api:test-session".to_string()),
            profile_id: Some("test-profile".to_string()),
        }),
    );

    let modified_files = run_before_spawn_verify_hook(Some(&hooks), payload)
        .await
        .unwrap();

    assert_eq!(modified_files, vec![replacement]);
}

#[test]
fn workflow_terminal_output_prefers_final_audio_and_skips_intermediates() {
    let workflow = WorkflowMetadata {
        workflow_kind: "research_podcast".to_string(),
        current_phase: "generate_audio".to_string(),
        allowed_tools: vec!["podcast_generate".to_string()],
        terminal_output: Some(WorkflowTerminalOutputPolicy {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "audio".to_string(),
        }),
        progress: None,
    };

    let files_to_send = vec![
        PathBuf::from("/tmp/podcast_part_1.mp3"),
        PathBuf::from("/tmp/research_report.md"),
        PathBuf::from("/tmp/podcast_full_final.mp3"),
    ];
    let files_modified = vec![PathBuf::from("/tmp/script.md")];

    let selected =
        select_workflow_terminal_files(&files_to_send, &files_modified, Some(&workflow)).unwrap();

    assert_eq!(selected, vec![PathBuf::from("/tmp/podcast_full_final.mp3")]);
}

#[test]
fn deliverable_contract_surfaces_a_shell_written_file_no_tool_reported_it() {
    // Reproduces the mini4 "zero deliverable" bug: a worker with no
    // write_file in its toolset wrote its review via a shell heredoc, so
    // the tool-record path saw nothing. The seeded workspace contract must
    // surface the file regardless of how it was written.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    seed_deliverable_contract(root, "*.md").expect("seed deliverable contract");

    // Simulate `cat > octos-review.md <<EOF ...` — no write_file ran, so
    // files_modified / files_to_send are both empty.
    std::fs::write(root.join("octos-review.md"), "# Review\n").unwrap();

    // Control: the pre-existing tool-record path is blind to it (the bug).
    let via_tool_record = resolve_background_terminal_files(&[], &[], None).unwrap();
    assert!(
        via_tool_record.is_empty(),
        "tool-record path should see nothing: {via_tool_record:?}"
    );

    // Fix: the workspace contract surfaces the shell-written deliverable.
    let via_contract = resolve_deliverable_terminal_files(root);
    assert_eq!(via_contract.len(), 1, "contract surfaced: {via_contract:?}");
    assert!(via_contract[0].ends_with("octos-review.md"));
}

#[test]
fn subagent_tool_preflight_reports_missing_allowed_tool() {
    let tools = ToolRegistry::with_builtins("/tmp");

    let error = ensure_subagent_tools_available(&tools, &[String::from("podcast_generate")], true)
        .unwrap_err();

    assert!(error.contains("required tool(s) not available on this host"));
    assert!(error.contains("podcast_generate"));
}

#[tokio::test]
async fn child_spawn_clone_is_named_spawn_and_binds_spawn_agent_via_registry_swap() {
    // The clone the child-registry sites register must be named "spawn"
    // (so `ToolRegistry::register` swaps the delegate-less builtin
    // `spawn_agent` for a delegate-bound one) and must be rebased onto
    // the child's working directory rather than the parent's.
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let parent = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    )
    .with_deliverable_root(PathBuf::from("/runtime/deliverables"))
    .with_workspace_write_access(false);

    let child_id = AgentId::new("child-0");
    let child_spawn = parent.child_spawn_clone(PathBuf::from("/work/child"), &child_id);
    assert_eq!(child_spawn.name(), "spawn");
    assert_eq!(child_spawn.working_dir, PathBuf::from("/work/child"));
    assert_eq!(
        child_spawn.deliverable_root,
        Some(
            PathBuf::from("/runtime/deliverables")
                .join("child-0")
                .join("children")
        )
    );
    assert!(!child_spawn.workspace_write_access);

    // A bare builtins registry (what every child registry starts from)
    // carries only the delegate-LESS builtin spawn_agent — the reason a
    // nested spawn failed with "No native Octos spawn tool is bound".
    let mut registry = ToolRegistry::with_builtins("/tmp");
    assert!(
        registry.get("spawn").is_none(),
        "builtins must not carry a native spawn delegate on their own"
    );

    // Registering the clone triggers the swap: spawn + a delegate-bound
    // spawn_agent + delegate are all present for the child.
    registry.register(child_spawn);
    assert!(registry.get("spawn").is_some());
    assert!(registry.get("spawn_agent").is_some());
    assert!(registry.get("delegate").is_some());
}

#[tokio::test]
async fn test_background_spawn_emits_child_session_lifecycle_events() {
    let memory = Arc::new(create_test_store().await);
    let llm = Arc::new(MockProvider);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let supervisor = Arc::new(TaskSupervisor::new());
    let temp = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let ledger = temp.path().join("tasks.jsonl");
    let events = Arc::new(std::sync::Mutex::new(
        Vec::<ChildSessionLifecyclePayload>::new(),
    ));
    let events_ref = Arc::clone(&events);
    let sender: ChildSessionLifecycleSender = Arc::new(move |payload| {
        let events_ref = Arc::clone(&events_ref);
        Box::pin(async move {
            events_ref
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload);
            true
        })
    });

    let tool = SpawnTool::with_context(
        llm,
        memory,
        temp.path().to_path_buf(),
        tx,
        "api",
        "test-chat",
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session".to_string(), ledger)
    .with_child_session_sender(sender);

    let args = serde_json::json!({
        "task": "Draft the report",
        "mode": "background",
        "allowed_tools": []
    });
    let result = tool.execute(&args).await.unwrap();
    assert!(result.success);

    let started = std::time::Instant::now();
    loop {
        let events = events.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if events.len() >= 2 {
            assert_eq!(events[0].kind, ChildSessionLifecycleKind::Spawned);
            assert_eq!(events[1].kind, ChildSessionLifecycleKind::Completed);
            assert_eq!(events[0].parent_session_key, "api:test-session");
            assert_eq!(events[1].parent_session_key, "api:test-session");
            assert_eq!(events[0].child_session_key, events[1].child_session_key);
            assert_eq!(events[0].task_id, events[1].task_id);
            return;
        }

        assert!(
            started.elapsed() < BACKGROUND_DEADLINE,
            "child-session lifecycle events did not arrive in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// Minimal mock provider for testing
struct MockProvider;

#[async_trait]
impl LlmProvider for MockProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

async fn create_test_store() -> EpisodeStore {
    let dir = tempfile::tempdir().unwrap();
    // Leak the dir so it stays alive for the test
    let dir = Box::leak(Box::new(dir));
    EpisodeStore::open(dir.path()).await.unwrap()
}

/// Emits assistant text + a `list_dir` call, then a final text answer —
/// the minimal shape of a child that plans, uses a tool, and reports.
struct ContentThenToolProvider {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmProvider for ContentThenToolProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Ok(octos_llm::ChatResponse {
                content: Some("PLAN: scan the tree".into()),
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_ls".into(),
                    name: "list_dir".into(),
                    arguments: serde_json::json!({"path": "."}),
                    metadata: None,
                }],
                stop_reason: octos_llm::StopReason::ToolUse,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            });
        }
        Ok(octos_llm::ChatResponse {
            content: Some("FINAL: transcript test done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn background_child_transcript_streams_to_router_and_final_output_is_recorded() {
    // Mini4 re-review pipeline fix, end-to-end through a REAL detached
    // background child:
    //  (a) the child's transcript (assistant text + tool activity) must
    //      stream into the parent's SubAgentOutputRouter file — the
    //      live window `read_task_output` reads (old behaviour:
    //      SilentReporter dropped everything);
    //  (b) the child's full final result must be recorded on the task
    //      (`final_output`) through the real completion path.
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let router = Arc::new(crate::subagent_output::SubAgentOutputRouter::new(
        temp.path().join("router"),
    ));
    let tool = SpawnTool::new(
        Arc::new(ContentThenToolProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone())
    .with_parent_subagent_output_router(router.clone());

    let result = tool
        .execute(&serde_json::json!({
            "task": "scan the workspace and report",
            "label": "transcripter",
            "mode": "background",
            "allowed_tools": ["list_dir"]
        }))
        .await
        .unwrap();
    assert!(result.success, "dispatch failed: {}", result.output);

    let started = std::time::Instant::now();
    let task = loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            if task.status == crate::task_supervisor::TaskStatus::Completed {
                break task.clone();
            }
            if task.status == crate::task_supervisor::TaskStatus::Failed {
                panic!("background child failed: {:?}", task.error);
            }
        }
        assert!(
            started.elapsed() < BACKGROUND_DEADLINE,
            "background child did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    // (b) full final result recorded on the task record.
    let final_output = task
        .final_output
        .as_deref()
        .expect("final_output must be recorded at completion");
    assert!(
        final_output.contains("FINAL: transcript test done"),
        "final_output must carry the child's answer: {final_output:?}"
    );
    assert!(final_output.contains("Status: SUCCESS"));

    // (a) the transcript reached the parent's router file.
    let transcript = router
        .preview(&task.id)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    assert!(
        transcript.contains("[tool] list_dir"),
        "tool start must stream to the router: {transcript:?}"
    );
    assert!(
        transcript.contains("[tool ok] list_dir"),
        "tool completion must stream to the router: {transcript:?}"
    );
    assert!(
        transcript.contains("FINAL: transcript test done"),
        "assistant text must stream to the router: {transcript:?}"
    );
}

/// Emits one `shell` call that writes a deliverable via a redirect —
/// reporting no `file_modified`, exactly like the mini4 heredoc reviews —
/// then ends the turn.
struct ShellDeliverableProvider {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmProvider for ShellDeliverableProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Ok(octos_llm::ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_shell".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({
                        "command": "printf '# Review\\n' > octos-review.md",
                    }),
                    metadata: None,
                }],
                stop_reason: octos_llm::StopReason::ToolUse,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            });
        }
        Ok(octos_llm::ChatResponse {
            content: Some("wrote the review".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn background_deliverable_surfaces_shell_written_file_in_output_files() {
    // End-to-end reproduction of the mini4 "zero deliverable" flow: a
    // background spawn whose worker writes its deliverable with a raw
    // `shell` redirect — NO write_file, so no `file_modified` — must still
    // land in the task ledger's `output_files`, surfaced by the seeded
    // workspace contract rather than the tool-record path.
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let temp = tempfile::tempdir().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.enable_persistence(&ledger).unwrap();
    let tool = SpawnTool::new(
        Arc::new(ShellDeliverableProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(create_test_store().await),
        workspace.clone(),
        in_tx,
    )
    .with_task_supervisor(supervisor.clone(), "api:test-session", ledger.clone())
    .with_sandbox(SandboxConfig {
        mode: crate::sandbox::SandboxMode::None,
        ..Default::default()
    });

    let result = tool
        .execute(&serde_json::json!({
            "task": "Review the repo, then write octos-review.md",
            "label": "reviewer",
            "mode": "background",
            "allowed_tools": ["shell"],
            "deliverable": "*.md"
        }))
        .await
        .unwrap();
    assert!(result.success, "spawn dispatch failed: {}", result.output);

    let started = std::time::Instant::now();
    loop {
        let tasks = supervisor.get_tasks_for_session("api:test-session");
        if let Some(task) = tasks.first() {
            match task.status {
                crate::task_supervisor::TaskStatus::Completed => {
                    assert_eq!(
                        task.output_files.len(),
                        1,
                        "the shell-written deliverable must surface in output_files: {:?}",
                        task.output_files
                    );
                    assert!(
                        task.output_files[0].ends_with("octos-review.md"),
                        "output_files[0] = {:?}",
                        task.output_files[0]
                    );
                    break;
                }
                crate::task_supervisor::TaskStatus::Failed => {
                    panic!("background deliverable spawn failed: {:?}", task.error);
                }
                _ => {}
            }
        }
        assert!(
            started.elapsed() < BACKGROUND_DEADLINE,
            "background deliverable spawn did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Build a minimal `Input` from a JSON value with the defaults the
/// tests expect. Centralising this keeps the M8.2 manifest tests below
/// independent of future serde changes.
fn parse_spawn_input(value: serde_json::Value) -> Input {
    serde_json::from_value(value).expect("input parses")
}

#[test]
fn resolve_spawn_max_iterations_defaults_and_bounds() {
    // Not requested → the generous spawn default (NOT the interactive 50).
    assert_eq!(
        resolve_spawn_max_iterations(None),
        DEFAULT_SPAWN_MAX_ITERATIONS
    );
    // (That it exceeds the interactive 50 is asserted at compile time
    // beside the constant itself.)
    // Normal value passes through.
    assert_eq!(resolve_spawn_max_iterations(Some(120)), 120);
    // 0 is nonsensical → clamped up to 1.
    assert_eq!(resolve_spawn_max_iterations(Some(0)), 1);
    // Over the ceiling → clamped down (runaway-loop guard).
    assert_eq!(
        resolve_spawn_max_iterations(Some(100_000)),
        MAX_SPAWN_MAX_ITERATIONS
    );
    assert_eq!(
        resolve_spawn_max_iterations(Some(MAX_SPAWN_MAX_ITERATIONS)),
        MAX_SPAWN_MAX_ITERATIONS
    );
}

#[test]
fn should_resolve_manifest_in_spawn_tool() {
    // Spawn args reference `repo-editor`; the manifest's `tools`
    // list must flow into the resolved `Input.allowed_tools`. Inline
    // `allowed_tools` is empty so the manifest fills it in.
    let registry = crate::agents::AgentDefinitions::with_builtins();
    let mut input = parse_spawn_input(serde_json::json!({
        "task": "edit the repo files",
        "agent_definition_id": "repo-editor"
    }));
    apply_agent_definition(&mut input, &registry).expect("apply");

    // Repo-editor manifest lists the file tools + shell.
    for expected in [
        "read_file",
        "write_file",
        "edit_file",
        "shell",
        "grep",
        "glob",
    ] {
        assert!(
            input.allowed_tools.contains(&expected.to_string()),
            "manifest tool {expected} did not flow into allowed_tools"
        );
    }
}

#[test]
fn manifest_disallowed_tools_are_denied_by_policy_not_pruned_from_allow_list() {
    // inline [shell, grep] + manifest disallowed [shell]. The allow-list is
    // NOT mutated (shell stays); apply_agent_definition returns the
    // disallowed list, which build_subagent_tool_policy enforces as a
    // DENY. Deny wins over allow → shell blocked, grep allowed.
    let mut registry = crate::agents::AgentDefinitions::new();
    registry.insert(
        "example",
        crate::agents::AgentDefinition::from_json_str(
            r#"{
                    "name": "example",
                    "version": 1,
                    "tools": ["read_file", "shell"],
                    "disallowed_tools": ["shell"]
                }"#,
        )
        .expect("parse"),
    );

    let mut input = parse_spawn_input(serde_json::json!({
        "task": "do it",
        "agent_definition_id": "example",
        "allowed_tools": ["shell", "grep"]
    }));
    let disallowed = apply_agent_definition(&mut input, &registry).expect("apply");

    assert_eq!(disallowed, vec!["shell".to_string()]);
    // Inline list is kept verbatim — disallow is NOT a prune anymore.
    assert!(input.allowed_tools.contains(&"grep".to_string()));
    assert!(input.allowed_tools.contains(&"shell".to_string()));

    let policy = build_subagent_tool_policy(input.allowed_tools.clone(), disallowed);
    assert!(
        !policy.is_allowed("shell"),
        "manifest-disallowed shell must be denied by policy"
    );
    assert!(policy.is_allowed("grep"), "grep must remain allowed");
}

#[test]
fn should_error_when_agent_definition_id_unknown() {
    // Typos in the id are a hard error so a silent-typo cannot erase
    // the manifest's safety envelope.
    let registry = crate::agents::AgentDefinitions::with_builtins();
    let mut input = parse_spawn_input(serde_json::json!({
        "task": "do it",
        "agent_definition_id": "no-such-manifest"
    }));
    let err = apply_agent_definition(&mut input, &registry).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("no-such-manifest"), "message: {msg}");
}

// ────────── M8 Runtime Parity W2.B2 recovery prompt helper ──────────

#[test]
fn build_spawn_recovery_prompt_includes_task_and_error_text() {
    let prompt = build_spawn_recovery_prompt(
        "Generate a 5-slide deck on AI",
        "validator rejected child artifact: deck.pptx missing",
    );
    assert!(prompt.contains("[system-internal]"));
    assert!(prompt.contains("Generate a 5-slide deck on AI"));
    assert!(
        prompt.contains("validator rejected child artifact: deck.pptx missing"),
        "recovery prompt must surface the verbatim failure: {prompt}"
    );
    assert!(
        prompt.contains("different strategy") || prompt.contains("smaller scope"),
        "recovery prompt must direct the LLM toward an alternative"
    );
}

/// Provider that returns a hard `Err` on the first call and a
/// successful EndTurn on every subsequent call. Used to drive the
/// M8.9 recovery wrapper.
struct FailThenSucceedProvider {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmProvider for FailThenSucceedProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Err(eyre::eyre!("simulated provider failure"));
        }
        Ok(octos_llm::ChatResponse {
            content: Some("recovered".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn run_task_with_m8_9_recovery_retries_once_after_initial_failure() {
    let provider = Arc::new(FailThenSucceedProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let calls_ref = provider.calls.load(Ordering::SeqCst);
    assert_eq!(calls_ref, 0);

    let memory = Arc::new(create_test_store().await);
    let registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
    let worker = Agent::new(
        AgentId::new("test-worker"),
        provider.clone(),
        registry,
        memory,
    );
    let subtask = Task::new(
        TaskKind::Code {
            instruction: "Recover me".into(),
            files: vec![],
        },
        TaskContext {
            working_dir: PathBuf::from("/tmp"),
            ..Default::default()
        },
    );

    let result = run_task_with_m8_9_recovery(&worker, &subtask, "Recover me").await;
    let task_result = result.expect("recovery succeeds");
    assert!(task_result.success, "recovery turn must succeed");
    assert!(
        provider.calls.load(Ordering::SeqCst) >= 2,
        "recovery must invoke the provider at least twice (one fail + one retry); got {}",
        provider.calls.load(Ordering::SeqCst)
    );
}

/// Guard C regression: a spawn invocation at depth 4 must refuse
/// before any backend dispatch, surfacing a structured tool failure
/// the LLM can react to. The depth gate fires before
/// argument parsing — even invalid JSON returns the depth-limit
/// error rather than the legacy "invalid spawn tool input" path.
#[tokio::test]
async fn spawn_refuses_at_depth_4() {
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        PathBuf::from("/tmp"),
        in_tx,
    );

    // Build a ToolContext at the depth cap. The spawn tool reads
    // `ctx.spawn_depth` and refuses before parsing args.
    let mut ctx = super::super::ToolContext::zero();
    ctx.spawn_depth = MAX_SPAWN_DEPTH;

    let result = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "do something deeply nested"
            }),
        )
        .await;
    let tool_result = match result {
        Ok(r) => r,
        Err(error) => panic!("depth refusal should return Ok(failed) rather than Err: {error}"),
    };
    assert!(!tool_result.success, "spawn at the cap must report failure");
    assert!(
        tool_result
            .output
            .contains(&format!("spawn depth limit ({MAX_SPAWN_DEPTH}) exceeded")),
        "structured reason missing from output: {}",
        tool_result.output
    );
    assert!(
        tool_result.output.contains("refusing further nesting"),
        "structured reason missing from output: {}",
        tool_result.output
    );

    // Sanity: at depth 0 the tool keeps working (no early refusal).
    let mut ctx0 = super::super::ToolContext::zero();
    ctx0.spawn_depth = 0;
    // We pass an empty input so the legacy validation path runs. A
    // zero-depth spawn does NOT short-circuit with the depth-limit
    // refusal — it falls through into the regular pipeline (which
    // surfaces an unrelated error for the empty input).
    let baseline = tool
        .execute_with_context(&ctx0, &serde_json::json!({}))
        .await;
    match baseline {
        Ok(r) => {
            assert!(
                !r.output.contains("spawn depth limit"),
                "below-cap spawn must not emit the depth-limit refusal: {}",
                r.output
            );
        }
        Err(error) => {
            let err_msg = format!("{error}");
            assert!(
                !err_msg.contains("spawn depth limit"),
                "below-cap spawn must not emit the depth-limit refusal: {err_msg}"
            );
        }
    }
}

// -----------------------------------------------------------------------
// Phase 2-D: SessionScope propagation tests for SpawnTool.
//
// The migrated spawn tool reads `ctx.session_scope` and threads it
// onto the child Agent via `Agent::with_session_scope`. The child
// Agent's execution loop then plants the same scope onto every
// child `ToolContext` (see `agent/execution.rs`). These tests
// exercise that path end-to-end by mounting a recording tool on the
// child registry and asserting on what `execute_with_context` sees.
// -----------------------------------------------------------------------

/// Test-only tool that records the `session_scope.workspace()` it
/// observes on its `ToolContext`. Used by the Phase 2-D propagation
/// tests to capture what the child Agent's execution loop hands to
/// migrated tools. Lives only inside `#[cfg(test)]`.
struct ScopeRecordingTool {
    observed: Arc<std::sync::Mutex<Option<PathBuf>>>,
}

#[async_trait]
impl Tool for ScopeRecordingTool {
    fn name(&self) -> &str {
        "scope_probe"
    }

    fn description(&self) -> &str {
        "test-only tool that records the session_scope it observes"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        self.execute_with_context(&super::super::ToolContext::zero(), args)
            .await
    }

    async fn execute_with_context(
        &self,
        ctx: &super::super::ToolContext,
        _args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let observed = ctx
            .session_scope
            .as_ref()
            .map(|scope| scope.workspace().to_path_buf());
        *self.observed.lock().unwrap_or_else(|e| e.into_inner()) = observed;
        Ok(ToolResult {
            output: "ok".to_string(),
            success: true,
            ..Default::default()
        })
    }
}

/// Mock provider that calls `scope_probe` once and then ends — drives
/// the child Agent through exactly one tool execution so the
/// recording tool sees the migrated `ToolContext`.
struct ScopeProbeProvider;

#[async_trait]
impl LlmProvider for ScopeProbeProvider {
    async fn chat(
        &self,
        messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        // First call → invoke scope_probe; second call (after the
        // probe's tool_result lands) → end the turn.
        let probe_already_run = messages
            .iter()
            .any(|msg| matches!(msg.role, octos_core::MessageRole::Tool));
        if probe_already_run {
            Ok(octos_llm::ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        } else {
            Ok(octos_llm::ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_scope_probe".into(),
                    name: "scope_probe".into(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }],
                stop_reason: octos_llm::StopReason::ToolUse,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn run_git(repo: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git command starts");
    assert!(status.success(), "git {args:?} failed with {status}");
}

/// `git init` + one commit at `repo`, so worktree tests have a HEAD to
/// branch worker worktrees from.
#[test]
fn is_inside_git_work_tree_detects_repo_vs_plain_dir() {
    let git_dir = tempfile::tempdir().unwrap();
    init_worktree_test_repo(git_dir.path());
    assert!(
        is_inside_git_work_tree(git_dir.path()),
        "an initialized repo must be detected as a work tree"
    );

    let plain = tempfile::tempdir().unwrap();
    assert!(
        !is_inside_git_work_tree(plain.path()),
        "a non-git directory must not be detected as a work tree"
    );
}

fn init_worktree_test_repo(repo: &std::path::Path) {
    std::fs::create_dir_all(repo).unwrap();
    run_git(repo, &["init"]);
    std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
    run_git(repo, &["add", "shared.txt"]);
    run_git(
        repo,
        &[
            "-c",
            "user.name=Octos Test",
            "-c",
            "user.email=octos@example.invalid",
            "commit",
            "-m",
            "base",
        ],
    );
}

fn git_worktree_list(repo: &std::path::Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git worktree list runs");
    assert!(output.status.success(), "git worktree list failed");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The `.octos/work` root may exist (it is created before the last
/// refusal points), but a refused spawn must not leave any worker
/// worktree directory inside it.
fn assert_no_worker_worktrees(repo: &std::path::Path) {
    let work_root = repo.join(".octos/work");
    if !work_root.exists() {
        return;
    }
    let leftovers: Vec<PathBuf> = std::fs::read_dir(&work_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(
        leftovers.is_empty(),
        "refused spawn left worker worktrees behind: {leftovers:?}"
    );
}

/// PR #1250 finding 2: session-scope validation must run BEFORE
/// `git worktree add`. With a scope rooted at a repo SUBDIR, the
/// planned worktree (`<repo-root>/.octos/work/...`) falls outside the
/// session root: the spawn must be refused with nothing created.
#[tokio::test]
async fn worktree_isolation_refuses_scope_escape_before_creating() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_worktree_test_repo(&repo);
    let session_root = repo.join("session-root");
    std::fs::create_dir_all(&session_root).unwrap();
    let baseline = git_worktree_list(&repo);

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(MockProvider),
        Arc::new(create_test_store().await),
        session_root.clone(),
        in_tx,
    );
    let scope =
        octos_core::SessionScope::solo(session_root.clone(), vec![]).expect("scope construction");
    let mut ctx = super::super::ToolContext::zero();
    ctx.session_scope = Some(Arc::new(scope));

    let result = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "edit shared.txt",
                "mode": "sync",
                "isolation": "worktree"
            }),
        )
        .await
        .unwrap();

    assert!(
        !result.success,
        "a worktree outside the session root must refuse the spawn"
    );
    assert!(
        result.output.contains("rejected by session scope"),
        "refusal must name the scope rejection; got: {}",
        result.output
    );
    assert_eq!(
        git_worktree_list(&repo),
        baseline,
        "scope-refused spawn must not create a worktree"
    );
    assert_no_worker_worktrees(&repo);
    assert!(
        !repo.join(".octos/work").exists(),
        "scope-refused spawn must not create the work root either"
    );
}

#[test]
fn worker_worktree_slug_validation_rejects_traversal() {
    for valid in ["subagent-0", "abc.DEF_123", "parent/child-1"] {
        validate_worker_worktree_slug(valid).expect("valid slug accepted");
    }

    for invalid in [
        "",
        ".",
        "..",
        "../escape",
        "escape/..",
        "/absolute",
        "\\absolute",
        "bad\\slash",
        "bad space",
    ] {
        assert!(
            validate_worker_worktree_slug(invalid).is_err(),
            "invalid slug {invalid:?} was accepted"
        );
    }
}

#[tokio::test]
async fn spawn_propagates_scope_to_sub_agent() {
    // When the parent `ToolContext` carries a `SessionScope`, the
    // sync-mode sub-agent's tools must see the same scope on their
    // own `ToolContext`. Without this, a session's filesystem
    // contract is forgotten the moment work is delegated to a
    // sub-agent.
    let scope_dir = tempfile::tempdir().unwrap();
    let scope = octos_core::SessionScope::solo(scope_dir.path().to_path_buf(), vec![])
        .expect("scope construction");

    let observed = Arc::new(std::sync::Mutex::new(None::<PathBuf>));
    let observed_for_factory = observed.clone();

    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let tool = SpawnTool::new(
        Arc::new(ScopeProbeProvider),
        Arc::new(create_test_store().await),
        scope_dir.path().to_path_buf(),
        in_tx,
    )
    .with_child_tool_factory(Arc::new(move || {
        Arc::new(ScopeRecordingTool {
            observed: observed_for_factory.clone(),
        })
    }));

    let mut ctx = super::super::ToolContext::zero();
    ctx.session_scope = Some(Arc::new(scope));

    let result = tool
        .execute_with_context(
            &ctx,
            &serde_json::json!({
                "task": "probe the scope",
                "mode": "sync",
                "allowed_tools": ["scope_probe"]
            }),
        )
        .await
        .expect("spawn returns Ok");
    assert!(
        result.success,
        "expected sync spawn success: {}",
        result.output
    );

    let captured = observed.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let captured = captured.expect(
        "scope_probe must observe a SessionScope on its ToolContext — \
             without Phase 2-D propagation the child Agent runs scope-less",
    );
    let canonical_expected =
        std::fs::canonicalize(scope_dir.path()).expect("canonicalise scope dir");
    let canonical_observed =
        std::fs::canonicalize(&captured).expect("canonicalise observed workspace");
    assert_eq!(
        canonical_observed, canonical_expected,
        "child Agent's session_scope.workspace() must match the parent's"
    );
}
