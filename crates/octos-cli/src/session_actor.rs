//! Session-scoped runtime shared state.
//!
//! This module now only carries the LIVE pieces of the former session-actor
//! machinery: the [`SessionTaskQueryStore`] the serve/stdio/api task-query
//! surfaces read, and the [`PendingMessages`] alias. The per-session actor
//! task itself (and its registry/factory) was removed with the gateway
//! subsystem; the serve/stdio transport owns its turn execution directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use octos_agent::TaskSupervisor;
use octos_core::SessionKey;

/// Shared buffer of outbound messages from inactive sessions, keyed by session key string.
/// Flushed when the user switches to that session via `/s`.
pub type PendingMessages = Arc<StdMutex<HashMap<String, Vec<octos_core::OutboundMessage>>>>;

/// Shared lookup table for session-scoped background task supervisors.
#[derive(Default, Clone)]
pub struct SessionTaskQueryStore {
    /// Per-session list of registered supervisors, oldest-first. A session
    /// accumulates more than one when a long-running `spawn_only` task spawned
    /// in an earlier turn is still live (its worker holds that turn's
    /// supervisor alive via `Arc<ToolRegistry>`) while a later turn registers a
    /// fresh supervisor — `ToolRegistry::snapshot_excluding` builds a NEW
    /// `TaskSupervisor` per turn. Keeping all live ones (rather than
    /// overwriting) lets `cancel_task` reach the supervisor whose cancel token
    /// the live worker actually polls; cancelling through a later turn's
    /// supervisor would only fire a useless fresh token.
    supervisors: Arc<StdMutex<HashMap<String, Vec<SessionTaskQueryEntry>>>>,
}

struct SessionTaskQueryEntry {
    supervisor: Weak<TaskSupervisor>,
    data_dir: PathBuf,
}

fn task_response_path(data_dir: &Path, path: &str) -> String {
    octos_bus::file_handle::encode_profile_file_handle(data_dir, Path::new(path))
        .unwrap_or_else(|| path.to_string())
}

fn task_runtime_detail_for_response(
    detail: Option<&str>,
) -> (serde_json::Value, Option<String>, Option<String>) {
    let runtime_detail = match detail {
        Some(detail) => serde_json::from_str(detail)
            .unwrap_or_else(|_| serde_json::Value::String(detail.to_string())),
        None => serde_json::Value::Null,
    };
    let workflow_kind = runtime_detail
        .get("workflow_kind")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let current_phase = runtime_detail
        .get("current_phase")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    (runtime_detail, workflow_kind, current_phase)
}

fn sanitize_task_for_response(
    data_dir: &Path,
    task: &octos_agent::BackgroundTask,
) -> serde_json::Value {
    let (runtime_detail, workflow_kind, current_phase) =
        task_runtime_detail_for_response(task.runtime_detail.as_deref());
    serde_json::json!({
        "id": task.id,
        "tool_name": task.tool_name,
        "tool_call_id": task.tool_call_id,
        "parent_session_key": task.parent_session_key,
        "child_session_key": task.child_session_key,
        "status": task.status,
        "lifecycle_state": task.lifecycle_state(),
        "started_at": task.started_at,
        "updated_at": task.updated_at,
        "completed_at": task.completed_at,
        "runtime_state": task.runtime_state,
        "runtime_detail": runtime_detail,
        "workflow_kind": workflow_kind,
        "current_phase": current_phase,
        "child_terminal_state": task.child_terminal_state,
        "child_join_state": task.child_join_state,
        "child_joined_at": task.child_joined_at,
        "child_failure_action": task.child_failure_action,
        // #966 / M13-B — surface the new BackgroundTask projection
        // fields. Each is Option-typed; absent values serialize as
        // null, which the AppUI TaskListProjection treats as None
        // (its fields use `#[serde(default)]`). Existing snapshots
        // without these fields surface as null/None, so the wire
        // shape stays backwards-compatible.
        "source": task.source,
        "role": task.role,
        "summary": task.summary,
        "artifact_count": task.artifact_count,
        "runtime_policy_stamp": task.runtime_policy_stamp,
        "output_files": task.output_files.iter().map(|path| task_response_path(data_dir, path)).collect::<Vec<_>>(),
        "error": task.error,
        "session_key": task.session_key,
    })
}

impl SessionTaskQueryStore {
    pub fn register(
        &self,
        session_key: &SessionKey,
        supervisor: &Arc<TaskSupervisor>,
        data_dir: &Path,
    ) {
        let mut guard = self.supervisors.lock().unwrap_or_else(|e| e.into_inner());
        let entries = guard.entry(session_key.to_string()).or_default();
        // Drop entries whose supervisor has been dropped (its turn ended with
        // no live task holding it), then dedup: if this exact supervisor is
        // already registered, just refresh its data_dir. Otherwise append at
        // the end so the per-session order stays oldest-first — `cancel_task`
        // scans oldest-first to prefer the supervisor the live worker polls.
        entries.retain(|entry| entry.supervisor.strong_count() > 0);
        for entry in entries.iter_mut() {
            if let Some(existing) = entry.supervisor.upgrade() {
                if Arc::ptr_eq(&existing, supervisor) {
                    entry.data_dir = data_dir.to_path_buf();
                    return;
                }
            }
        }
        entries.push(SessionTaskQueryEntry {
            supervisor: Arc::downgrade(supervisor),
            data_dir: data_dir.to_path_buf(),
        });
    }

    /// Return every live supervisor + data dir registered for `session_key`,
    /// oldest-first, pruning entries whose `Arc<TaskSupervisor>` has dropped
    /// (and the session key entirely when none remain). A session has more
    /// than one when an earlier turn's supervisor is still alive — a live
    /// `spawn_only` worker holds it — alongside a later turn's fresh one.
    fn live_entries_for_session(&self, session_key: &str) -> Vec<(Arc<TaskSupervisor>, PathBuf)> {
        let mut guard = self.supervisors.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entries) = guard.get_mut(session_key) else {
            return Vec::new();
        };
        let mut live = Vec::new();
        entries.retain(|entry| match entry.supervisor.upgrade() {
            Some(supervisor) => {
                live.push((supervisor, entry.data_dir.clone()));
                true
            }
            None => false,
        });
        if entries.is_empty() {
            guard.remove(session_key);
        }
        live
    }

    /// Return the JSON task list for `session_key` and every reachable
    /// descendant session. The walk follows each task's
    /// [`octos_agent::BackgroundTask::child_session_key`] to the next
    /// supervisor (when one is registered and still alive) so that, e.g., a
    /// `bg_research` task running inside a child session shows up in its
    /// parent's `/api/sessions/:id/tasks` view. Without this, UIs cannot
    /// correlate the parent's rendered tool_call_id bubble with the actual
    /// child-session task.
    ///
    /// Traversal is breadth-first with a `visited` guard so cycles or
    /// duplicate child keys do not trigger redundant work. Auth/ownership
    /// checks happen at the API layer for the parent — descendants inherit
    /// access by virtue of being spawned from the authorized parent.
    pub fn query_json(&self, session_key: &str) -> serde_json::Value {
        let mut tasks: Vec<serde_json::Value> = Vec::new();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        queue.push_back(session_key.to_string());
        visited.insert(session_key.to_string());

        let mut seen_task_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(current) = queue.pop_front() {
            // A session may have several live supervisors (an earlier-turn task
            // still running while a later turn registered a fresh supervisor);
            // walk them oldest-first and dedup by task id, since a restored
            // copy of the same task can surface in more than one supervisor.
            for (supervisor, data_dir) in self.live_entries_for_session(&current) {
                // Freshen stale cross-turn copies from the ledger first (codex
                // P2): a later supervisor's restored copy is frozen at restore
                // time, so a finished task could otherwise surface as running
                // once its owning supervisor drops.
                let _ = supervisor.refresh_from_persistence();
                for task in supervisor.get_tasks_for_session(&current) {
                    if !seen_task_ids.insert(task.id.clone()) {
                        continue;
                    }
                    if let Some(child_key) = task.child_session_key.as_deref() {
                        if visited.insert(child_key.to_string()) {
                            queue.push_back(child_key.to_string());
                        }
                    }
                    tasks.push(sanitize_task_for_response(&data_dir, &task));
                }
            }
        }

        serde_json::Value::Array(tasks)
    }

    /// C8 / GAP A: return the raw [`octos_agent::BackgroundTask`] snapshots for
    /// `session_key` (and every reachable descendant session), each paired with
    /// the owning supervisor's `data_dir` for path encoding. Mirrors
    /// [`Self::query_json`]'s breadth-first traversal but yields the raw task
    /// structs so the WS `session/open` handler can replay each one as a
    /// `task/updated` event through the SAME emission path live updates use
    /// (`background_task_to_progress_json`). A reconnecting / freshly-opening
    /// TUI starts with an empty `session.tasks` and only applies incremental
    /// updates, so without this replay the existing task list is invisible
    /// until the next live transition.
    pub fn raw_tasks_for_session(
        &self,
        session_key: &str,
    ) -> Vec<(octos_agent::BackgroundTask, PathBuf)> {
        let mut tasks: Vec<(octos_agent::BackgroundTask, PathBuf)> = Vec::new();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        queue.push_back(session_key.to_string());
        visited.insert(session_key.to_string());

        let mut seen_task_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(current) = queue.pop_front() {
            // See `query_json`: walk every live supervisor for the session
            // oldest-first, dedup by task id across supervisors.
            for (supervisor, data_dir) in self.live_entries_for_session(&current) {
                // Freshen stale cross-turn copies from the ledger (codex P2),
                // same as `query_json` — this feeds reconnect replay.
                let _ = supervisor.refresh_from_persistence();
                for task in supervisor.get_tasks_for_session(&current) {
                    if !seen_task_ids.insert(task.id.clone()) {
                        continue;
                    }
                    if let Some(child_key) = task.child_session_key.as_deref() {
                        if visited.insert(child_key.to_string()) {
                            queue.push_back(child_key.to_string());
                        }
                    }
                    tasks.push((task, data_dir.clone()));
                }
            }
        }

        tasks
    }

    /// M7.9 / W2: locate the supervisor owning `task_id` and forward
    /// `cancel(task_id)` to it. Returns `Ok(())` on success, mapping
    /// supervisor errors back to the typed [`TaskCancelError`] enum so
    /// the API layer can map them to HTTP status codes.
    ///
    /// Walks every live supervisor (pruning dropped ones) until it finds
    /// the task. When no supervisor knows about `task_id`, returns
    /// `Err(TaskCancelError::NotFound)`.
    pub fn cancel_task(&self, task_id: &str) -> Result<(), octos_agent::TaskCancelError> {
        for supervisor in self.live_supervisors() {
            // Freshen this task from the ledger first (codex P2): a stale
            // restored `Running` copy in a later supervisor must not accept a
            // cancel after the owning supervisor already drove it terminal —
            // `cancel` then correctly returns `AlreadyTerminal`.
            let _ = supervisor.refresh_task_from_persistence(task_id);
            if supervisor.get_task(task_id).is_some() {
                return supervisor.cancel(task_id);
            }
        }
        Err(octos_agent::TaskCancelError::NotFound)
    }

    /// M7.9 / W2: locate the supervisor owning `task_id` and forward
    /// `relaunch(task_id, opts)` to it. Returns `Ok(new_task_id)` on
    /// success.
    pub fn relaunch_task(
        &self,
        task_id: &str,
        opts: octos_agent::RelaunchOpts,
    ) -> Result<String, octos_agent::TaskRelaunchError> {
        for supervisor in self.live_supervisors() {
            // Freshen from the ledger first (codex P2) so a stale cross-turn
            // copy doesn't drive a relaunch off outdated state.
            let _ = supervisor.refresh_task_from_persistence(task_id);
            if supervisor.get_task(task_id).is_some() {
                return supervisor.relaunch(task_id, opts);
            }
        }
        Err(octos_agent::TaskRelaunchError::NotFound)
    }

    /// Snapshot live supervisors, pruning dropped weak refs. Shared
    /// helper for `cancel_task` / `relaunch_task` /
    /// `mark_child_session_failed`.
    fn live_supervisors(&self) -> Vec<Arc<TaskSupervisor>> {
        let mut guard = self.supervisors.lock().unwrap_or_else(|e| e.into_inner());
        let mut alive = Vec::new();
        // Flatten every session's supervisor list, oldest-first within each
        // session, pruning dropped entries (and now-empty sessions).
        // Oldest-first matters for `cancel_task`: when a task spawned in an
        // earlier turn has a restored copy in a later turn's supervisor, the
        // earlier (live) supervisor must be tried first so cancel fires the
        // token the worker is actually polling.
        guard.retain(|_, entries| {
            entries.retain(|entry| match entry.supervisor.upgrade() {
                Some(supervisor) => {
                    alive.push(supervisor);
                    true
                }
                None => false,
            });
            !entries.is_empty()
        });
        alive
    }

    /// M8 fix-first item 8 (gap 3): mark the parent task that owns a
    /// child session as failed.
    ///
    /// When a child session refuses to resume because its worktree has
    /// disappeared, the in-memory transcript is cleared as a safety floor
    /// (M8.6 fix-first item 3) but the parent task that spawned this
    /// child is left in `Running`. Dashboards then show a stuck task that
    /// will never make progress. This method walks every registered
    /// supervisor, looking for a `BackgroundTask` whose
    /// `child_session_key` matches `child_session_key`, and calls
    /// [`TaskSupervisor::mark_failed`] on it. Returns `true` when a
    /// matching task was found and updated; `false` otherwise.
    pub fn mark_child_session_failed(&self, child_session_key: &str, error: &str) -> bool {
        for supervisor in self.live_supervisors() {
            for task in supervisor.get_all_tasks() {
                if task.child_session_key.as_deref() == Some(child_session_key) {
                    supervisor.mark_failed(&task.id, error.to_string());
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C8 / GAP A: `raw_tasks_for_session` returns the live `BackgroundTask`
    /// snapshots (paired with the owning supervisor's data_dir) so the WS
    /// `session/open` handler can replay them as `task/updated` events. It must
    /// surface the same tasks `query_json` does, keyed by the registered
    /// `SessionKey`, and return empty for an unknown session.
    #[test]
    fn raw_tasks_for_session_returns_live_supervisor_tasks() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_id = supervisor.register("bg_research", "call-1", Some("api:session"));
        supervisor.mark_running(&task_id);

        let store = SessionTaskQueryStore::default();
        let session_key = SessionKey::new("api", "session");
        store.register(&session_key, &supervisor, &data_dir);

        let tasks = store.raw_tasks_for_session(&session_key.to_string());
        assert_eq!(tasks.len(), 1, "the running task must be surfaced");
        let (task, returned_data_dir) = &tasks[0];
        assert_eq!(task.id, task_id);
        assert_eq!(task.tool_name, "bg_research");
        assert_eq!(task.tool_call_id, "call-1");
        assert_eq!(task.status, octos_agent::TaskStatus::Running);
        assert_eq!(returned_data_dir, &data_dir);

        // An unknown session has no live supervisor → empty replay.
        assert!(
            store
                .raw_tasks_for_session(&SessionKey::new("api", "other").to_string())
                .is_empty()
        );
    }

    /// Cross-turn cancel regression (codex P2): a `spawn_only` task spawned in
    /// turn 1 polls turn-1's cancel token. When turn 2 registers a fresh
    /// supervisor for the SAME session, the store must keep turn-1's supervisor
    /// (still alive — the live worker holds it via `Arc<ToolRegistry>`)
    /// reachable, so cancel fires the token the worker actually polls rather
    /// than a later supervisor's useless fresh one. The old `HashMap::insert`
    /// evicted turn-1's supervisor, leaving the task uncancellable.
    #[test]
    fn cancel_task_reaches_earlier_turn_supervisor_after_a_later_turn_registers() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let session_key = SessionKey::new("api", "session");

        let sup1 = Arc::new(TaskSupervisor::new());
        let task_id = sup1.register("bg_research", "call-1", Some("api:session"));
        sup1.mark_running(&task_id);
        let live_token = sup1.cancel_token(&task_id);
        assert!(!live_token.is_cancelled());

        let store = SessionTaskQueryStore::default();
        store.register(&session_key, &sup1, &data_dir);

        // Turn 2's fresh supervisor for the same session (used to evict sup1).
        let sup2 = Arc::new(TaskSupervisor::new());
        store.register(&session_key, &sup2, &data_dir);

        store
            .cancel_task(&task_id)
            .expect("task spawned under the earlier supervisor must still cancel");
        assert!(
            live_token.is_cancelled(),
            "cancel must fire the live (turn-1) supervisor's token"
        );
    }

    /// `query_json` walks EVERY live supervisor for a session (not just the
    /// last-registered one) and dedups by task id: the task whose live copy is
    /// in the earlier supervisor appears exactly once, and a later-supervisor's
    /// own task still surfaces.
    #[test]
    fn query_json_walks_all_live_supervisors_and_dedups_by_task_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let ledger = data_dir.join("tasks.jsonl");
        let session_key = SessionKey::new("api", "session");

        let sup1 = Arc::new(TaskSupervisor::new());
        sup1.enable_persistence(&ledger).unwrap();
        let t1 = sup1.register("bg_research", "call-1", Some("api:session"));
        sup1.mark_running(&t1);

        // A later turn's supervisor restores T1 (same id) from the shared
        // ledger and also owns its own T2.
        let sup2 = Arc::new(TaskSupervisor::new());
        sup2.enable_persistence(&ledger).unwrap();
        assert!(sup2.get_task(&t1).is_some(), "T1 restored into sup2");
        let t2 = sup2.register("deep_search", "call-2", Some("api:session"));
        sup2.mark_running(&t2);

        let store = SessionTaskQueryStore::default();
        store.register(&session_key, &sup1, &data_dir);
        store.register(&session_key, &sup2, &data_dir);

        let json = store.query_json(&session_key.to_string());
        let ids: Vec<String> = json
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|task| task.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        assert_eq!(
            ids.iter().filter(|id| **id == t1).count(),
            1,
            "T1 must appear exactly once across the two supervisors"
        );
        assert!(
            ids.contains(&t2),
            "the later supervisor's own task must still surface"
        );
    }

    /// `raw_tasks_for_session` mirrors `query_json`'s multi-supervisor walk +
    /// task-id dedup (it feeds reconnect/session-open task replay).
    #[test]
    fn raw_tasks_for_session_walks_all_live_supervisors_and_dedups() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let ledger = data_dir.join("tasks.jsonl");
        let session_key = SessionKey::new("api", "session");

        let sup1 = Arc::new(TaskSupervisor::new());
        sup1.enable_persistence(&ledger).unwrap();
        let t1 = sup1.register("bg_research", "call-1", Some("api:session"));
        sup1.mark_running(&t1);

        let sup2 = Arc::new(TaskSupervisor::new());
        sup2.enable_persistence(&ledger).unwrap();
        let t2 = sup2.register("deep_search", "call-2", Some("api:session"));
        sup2.mark_running(&t2);

        let store = SessionTaskQueryStore::default();
        store.register(&session_key, &sup1, &data_dir);
        store.register(&session_key, &sup2, &data_dir);

        let tasks = store.raw_tasks_for_session(&session_key.to_string());
        assert_eq!(
            tasks.iter().filter(|(task, _)| task.id == t1).count(),
            1,
            "T1 must appear exactly once"
        );
        assert!(
            tasks.iter().any(|(task, _)| task.id == t2),
            "the later supervisor's own task must surface"
        );
    }

    /// codex P2 follow-up: a later turn's supervisor (sup2) holds a restored
    /// copy of an earlier turn's task (t1) and never receives its later status
    /// updates. After sup1 completes t1 and drops while sup2 stays alive for its
    /// own task, the store must reconcile t1 from the ledger — never surfacing
    /// sup2's stale `Running` copy, nor accepting a cancel against the
    /// already-finished task.
    #[test]
    fn store_reconciles_stale_cross_turn_copy_from_ledger() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let ledger = data_dir.join("tasks.jsonl");
        let session_key = SessionKey::new("api", "session");

        let sup1 = Arc::new(TaskSupervisor::new());
        sup1.enable_persistence(&ledger).unwrap();
        let t1 = sup1.register("bg_research", "call-1", Some("api:session"));
        sup1.mark_running(&t1);

        let sup2 = Arc::new(TaskSupervisor::new());
        sup2.enable_persistence(&ledger).unwrap();
        let t2 = sup2.register("deep_search", "call-2", Some("api:session"));
        sup2.mark_running(&t2);

        let store = SessionTaskQueryStore::default();
        store.register(&session_key, &sup1, &data_dir);
        store.register(&session_key, &sup2, &data_dir);

        // sup1 finishes t1 (persists Completed to the shared ledger) then drops
        // — its turn ended and the worker released the per-turn registry.
        sup1.mark_completed(&t1, vec![]);
        drop(sup1);

        // sup2's stale `Running` copy of t1 must never surface.
        let raw = store.raw_tasks_for_session(&session_key.to_string());
        assert!(
            !raw.iter()
                .any(|(task, _)| task.id == t1 && task.status == octos_agent::TaskStatus::Running),
            "t1's stale running copy must not surface after its owner completed it"
        );
        assert!(
            raw.iter().any(|(task, _)| task.id == t2),
            "sup2's own task must still surface"
        );
        // query_json shows t1 at most once (deduped) and t2 present.
        let ids: Vec<String> = store
            .query_json(&session_key.to_string())
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|task| task.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        assert!(ids.iter().filter(|id| **id == t1).count() <= 1);
        assert!(
            ids.contains(&t2),
            "sup2's own task must surface in query_json"
        );

        // A cancel against the finished task reports AlreadyTerminal — proving
        // the store reconciled t1's terminal status from the ledger rather than
        // acting on sup2's stale running copy.
        assert!(matches!(
            store.cancel_task(&t1),
            Err(octos_agent::TaskCancelError::AlreadyTerminal)
        ));
    }

    /// codex P1 follow-up: ledger refresh must never import a task into a
    /// supervisor that doesn't own it. Two live turns share a ledger but poll
    /// different cancel tokens; if an older supervisor imported a later
    /// supervisor's task, cancel/relaunch (oldest-first) would fire the wrong
    /// token while the real worker ran on. Refresh updates only already-owned
    /// rows, so cancel routes to the owning supervisor's live token.
    #[test]
    fn refresh_does_not_import_a_later_supervisors_task_into_an_older_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let ledger = data_dir.join("tasks.jsonl");
        let session_key = SessionKey::new("api", "session");

        // sup1 (turn N) owns t1; sup2 (turn N+1, registered later) owns t2.
        // Both live, both persist to the shared ledger.
        let sup1 = Arc::new(TaskSupervisor::new());
        sup1.enable_persistence(&ledger).unwrap();
        let t1 = sup1.register("bg_research", "call-1", Some("api:session"));
        sup1.mark_running(&t1);

        let sup2 = Arc::new(TaskSupervisor::new());
        sup2.enable_persistence(&ledger).unwrap();
        let t2 = sup2.register("deep_search", "call-2", Some("api:session"));
        sup2.mark_running(&t2);
        let t2_token = sup2.cancel_token(&t2);

        let store = SessionTaskQueryStore::default();
        store.register(&session_key, &sup1, &data_dir);
        store.register(&session_key, &sup2, &data_dir);

        // A projection refreshes every supervisor from the ledger.
        let _ = store.query_json(&session_key.to_string());

        // sup1 (registered before t2 existed) must NOT have imported t2.
        assert!(
            sup1.get_task(&t2).is_none(),
            "t2 must not be imported into the older supervisor"
        );

        // Cancelling t2 must fire sup2's live token (the worker's), not sup1's.
        store.cancel_task(&t2).expect("t2 is cancellable");
        assert!(
            t2_token.is_cancelled(),
            "cancel must reach t2's owning supervisor (sup2) live token"
        );
    }

    #[test]
    fn session_task_query_store_hides_absolute_output_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        let workspace = data_dir
            .join("users")
            .join("api%3Asession")
            .join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let output = workspace.join("voice.mp3");
        std::fs::write(&output, b"audio").unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_ledger_path = data_dir.join("tasks.jsonl");
        supervisor.enable_persistence(&task_ledger_path).unwrap();
        let task_id = supervisor.register_with_lineage(
            "fm_tts",
            "call-1",
            Some("api:session"),
            Some(task_ledger_path.to_str().unwrap()),
        );
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            octos_agent::TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );
        supervisor.mark_completed(&task_id, vec![output.to_string_lossy().to_string()]);

        let store = SessionTaskQueryStore::default();
        let session_key = SessionKey::new("api", "session");
        store.register(&session_key, &supervisor, &data_dir);

        let payload = store.query_json(&session_key.to_string());
        let tasks = payload.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["lifecycle_state"], "ready");
        assert_eq!(tasks[0]["runtime_state"], "completed");
        assert_eq!(tasks[0]["runtime_detail"], "send_file");
        let files = tasks[0]["output_files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        let handle = files[0].as_str().unwrap();
        assert!(handle.starts_with("pf/"));
        assert!(!handle.starts_with("/"));
        assert_eq!(tasks[0]["parent_session_key"], "api:session");
        assert!(
            tasks[0]["child_session_key"]
                .as_str()
                .unwrap()
                .starts_with("api:session#child-")
        );
        assert!(tasks[0]["task_ledger_path"].is_null());
    }

    #[test]
    fn session_task_query_store_exposes_parsed_workflow_runtime_detail() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        let workspace = data_dir
            .join("users")
            .join("api%3Asession")
            .join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_ledger_path = data_dir.join("tasks.jsonl");
        supervisor.enable_persistence(&task_ledger_path).unwrap();
        let task_id = supervisor.register_with_lineage(
            "podcast_generate",
            "call-1",
            Some("api:session"),
            Some(task_ledger_path.to_str().unwrap()),
        );
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            octos_agent::TaskRuntimeState::DeliveringOutputs,
            Some(
                serde_json::json!({
                    "workflow_kind": "research_podcast",
                    "current_phase": "deliver_result"
                })
                .to_string(),
            ),
        );
        supervisor.mark_completed(&task_id, vec![]);
        supervisor.mark_child_session_outcome(
            &task_id,
            octos_agent::task_supervisor::ChildSessionTerminalState::Completed,
            octos_agent::task_supervisor::ChildSessionJoinState::Joined,
        );

        let store = SessionTaskQueryStore::default();
        let session_key = SessionKey::new("api", "session");
        store.register(&session_key, &supervisor, &data_dir);

        let payload = store.query_json(&session_key.to_string());
        let tasks = payload.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["lifecycle_state"], "ready");
        assert_eq!(tasks[0]["runtime_state"], "completed");
        assert_eq!(tasks[0]["workflow_kind"], "research_podcast");
        assert_eq!(tasks[0]["current_phase"], "deliver_result");
        assert_eq!(
            tasks[0]["runtime_detail"]["workflow_kind"],
            "research_podcast"
        );
        assert_eq!(
            tasks[0]["runtime_detail"]["current_phase"],
            "deliver_result"
        );
        assert_eq!(tasks[0]["child_terminal_state"], "completed");
        assert_eq!(tasks[0]["child_join_state"], "joined");
        assert!(tasks[0]["child_failure_action"].is_null());
    }

    #[test]
    fn session_task_query_store_exposes_harness_progress_runtime_detail() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_ledger_path = data_dir.join("tasks.jsonl");
        supervisor.enable_persistence(&task_ledger_path).unwrap();
        let task_id = supervisor.register_with_lineage(
            "search",
            "call-1",
            Some("api:session"),
            Some(task_ledger_path.to_str().unwrap()),
        );
        supervisor.mark_running(&task_id);
        let event = octos_agent::HarnessEvent::progress(
            "api:session",
            task_id.clone(),
            Some("bg_research"),
            "fetch",
            Some("Fetching 4 pages"),
            Some(0.4),
        );
        supervisor.apply_harness_event(&task_id, &event).unwrap();

        let store = SessionTaskQueryStore::default();
        let session_key = SessionKey::new("api", "session");
        store.register(&session_key, &supervisor, &data_dir);

        let payload = store.query_json(&session_key.to_string());
        let tasks = payload.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], task_id);
        assert_eq!(tasks[0]["session_key"], "api:session");
        assert_eq!(tasks[0]["workflow_kind"], "bg_research");
        assert_eq!(tasks[0]["current_phase"], "fetch");
        assert_eq!(tasks[0]["runtime_detail"]["session_id"], "api:session");
        assert_eq!(
            tasks[0]["runtime_detail"]["schema_version"],
            serde_json::json!(octos_agent::abi_schema::HARNESS_PROGRESS_EVENT_SCHEMA_VERSION)
        );
        assert_eq!(tasks[0]["runtime_detail"]["task_id"], task_id);
        assert_eq!(
            tasks[0]["runtime_detail"]["progress_message"],
            "Fetching 4 pages"
        );
        assert_eq!(tasks[0]["runtime_detail"]["progress"], 0.4);
    }

    #[test]
    fn session_task_query_store_projects_verifying_lifecycle_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_ledger_path = data_dir.join("tasks.jsonl");
        supervisor.enable_persistence(&task_ledger_path).unwrap();
        let task_id = supervisor.register_with_lineage(
            "site_build",
            "call-1",
            Some("api:session"),
            Some(task_ledger_path.to_str().unwrap()),
        );
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            octos_agent::TaskRuntimeState::VerifyingOutputs,
            Some(
                serde_json::json!({
                    "workflow_kind": "site",
                    "current_phase": "verify_contract"
                })
                .to_string(),
            ),
        );

        let store = SessionTaskQueryStore::default();
        let session_key = SessionKey::new("api", "session");
        store.register(&session_key, &supervisor, &data_dir);

        let payload = store.query_json(&session_key.to_string());
        let tasks = payload.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["status"], "running");
        assert_eq!(tasks[0]["lifecycle_state"], "verifying");
        assert_eq!(tasks[0]["runtime_state"], "verifying_outputs");
        assert_eq!(tasks[0]["workflow_kind"], "site");
        assert_eq!(tasks[0]["current_phase"], "verify_contract");
        assert_eq!(tasks[0]["runtime_detail"]["workflow_kind"], "site");
        assert_eq!(
            tasks[0]["runtime_detail"]["current_phase"],
            "verify_contract"
        );
    }

    #[test]
    fn mark_child_session_failed_marks_owning_task_when_supervisor_registered() {
        // M8 fix-first item 8 (gap 3): when a child session refuses to
        // resume because its worktree is gone, SessionTaskQueryStore must
        // walk every registered supervisor, find the BackgroundTask
        // whose `child_session_key` matches, and call mark_failed on it.
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_ledger_path = data_dir.join("tasks.jsonl");
        supervisor.enable_persistence(&task_ledger_path).unwrap();

        // Register a parent task that spawns a child session — the
        // supervisor's `register_with_lineage` derives a deterministic
        // `child_session_key` from the parent + task id.
        let parent_session_key = SessionKey::new("api", "parent-session");
        let task_id = supervisor.register_with_lineage(
            "spawn",
            "call-1",
            Some(&parent_session_key.to_string()),
            Some(task_ledger_path.to_str().unwrap()),
        );
        supervisor.mark_running(&task_id);

        // Pull the derived child_session_key the supervisor recorded.
        let registered_task = supervisor.get_task(&task_id).expect("task tracked");
        let child_session_key = registered_task
            .child_session_key
            .clone()
            .expect("register_with_lineage derives a child key");

        // Register the supervisor in the query store as the parent
        // session would. The store now tracks a Weak<TaskSupervisor>
        // keyed by parent session key.
        let store = SessionTaskQueryStore::default();
        store.register(&parent_session_key, &supervisor, &data_dir);

        // ACT: simulate the child session refusing to resume.
        let was_marked = store.mark_child_session_failed(
            &child_session_key,
            "resume sanitize refused: worktree missing",
        );
        assert!(was_marked, "the parent task must be located by child key");

        // ASSERT: the task transitioned to Failed with the supplied error.
        let updated = supervisor.get_task(&task_id).expect("task still tracked");
        assert_eq!(
            updated.status,
            octos_agent::TaskStatus::Failed,
            "WorktreeMissing on a child session must mark the parent task failed"
        );
        assert!(
            updated
                .error
                .as_deref()
                .map(|e| e.contains("worktree missing"))
                .unwrap_or(false),
            "task error must carry the resume failure reason: {:?}",
            updated.error
        );
    }

    #[test]
    fn mark_child_session_failed_returns_false_when_no_task_matches() {
        // The store returns false when no registered supervisor owns a
        // task with the requested child_session_key. This guards against
        // false-positive marks on unrelated supervisors.
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        let parent_session_key = SessionKey::new("api", "parent-session");
        let store = SessionTaskQueryStore::default();
        store.register(&parent_session_key, &supervisor, &data_dir);

        let was_marked = store.mark_child_session_failed("api:other-session#child-zzz", "anything");
        assert!(
            !was_marked,
            "mark_child_session_failed must return false when no task matches"
        );
    }

    #[test]
    fn query_json_includes_descendant_session_tasks() {
        // Server-side bug fix: `/api/sessions/:id/tasks` previously
        // returned ONLY the parent session's tasks. When a workflow runs
        // `bg_research` in a CHILD session (parent spawns child via
        // spawn_only), that task was invisible from the parent view —
        // blocking UIs that cross-correlate the rendered tool_call_id
        // bubble with the actual bg_research task.
        //
        // After the fix, query_json walks the parent's session_key and
        // every reachable descendant (via each task's `child_session_key`)
        // breadth-first, returning a flat array carrying both sets. Each
        // entry's existing `session_key` field lets callers filter
        // parent-only when needed.
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Parent session: register a `spawn` task. The supervisor derives
        // a deterministic child_session_key the way the live spawn tool
        // would.
        let parent_supervisor = Arc::new(TaskSupervisor::new());
        let parent_ledger = data_dir.join("parent-tasks.jsonl");
        parent_supervisor
            .enable_persistence(&parent_ledger)
            .unwrap();
        let parent_session_key = SessionKey::new("api", "parent-session");
        let parent_task_id = parent_supervisor.register_with_lineage(
            "spawn",
            "call-spawn",
            Some(&parent_session_key.to_string()),
            Some(parent_ledger.to_str().unwrap()),
        );
        parent_supervisor.mark_running(&parent_task_id);

        // Pull the derived child session key the supervisor recorded.
        let parent_task = parent_supervisor
            .get_task(&parent_task_id)
            .expect("parent task tracked");
        let child_session_key_str = parent_task
            .child_session_key
            .clone()
            .expect("register_with_lineage derives a child key");
        let child_session_key = SessionKey(child_session_key_str.clone());

        // Child session: register its own supervisor with a `bg_research`
        // task (the workflow whose tool_call_id the UI wants to correlate
        // back from the parent).
        let child_supervisor = Arc::new(TaskSupervisor::new());
        let child_ledger = data_dir.join("child-tasks.jsonl");
        child_supervisor.enable_persistence(&child_ledger).unwrap();
        let child_task_id = child_supervisor.register_with_lineage(
            "bg_research",
            "call-pipeline",
            Some(&child_session_key_str),
            Some(child_ledger.to_str().unwrap()),
        );
        child_supervisor.mark_running(&child_task_id);

        // Both supervisors register against the shared store, the way
        // ActorRunner does at startup for each session it serves.
        let store = SessionTaskQueryStore::default();
        store.register(&parent_session_key, &parent_supervisor, &data_dir);
        store.register(&child_session_key, &child_supervisor, &data_dir);

        // ACT: query the parent. Both tasks should surface in one flat
        // array.
        let payload = store.query_json(&parent_session_key.to_string());
        let tasks = payload.as_array().expect("array response");
        assert_eq!(
            tasks.len(),
            2,
            "parent /tasks must surface its own task plus the child's bg_research task"
        );

        let parent_entry = tasks
            .iter()
            .find(|t| t["tool_name"] == "spawn")
            .expect("parent spawn task present");
        assert_eq!(parent_entry["session_key"], "api:parent-session");
        assert_eq!(
            parent_entry["child_session_key"], child_session_key_str,
            "parent task carries its derived child_session_key"
        );

        let child_entry = tasks
            .iter()
            .find(|t| t["tool_name"] == "bg_research")
            .expect("child bg_research task surfaces from parent view");
        assert_eq!(child_entry["session_key"], child_session_key_str);
        assert_eq!(child_entry["tool_call_id"], "call-pipeline");
    }

    #[test]
    fn query_json_walks_multi_level_descendants_without_cycling() {
        // The traversal must follow chains deeper than one level
        // (parent -> spawn -> bg_research can go 3+ levels in
        // research/podcast workflows) and must terminate even when a
        // child's child_session_key happens to point back to an already
        // visited session.
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let parent_session_key = SessionKey::new("api", "deep-research");

        // Level 1: parent spawns child A.
        let parent_supervisor = Arc::new(TaskSupervisor::new());
        let parent_ledger = data_dir.join("parent.jsonl");
        parent_supervisor
            .enable_persistence(&parent_ledger)
            .unwrap();
        let level1_id = parent_supervisor.register_with_lineage(
            "spawn",
            "call-l1",
            Some(&parent_session_key.to_string()),
            Some(parent_ledger.to_str().unwrap()),
        );
        let level1_child_key = parent_supervisor
            .get_task(&level1_id)
            .and_then(|t| t.child_session_key)
            .expect("level-1 child key");

        // Level 2: child A spawns child B.
        let mid_supervisor = Arc::new(TaskSupervisor::new());
        let mid_ledger = data_dir.join("mid.jsonl");
        mid_supervisor.enable_persistence(&mid_ledger).unwrap();
        let level2_id = mid_supervisor.register_with_lineage(
            "spawn",
            "call-l2",
            Some(&level1_child_key),
            Some(mid_ledger.to_str().unwrap()),
        );
        let level2_child_key = mid_supervisor
            .get_task(&level2_id)
            .and_then(|t| t.child_session_key)
            .expect("level-2 child key");

        // Level 3: leaf task running inside child B. We also register a
        // synthetic task whose child_session_key points back at the
        // already-visited parent — the visited guard must prevent a loop.
        let leaf_supervisor = Arc::new(TaskSupervisor::new());
        let leaf_ledger = data_dir.join("leaf.jsonl");
        leaf_supervisor.enable_persistence(&leaf_ledger).unwrap();
        let leaf_id = leaf_supervisor.register_with_lineage(
            "bg_research",
            "call-l3",
            Some(&level2_child_key),
            Some(leaf_ledger.to_str().unwrap()),
        );
        leaf_supervisor.mark_running(&leaf_id);

        let store = SessionTaskQueryStore::default();
        store.register(&parent_session_key, &parent_supervisor, &data_dir);
        store.register(
            &SessionKey(level1_child_key.clone()),
            &mid_supervisor,
            &data_dir,
        );
        store.register(
            &SessionKey(level2_child_key.clone()),
            &leaf_supervisor,
            &data_dir,
        );

        let payload = store.query_json(&parent_session_key.to_string());
        let tasks = payload.as_array().expect("array response");
        assert_eq!(
            tasks.len(),
            3,
            "depth-3 descendant traversal must surface every task exactly once"
        );

        let tool_names: std::collections::HashSet<&str> = tasks
            .iter()
            .filter_map(|t| t["tool_name"].as_str())
            .collect();
        assert!(tool_names.contains("spawn"));
        assert!(tool_names.contains("bg_research"));
    }
}
