use super::*;

/// #1723: `record_final_output` fires `on_change` so the roster mirror
/// (`upsert_background_task_agent` → `set_agent_output_if_empty`) re-runs
/// with `final_output` present. It is called AFTER `mark_completed`, so the
/// terminal on_change already fired while `final_output` was `None`; without
/// this second notification the agent view / `/ps` detail stays empty.
#[test]
fn record_final_output_fires_on_change_with_the_output() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("spawn", "call-final-output", None);
    supervisor.mark_completed(&id, vec![]);

    // Only observe changes AFTER completion, so we isolate the
    // record_final_output notification.
    let seen_output = Arc::new(std::sync::Mutex::new(Option::<String>::None));
    let calls = Arc::new(AtomicUsize::new(0));
    let seen_c = seen_output.clone();
    let calls_c = calls.clone();
    supervisor.set_on_change(move |task| {
        calls_c.fetch_add(1, Ordering::SeqCst);
        if let Some(output) = task.final_output.clone() {
            *seen_c.lock().unwrap() = Some(output);
        }
    });

    supervisor.record_final_output(&id, "the child's full result");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "record_final_output must fire on_change exactly once"
    );
    assert_eq!(
        seen_output.lock().unwrap().as_deref(),
        Some("the child's full result"),
        "the on_change snapshot must carry the recorded final_output"
    );
}

#[test]
fn should_register_task_with_spawned_status() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("tts", "call-123", None);

    let tasks = supervisor.get_all_tasks();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, id);
    assert_eq!(tasks[0].tool_name, "tts");
    assert_eq!(tasks[0].tool_call_id, "call-123");
    assert_eq!(tasks[0].status, TaskStatus::Spawned);
    assert_eq!(tasks[0].runtime_state, TaskRuntimeState::Spawned);
    assert!(tasks[0].child_terminal_state.is_none());
    assert!(tasks[0].child_join_state.is_none());
    assert!(tasks[0].child_failure_action.is_none());
    assert!(tasks[0].completed_at.is_none());
    assert!(tasks[0].updated_at >= tasks[0].started_at);
}

/// Gap-1 unification: the unified `on_terminal` callback fires exactly
/// once per task for both success and failure transitions, carrying the
/// correct outcome + (for failures) the synth-ack-as-prompt-selection
/// boolean. Idempotent under repeated terminal marks.
#[test]
fn on_terminal_fires_once_for_success_and_failure_with_correct_payload() {
    use std::sync::{Arc, Mutex};

    // ── success ──────────────────────────────────────────────────
    let supervisor = TaskSupervisor::new();
    let events: Arc<Mutex<Vec<TerminalEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    supervisor.set_on_terminal(move |event: &TerminalEvent| {
        sink.lock().unwrap().push(event.clone());
    });

    let ok = supervisor.register("bg_research", "call-ok", Some("web:s1"));
    supervisor.mark_running(&ok);
    supervisor.mark_completed(&ok, vec!["/tmp/octos/out.md".to_owned()]);
    // Idempotent: a defensive double mark must not re-fire.
    supervisor.mark_completed(&ok, vec!["/tmp/octos/out.md".to_owned()]);

    {
        let observed = events.lock().unwrap();
        let completed: Vec<_> = observed
            .iter()
            .filter(|e| matches!(e.outcome, TerminalOutcome::Completed))
            .collect();
        assert_eq!(
            completed.len(),
            1,
            "exactly one Completed terminal event must fire (idempotent)"
        );
        assert_eq!(completed[0].task.id, ok);
        assert!(
            !completed[0].synth_ack_emitted,
            "completion events do not consult synth-ack"
        );
        assert!(!completed[0].is_failure());
    }

    // ── failure WITH synth-ack (recovery body should be selected) ──
    let with_ack = supervisor.register_with_input_and_cmid(
        "mofa_slides",
        "call-fail-ack",
        Some("web:s1"),
        Some(serde_json::json!({"topic": "rust"})),
        Some("cmid-42".to_owned()),
    );
    supervisor.mark_synth_ack_emitted("call-fail-ack");
    supervisor.mark_running(&with_ack);
    supervisor.mark_failed(
        &with_ack,
        "plugin exited 137. available: a, b, c".to_owned(),
    );
    // Idempotent re-mark (live + cascade collapse to one event).
    supervisor.mark_failed(&with_ack, "second mark".to_owned());

    // ── failure WITHOUT synth-ack (suppression at prompt selection) ─
    let no_ack = supervisor.register_with_input_and_cmid(
        "mofa_slides",
        "call-fail-noack",
        Some("web:s1"),
        Some(serde_json::json!({"topic": "go"})),
        None,
    );
    supervisor.mark_running(&no_ack);
    supervisor.mark_failed(&no_ack, "sibling suppressed".to_owned());

    let observed = events.lock().unwrap();
    let with_ack_event = observed
        .iter()
        .find(|e| e.task.id == with_ack)
        .expect("failure-with-ack event present");
    assert!(with_ack_event.is_failure());
    assert!(
        with_ack_event.synth_ack_emitted,
        "failure-with-ack must carry synth_ack_emitted=true so the consumer renders the recovery body"
    );
    let sig = with_ack_event.failure_signal().expect("failure signal");
    assert_eq!(sig.tool_name, "mofa_slides");
    assert_eq!(
        sig.originating_client_message_id.as_deref(),
        Some("cmid-42")
    );
    assert_eq!(
        sig.suggested_alternatives,
        vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        "alternatives must be parsed off the error text",
    );

    let no_ack_event = observed
        .iter()
        .find(|e| e.task.id == no_ack)
        .expect("failure-without-ack event present");
    assert!(no_ack_event.is_failure());
    assert!(
        !no_ack_event.synth_ack_emitted,
        "failure-without-ack must carry synth_ack_emitted=false so the consumer suppresses the recovery body"
    );

    // Exactly one event per task id.
    let with_ack_count = observed.iter().filter(|e| e.task.id == with_ack).count();
    assert_eq!(
        with_ack_count, 1,
        "failure event must fire exactly once per task"
    );
}

#[test]
fn should_transition_through_lifecycle_states() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("tts", "call-1", None);
    let task = &supervisor.get_all_tasks()[0];
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Queued);

    supervisor.mark_running(&id);
    let task = &supervisor.get_all_tasks()[0];
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.runtime_state, TaskRuntimeState::ExecutingTool);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Running);

    supervisor.mark_runtime_state(
        &id,
        TaskRuntimeState::DeliveringOutputs,
        Some("send_file".to_string()),
    );
    let task = &supervisor.get_all_tasks()[0];
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.runtime_state, TaskRuntimeState::DeliveringOutputs);
    assert_eq!(task.runtime_detail.as_deref(), Some("send_file"));
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Verifying);

    supervisor.mark_completed(&id, vec!["output.mp3".to_string()]);
    let task = &supervisor.get_all_tasks()[0];
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.runtime_state, TaskRuntimeState::Completed);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Ready);
    assert!(task.completed_at.is_some());
    assert_eq!(task.output_files, vec!["output.mp3"]);
}

#[test]
fn should_persist_child_session_outcome_state() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("tts", "call-7", Some("api:session"));

    supervisor.mark_child_session_outcome(
        &id,
        ChildSessionTerminalState::RetryableFailure,
        ChildSessionJoinState::Joined,
    );

    let task = supervisor.get_task(&id).expect("task missing");
    assert_eq!(
        task.child_terminal_state,
        Some(ChildSessionTerminalState::RetryableFailure)
    );
    assert_eq!(task.child_join_state, Some(ChildSessionJoinState::Joined));
    assert_eq!(
        task.child_failure_action,
        Some(ChildSessionFailureAction::Retry)
    );
    assert!(task.child_joined_at.is_some());
}

#[test]
fn should_track_failed_tasks_with_error() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("tts", "call-2", None);

    supervisor.mark_running(&id);
    supervisor.mark_failed(&id, "connection refused".to_string());

    let task = &supervisor.get_all_tasks()[0];
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.runtime_state, TaskRuntimeState::Failed);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Failed);
    assert_eq!(task.error.as_deref(), Some("connection refused"));
    assert!(task.completed_at.is_some());
}

#[test]
fn should_return_only_active_tasks_in_get_active() {
    let supervisor = TaskSupervisor::new();
    let id1 = supervisor.register("tts", "call-1", None);
    let _id2 = supervisor.register("tts", "call-2", None);

    supervisor.mark_completed(&id1, vec![]);

    let active = supervisor.get_active_tasks();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].tool_call_id, "call-2");
}

#[test]
fn should_restore_completed_and_failed_truth_after_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let supervisor = TaskSupervisor::new();
    supervisor.enable_persistence(&ledger_path).unwrap();

    let completed = supervisor.register_with_lineage("fm_tts", "call-2", Some("api:session"), None);
    supervisor.mark_running(&completed);
    supervisor.mark_runtime_state(
        &completed,
        TaskRuntimeState::DeliveringOutputs,
        Some("send_file".to_string()),
    );
    supervisor.mark_completed(&completed, vec!["/tmp/output.mp3".to_string()]);
    supervisor.mark_child_session_outcome(
        &completed,
        ChildSessionTerminalState::Completed,
        ChildSessionJoinState::Joined,
    );

    let failed =
        supervisor.register_with_lineage("podcast_generate", "call-3", Some("api:session"), None);
    supervisor.mark_running(&failed);
    supervisor.mark_failed(&failed, "No dialogue lines found in script".to_string());
    supervisor.mark_child_session_outcome(
        &failed,
        ChildSessionTerminalState::TerminalFailure,
        ChildSessionJoinState::Orphaned,
    );

    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();

    let tasks = restored.get_all_tasks();
    assert_eq!(tasks.len(), 2);

    let completed_task = tasks
        .iter()
        .find(|task| task.id == completed)
        .expect("completed task missing");
    assert_eq!(completed_task.status, TaskStatus::Completed);
    assert_eq!(completed_task.runtime_state, TaskRuntimeState::Completed);
    assert_eq!(completed_task.runtime_detail.as_deref(), Some("send_file"));
    assert_eq!(completed_task.output_files, vec!["/tmp/output.mp3"]);
    let expected_completed_child = format!("api:session#child-{completed}");
    assert_eq!(
        completed_task.parent_session_key.as_deref(),
        Some("api:session")
    );
    assert_eq!(
        completed_task.child_session_key.as_deref(),
        Some(expected_completed_child.as_str())
    );
    assert_eq!(
        completed_task.task_ledger_path.as_deref(),
        Some(ledger_path.to_str().unwrap())
    );
    assert_eq!(
        completed_task.child_terminal_state,
        Some(ChildSessionTerminalState::Completed)
    );
    assert_eq!(
        completed_task.child_join_state,
        Some(ChildSessionJoinState::Joined)
    );
    assert_eq!(completed_task.child_failure_action, None);
    assert!(completed_task.child_joined_at.is_some());

    let failed_task = tasks
        .iter()
        .find(|task| task.id == failed)
        .expect("failed task missing");
    assert_eq!(failed_task.status, TaskStatus::Failed);
    assert_eq!(failed_task.runtime_state, TaskRuntimeState::Failed);
    assert_eq!(failed_task.runtime_detail, None);
    assert_eq!(
        failed_task.error.as_deref(),
        Some("No dialogue lines found in script")
    );
    assert_eq!(
        failed_task.parent_session_key.as_deref(),
        Some("api:session")
    );
    let expected_failed_child = format!("api:session#child-{failed}");
    assert_eq!(
        failed_task.child_session_key.as_deref(),
        Some(expected_failed_child.as_str())
    );
    assert_eq!(
        failed_task.task_ledger_path.as_deref(),
        Some(ledger_path.to_str().unwrap())
    );
    assert_eq!(
        failed_task.child_terminal_state,
        Some(ChildSessionTerminalState::TerminalFailure)
    );
    assert_eq!(
        failed_task.child_join_state,
        Some(ChildSessionJoinState::Orphaned)
    );
    assert_eq!(
        failed_task.child_failure_action,
        Some(ChildSessionFailureAction::Escalate)
    );
    assert!(failed_task.child_joined_at.is_none());
}

// ── M8.9: spawn_only failure recovery signals ───────────────────────────

use std::sync::Mutex as StdMutex;

fn collect_failure_signals(
    supervisor: &TaskSupervisor,
) -> Arc<StdMutex<Vec<SpawnOnlyFailureSignal>>> {
    let collected = Arc::new(StdMutex::new(Vec::new()));
    let captured = Arc::clone(&collected);
    supervisor.set_on_failure_signal(move |signal| {
        captured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(signal.clone());
    });
    collected
}

#[test]
fn should_emit_failure_signal_when_spawn_only_task_status_becomes_failed() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register_with_input(
        "fm_tts",
        "call-1",
        Some("api:session"),
        Some(serde_json::json!({"voice": "yangmi", "text": "hi"})),
    );
    // Synth-ack gate: simulate the LLM having seen the
    // "Background work started for `fm_tts`." ack — production wires
    // this from `loop_runner.rs` when the synth-ack fires.
    supervisor.mark_synth_ack_emitted("call-1");
    supervisor.mark_running(&task_id);
    supervisor.mark_failed(
        &task_id,
        "voice 'yangmi' not registered. available: vivian, serena, longxiang".to_string(),
    );

    let signals = collected.lock().unwrap().clone();
    assert_eq!(signals.len(), 1, "expected exactly one failure signal");
    let signal = &signals[0];
    assert_eq!(signal.task_id, task_id);
    assert_eq!(signal.tool_name, "fm_tts");
    assert_eq!(signal.parent_session_key.as_deref(), Some("api:session"));
    assert!(
        signal
            .error_message
            .contains("voice 'yangmi' not registered")
    );
    assert_eq!(
        signal.suggested_alternatives,
        vec![
            "vivian".to_string(),
            "serena".to_string(),
            "longxiang".to_string()
        ]
    );
    assert_eq!(signal.tool_input["voice"], "yangmi");
}

#[test]
fn should_not_emit_signal_on_successful_completion() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-2", None);
    supervisor.mark_running(&task_id);
    supervisor.mark_completed(&task_id, vec!["/tmp/out.mp3".to_string()]);

    assert!(
        collected.lock().unwrap().is_empty(),
        "completion must not emit failure signal"
    );
}

#[test]
fn should_capture_tool_input_in_failure_signal() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let input = serde_json::json!({
        "voice": "yangmi",
        "text": "hello world",
        "format": "mp3",
    });
    let task_id = supervisor.register_with_input("fm_tts", "call-5", None, Some(input.clone()));
    supervisor.mark_synth_ack_emitted("call-5");
    supervisor.mark_failed(&task_id, "internal error".to_string());

    let signals = collected.lock().unwrap().clone();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].tool_input, input);
}

#[test]
fn should_emit_failure_signal_only_after_synth_ack_recorded_for_tool_call_id() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);

    // First task — synth-ack was suppressed, failure must NOT signal.
    let suppressed_task = supervisor.register("fm_tts", "call-suppressed", None);
    supervisor.mark_failed(&suppressed_task, "boom A".to_string());

    // Second task — synth-ack fired, failure MUST signal.
    let acked_task = supervisor.register("fm_tts", "call-acked", None);
    supervisor.mark_synth_ack_emitted("call-acked");
    supervisor.mark_failed(&acked_task, "boom B".to_string());

    let signals = collected.lock().unwrap().clone();
    assert_eq!(
        signals.len(),
        1,
        "exactly one failure signal — the synth-acked task — must reach the callback",
    );
    assert_eq!(signals[0].task_id, acked_task);
}

// ── F004 B2: TaskSupervisor → ToolProgress bridge ─────────────────────

/// Test reporter that captures every reported event so the bridge
/// assertions can branch on event kind without parsing JSON.
struct CapturingReporter {
    events: Arc<StdMutex<Vec<crate::progress::ProgressEvent>>>,
}

impl crate::progress::ProgressReporter for CapturingReporter {
    fn report(&self, event: crate::progress::ProgressEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }
}

fn collect_progress_events(
    supervisor: &TaskSupervisor,
) -> Arc<StdMutex<Vec<crate::progress::ProgressEvent>>> {
    let events = Arc::new(StdMutex::new(Vec::new()));
    let reporter = Arc::new(CapturingReporter {
        events: Arc::clone(&events),
    });
    supervisor.set_progress_reporter(reporter);
    events
}

fn extract_tool_progress(
    events: &[crate::progress::ProgressEvent],
) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            crate::progress::ProgressEvent::ToolProgress {
                name,
                tool_id,
                message,
            } => Some((name.clone(), tool_id.clone(), message.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn should_emit_tool_progress_on_completion_with_tool_call_id() {
    let supervisor = TaskSupervisor::new();
    let events = collect_progress_events(&supervisor);
    let task_id = supervisor.register("podcast_generate", "call-complete-1", None);
    supervisor.mark_completed(&task_id, vec!["/tmp/out.mp3".to_string()]);

    let captured = events.lock().unwrap().clone();
    let tool_progress = extract_tool_progress(&captured);
    let completion = tool_progress
        .iter()
        .find(|(_, _, message)| message.ends_with(": completed"))
        .expect("completion progress event missing");
    assert_eq!(completion.0, "podcast_generate");
    assert_eq!(completion.1, "call-complete-1");
    assert_eq!(completion.2, "podcast_generate: completed");
}

// ────────── M7.9 cancel / relaunch primitives (W2) ──────────

#[test]
fn cancel_running_task_transitions_to_cancelled_and_fires_token() {
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register("bg_research", "call-cancel-1", Some("session-A"));
    supervisor.mark_running(&task_id);
    let token = supervisor.cancel_token(&task_id);
    assert!(!token.is_cancelled());

    supervisor.cancel(&task_id).expect("cancel should succeed");

    let task = supervisor.get_task(&task_id).expect("task still tracked");
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Cancelled);
    assert!(token.is_cancelled());
    assert!(task.completed_at.is_some());
}

#[test]
fn cancel_unknown_task_returns_not_found() {
    let supervisor = TaskSupervisor::new();
    let result = supervisor.cancel("does-not-exist");
    assert_eq!(result, Err(TaskCancelError::NotFound));
}

// ────────── M8 Req #4 DoD: cancel cannot be overwritten by late workers ──────────

/// Race regression: a worker that finishes AFTER the user has cancelled
/// the task must NOT resurrect it to `Completed`. The supervisor's
/// `mark_completed` guard short-circuits when the task is already in a
/// terminal state. Asserts state stays `Cancelled`, the on_change callback
/// fires exactly twice (once for `mark_running`, once for `cancel`), and
/// the ProgressReporter does NOT emit a spurious "completed" event after
/// cancellation.
#[test]
fn mark_completed_after_cancel_does_not_overwrite_cancelled_state() {
    use std::sync::Mutex;
    let supervisor = TaskSupervisor::new();
    let progress_events = collect_progress_events(&supervisor);
    let on_change_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    {
        let on_change_count = on_change_count.clone();
        supervisor.set_on_change(move |_task| {
            *on_change_count.lock().unwrap() += 1;
        });
    }

    let task_id = supervisor.register("bg_research", "call-race-1", Some("session-X"));
    supervisor.mark_running(&task_id); // notify #1
    supervisor.cancel(&task_id).expect("cancel should succeed"); // notify #2

    // Late-arriving worker tries to mark completed — this is the race.
    supervisor.mark_completed(&task_id, vec!["late/output.bin".into()]); // must noop

    let task = supervisor.get_task(&task_id).expect("task still tracked");
    assert_eq!(
        task.status,
        TaskStatus::Cancelled,
        "late mark_completed must NOT overwrite Cancelled state"
    );
    assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Cancelled);
    assert!(
        task.output_files.is_empty(),
        "late completion's output_files must not leak onto a Cancelled task, got: {:?}",
        task.output_files
    );

    // on_change must have fired exactly twice — guard noop must not
    // double-fire the change callback.
    assert_eq!(
        *on_change_count.lock().unwrap(),
        2,
        "on_change should fire exactly twice (mark_running + cancel), not for the noop mark_completed"
    );

    // ProgressReporter must not have emitted any "completed" message
    // after cancellation. We saw running + cancelled, but never completed.
    let captured = progress_events.lock().unwrap().clone();
    let tool_progress = extract_tool_progress(&captured);
    let post_cancel_completed: Vec<_> = tool_progress
        .iter()
        .filter(|(_, _, message)| message.contains("completed"))
        .collect();
    assert!(
        post_cancel_completed.is_empty(),
        "guard must not emit 'completed' progress for a cancelled task, got: {tool_progress:?}"
    );
}

#[test]
fn relaunch_failed_task_creates_successor_and_fires_callback() {
    use std::sync::Mutex;
    let supervisor = TaskSupervisor::new();
    let captured: Arc<Mutex<Vec<RelaunchRequest>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let captured = captured.clone();
        supervisor.set_on_relaunch(move |req| {
            captured.lock().unwrap().push(req.clone());
        });
    }

    let task_id = supervisor.register("bg_research", "call-relaunch-1", Some("session-D"));
    supervisor.mark_running(&task_id);
    supervisor.mark_failed(&task_id, "node 'design' failed".to_string());

    let new_id = supervisor
        .relaunch(
            &task_id,
            RelaunchOpts {
                from_node: Some("design".into()),
            },
        )
        .expect("relaunch should succeed");
    assert_ne!(new_id, task_id, "relaunch must allocate a fresh id");

    let new_task = supervisor.get_task(&new_id).expect("successor registered");
    assert_eq!(new_task.tool_name, "bg_research");
    assert_eq!(new_task.tool_call_id, "call-relaunch-1");
    assert_eq!(new_task.session_key.as_deref(), Some("session-D"));

    let log = captured.lock().unwrap();
    assert_eq!(log.len(), 1, "relaunch callback fired exactly once");
    assert_eq!(log[0].original_task_id, task_id);
    assert_eq!(log[0].new_task_id, new_id);
    assert_eq!(log[0].opts.from_node.as_deref(), Some("design"));
}

#[test]
fn cancel_token_notifies_waiters() {
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register("bg_research", "call-cancel-notify", None);
    supervisor.mark_running(&task_id);
    let token = supervisor.cancel_token(&task_id);

    // Drive a small async runtime so the token can fire its
    // notification path (poll-then-wait).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let waiter = {
            let token = token.clone();
            tokio::spawn(async move { token.cancelled().await })
        };
        // Yield so the waiter actually parks on `notified()`.
        tokio::task::yield_now().await;
        supervisor.cancel(&task_id).expect("cancel should succeed");
        tokio::time::timeout(std::time::Duration::from_millis(500), waiter)
            .await
            .expect("waiter must wake within 500ms")
            .expect("waiter task panicked");
    });
    assert!(token.is_cancelled());
}

/// Guard A regression: a parent session that has already accepted
/// `MAX_CHILDREN_PER_PARENT` children must refuse the next register
/// with a structured `ChildFanoutExceeded` error and force-fail every
/// still-active child so the cascade collapses.
#[test]
fn register_task_refuses_201st_child_for_same_parent() {
    // Use a smaller cap via env var so the test does not allocate
    // 200+ tasks in CI. The cap reader caches once per process — we
    // run this test in isolation with a fresh `TaskSupervisor` and a
    // sub-process-friendly cap value that is set before any other
    // register call resolves the cache.
    //
    // Note: setting `OCTOS_MAX_CHILDREN_PER_PARENT` here would be
    // racy because `max_children_per_parent` caches with `OnceLock`.
    // Instead we exercise the production cap (200) — register 200
    // children, then assert the 201st is refused.
    let parent_session = "api:test-parent";
    let supervisor = TaskSupervisor::new();
    for i in 0..MAX_CHILDREN_PER_PARENT {
        let id = supervisor
            .try_register_with_input("tts", &format!("call-{i}"), Some(parent_session), None)
            .unwrap_or_else(|err| panic!("register #{i} should succeed; got {err}"));
        // Mark a slice of the children as active (Running) so the
        // force-fail cascade has something to flip on the 201st
        // call. Leaving every task in Spawned (also active) works
        // identically.
        if i % 2 == 0 {
            supervisor.mark_running(&id);
        }
    }
    assert_eq!(
        supervisor.get_tasks_for_session(parent_session).len(),
        MAX_CHILDREN_PER_PARENT,
        "supervisor should hold exactly the cap before the refusal fires"
    );

    // The 201st register must be refused with a typed error that
    // carries the count, cap, and the parent session key.
    let err = supervisor
        .try_register_with_input("tts", "call-overflow", Some(parent_session), None)
        .expect_err("201st child must be refused");
    match err {
        RegisterTaskError::ChildFanoutExceeded {
            parent_session_key,
            count,
            cap,
        } => {
            assert_eq!(parent_session_key, parent_session);
            assert_eq!(count, MAX_CHILDREN_PER_PARENT);
            assert_eq!(cap, MAX_CHILDREN_PER_PARENT);
        }
        other => panic!("expected ChildFanoutExceeded, got {other:?}"),
    }

    // The cap rejection must not leak a new task into the
    // supervisor — count stays at the cap.
    assert_eq!(
        supervisor.get_tasks_for_session(parent_session).len(),
        MAX_CHILDREN_PER_PARENT,
        "refused register must not insert a new task"
    );

    // Every still-active child of the runaway parent should have
    // been force-marked `Failed` with the structured reason so the
    // cascade collapses instead of waiting on each child to finish.
    let expected_reason =
        format!("child fanout exceeded ({MAX_CHILDREN_PER_PARENT} of {MAX_CHILDREN_PER_PARENT})");
    let tasks = supervisor.get_tasks_for_session(parent_session);
    let any_active = tasks.iter().any(|t| t.status.is_active());
    assert!(
        !any_active,
        "every active child should be flipped to Failed after the cap fires"
    );
    let failed_with_reason = tasks
        .iter()
        .filter(|t| {
            t.status == TaskStatus::Failed && t.error.as_deref() == Some(expected_reason.as_str())
        })
        .count();
    assert!(
        failed_with_reason > 0,
        "at least one child should carry the structured fan-out reason"
    );

    // A subsequent attempt against the same poisoned parent must
    // continue to be refused (fast-path via `poisoned_parents`).
    let err = supervisor
        .try_register_with_input("tts", "call-after-overflow", Some(parent_session), None)
        .expect_err("poisoned parent must keep refusing further registers");
    assert!(matches!(err, RegisterTaskError::ChildFanoutExceeded { .. }));

    // A fresh, distinct parent session is unaffected.
    let other = supervisor
        .try_register_with_input("tts", "call-other-1", Some("api:other-parent"), None)
        .expect("other parents stay unaffected by a poisoned peer");
    assert!(!other.is_empty());
}

/// #2056 — the restore observer fires ONCE per restore, with the table in its
/// FINAL post-sweep state, and never for a re-enable that restores nothing.
/// Consumers that mirror task state elsewhere (the octos-cli goal ledger) use
/// it to notice terminal transitions the previous process never delivered, so
/// a snapshot taken before the orphan sweep would hand them a row that is
/// about to change and a repeat firing would re-drive work already done.
#[test]
fn should_fire_on_restore_once_with_the_swept_table_when_persistence_is_enabled() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let writer = TaskSupervisor::new();
    writer.enable_persistence(&ledger_path).unwrap();
    let orphan = writer.register("search", "call-orphan", Some("api:session"));
    writer.mark_running(&orphan);
    let finished = writer.register("fm_tts", "call-done", Some("api:session"));
    writer.mark_completed(&finished, vec![]);
    drop(writer);

    type RestoredRows = Vec<(String, TaskStatus)>;
    let observed: Arc<Mutex<Vec<RestoredRows>>> = Arc::new(Mutex::new(Vec::new()));
    let restored = TaskSupervisor::new();
    let sink = Arc::clone(&observed);
    restored.set_on_restore(move |tasks| {
        let mut rows: RestoredRows = tasks
            .iter()
            .map(|task| (task.id.clone(), task.status.clone()))
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        sink.lock().unwrap().push(rows);
    });
    restored.enable_persistence(&ledger_path).unwrap();

    let calls = observed.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "exactly one firing per restore");
    let rows: std::collections::HashMap<String, TaskStatus> = calls[0].iter().cloned().collect();
    assert_eq!(
        rows.get(&orphan),
        Some(&TaskStatus::Failed),
        "the observer sees the table AFTER the orphan sweep, not before it",
    );
    assert_eq!(rows.get(&finished), Some(&TaskStatus::Completed));

    // Re-enabling the SAME path restores nothing (the idempotence guard) and
    // must not re-fire.
    restored.enable_persistence(&ledger_path).unwrap();
    assert_eq!(
        observed.lock().unwrap().len(),
        1,
        "a no-op re-enable must not re-fire the restore observer",
    );
}

#[test]
fn enable_persistence_reaps_orphan_running_tasks_at_startup() {
    // The bug: when the runtime crashes mid-task, the JSONL ledger has a
    // non-terminal entry for the in-flight task (Running / ResolvingOutputs
    // / etc) but no Completed/Failed event. On restart, the supervisor
    // restored that state verbatim — leaving the task forever
    // non-terminal because no live worker is backing it anymore.
    //
    // The fix: after replay, any task whose runtime_state is non-terminal
    // is reaped — marked Failed("orphaned across restart") — so callers
    // observing the supervisor see a clean state.

    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    // Phase 1: simulate a previous run that registered two tasks. Task A
    // is left mid-flight (Running). Task B reached terminal Completed.
    let supervisor = TaskSupervisor::new();
    supervisor.enable_persistence(&ledger_path).unwrap();
    let task_a = supervisor.register_with_lineage("search", "call-a", Some("api:session"), None);
    supervisor.mark_running(&task_a);
    let task_b = supervisor.register_with_lineage("fm_tts", "call-b", Some("api:session"), None);
    supervisor.mark_completed(&task_b, vec!["/tmp/voice.mp3".to_string()]);
    // Drop the first supervisor — its in-flight worker for task_a is gone.
    drop(supervisor);

    // Phase 2: a fresh supervisor replays the ledger and must reap the
    // orphaned non-terminal task.
    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();

    let reaped = restored
        .get_task(&task_a)
        .expect("orphan task must still be tracked after reap");
    assert_eq!(
        reaped.status,
        TaskStatus::Failed,
        "non-peer orphan keeps the genuine-Failed verdict (#27c scope)"
    );
    assert_eq!(reaped.runtime_state, TaskRuntimeState::Failed);
    let error = reaped.error.as_deref().unwrap_or("");
    assert!(
        error.contains("orphaned") || error.contains("restart"),
        "orphan task error must mention orphan/restart, got {error:?}"
    );
    assert!(
        reaped.completed_at.is_some(),
        "a genuine-Failed orphan carries the terminal completed_at timestamp"
    );

    let surviving = restored
        .get_task(&task_b)
        .expect("completed task must still be tracked after reap");
    assert_eq!(
        surviving.status,
        TaskStatus::Completed,
        "terminal tasks must not be reaped"
    );
    assert_eq!(surviving.runtime_state, TaskRuntimeState::Completed);

    // Idempotency: a third supervisor replaying the same ledger must see
    // task_a still Parked (#27c — the sweep appended a Parked event, which
    // replays as Parked; a Parked task has no live worker in ANY process,
    // so re-sweeping is idempotent and leaves it re-attachable).
    let restored_again = TaskSupervisor::new();
    restored_again.enable_persistence(&ledger_path).unwrap();
    let reread = restored_again
        .get_task(&task_a)
        .expect("orphan task still tracked on second replay");
    assert_eq!(reread.status, TaskStatus::Failed);
    let reread_error = reread.error.as_deref().unwrap_or("");
    assert!(
        reread_error.contains("orphaned") || reread_error.contains("restart"),
        "orphan task error must persist across replay, got {reread_error:?}"
    );
    // The completed task is unaffected on replay.
    let reread_b = restored_again
        .get_task(&task_b)
        .expect("completed task still tracked on second replay");
    assert_eq!(reread_b.status, TaskStatus::Completed);

    // Cancelled tasks must also be respected as terminal — they should
    // not be reaped a second time. Add a cancelled task to the ledger,
    // reload, and assert the cancellation survives.
    let cancel_supervisor = restored_again;
    let task_c =
        cancel_supervisor.register_with_lineage("bg_research", "call-c", Some("api:session"), None);
    cancel_supervisor.mark_running(&task_c);
    cancel_supervisor
        .cancel(&task_c)
        .expect("cancel should succeed");
    drop(cancel_supervisor);
    let final_reload = TaskSupervisor::new();
    final_reload.enable_persistence(&ledger_path).unwrap();
    let cancelled = final_reload
        .get_task(&task_c)
        .expect("cancelled task still tracked after reload");
    assert_eq!(
        cancelled.status,
        TaskStatus::Cancelled,
        "cancelled tasks must not be reaped"
    );
    assert_eq!(cancelled.runtime_state, TaskRuntimeState::Cancelled);
}

/// STEP 2: a guard armed after `mark_running` and dropped WITHOUT a
/// terminal call drives the task to `Failed` with the dropped-worker
/// reason.
#[test]
fn terminal_guard_marks_failed_when_dropped_while_active() {
    use std::sync::Arc;

    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("mofa_slides", "call-guard", Some("api:session"));
    supervisor.mark_running(&id);

    {
        let _guard = TaskTerminalGuard::new(Arc::clone(&supervisor), id.clone());
        // No terminal call inside the scope — simulate an aborted body.
    }

    let task = supervisor.get_task(&id).expect("task");
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(
        task.error.as_deref(),
        Some("worker dropped before reaching terminal state"),
    );
}

// ── Orphan-sweep liveness gate (fix/orphan-sweep-liveness-gate) ──
//
// The WS turn path rebuilds a BRAND-NEW per-turn `TaskSupervisor`
// every turn and calls `enable_persistence(...)` over the SHARED
// per-session ledger. `enable_persistence`'s orphan-sweep ASSUMES
// "non-terminal ⇒ no live worker", so it FALSELY marks a still-Running
// DETACHED spawn_only task (a detached bg_research run, up to ~3600s)
// as "orphaned across restart" — even though the worker is alive on the
// PREVIOUS turn's supervisor and will mark_completed shortly. The fix
// gates the sweep on a process-global live-set that survives the
// per-turn supervisor rebuild and is empty after a true cross-process
// restart.

/// RED→GREEN: a task in the process-global live-set + a `Running` row in
/// the shared ledger must NOT be swept as "orphaned across restart" by a
/// NEW supervisor's `enable_persistence`. Mirrors the real bug: turn N's
/// detached worker is alive (id in live-set) when turn N+1's fresh
/// supervisor opens the same ledger.
#[test]
fn live_detached_task_is_not_swept_as_orphan() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    // Turn N: supervisor registers + runs a detached spawn_only task and
    // persists a still-Running row.
    let turn_n = TaskSupervisor::new();
    turn_n.enable_persistence(&ledger_path).unwrap();
    let id = turn_n.register("bg_research", "call-live-1", Some("api:sess"));
    turn_n.mark_running(&id);

    // The detached worker is alive: its id is in the process-global
    // live-set (in production the TaskTerminalGuard inserts it).
    mark_task_live(&id);
    // RAII clear at scope end so the global set does not leak across tests.
    struct ClearOnDrop<'a>(&'a str);
    impl Drop for ClearOnDrop<'_> {
        fn drop(&mut self) {
            clear_task_live(self.0);
        }
    }
    let _clear = ClearOnDrop(&id);

    // Turn N+1: a BRAND-NEW supervisor opens the SAME ledger. Pre-fix this
    // sweep marks the still-Running row "orphaned across restart".
    let turn_n1 = TaskSupervisor::new();
    turn_n1.enable_persistence(&ledger_path).unwrap();

    let restored = turn_n1.get_task(&id).expect("row restored");
    assert_eq!(
        restored.status,
        TaskStatus::Running,
        "a LIVE detached task (id in live-set) must NOT be swept as orphan",
    );
    assert_ne!(
        restored.error.as_deref(),
        Some("orphaned across restart"),
        "live detached task must never carry the false-orphan reason",
    );
}
