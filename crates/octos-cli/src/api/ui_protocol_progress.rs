//! Mapping from legacy progress JSON frames to UI protocol progress shapes.
//!
//! This module is deliberately independent from the WebSocket loop so the
//! protocol mapping can be tested before the live transport adopts it.

#![allow(dead_code)]

use octos_core::ui_protocol::{
    ApprovalId, ApprovalRequestedEvent, MessageDeltaEvent, PlanUpdatedEvent, ReasoningDeltaEvent,
    TaskRuntimeState as UiTaskRuntimeState, TaskUpdatedEvent, ToolCompletedEvent,
    ToolProgressEvent, ToolStartedEvent, TurnId, UiFileMutationNotice, UiNotification,
    UiPlanRecord, UiProgressEvent, UiProgressMetadata, UiRetryBackoff, UiTokenCostUpdate,
    WarningEvent, file_mutation_operations, progress_kinds,
};
use octos_core::{SessionKey, TaskId};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProgressMappingContext {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
}

impl ProgressMappingContext {
    pub(crate) fn new(session_id: SessionKey, turn_id: TurnId) -> Self {
        Self {
            session_id,
            turn_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UiProgressStatus {
    pub event: UiProgressEvent,
}

impl UiProgressStatus {
    fn new(context: &ProgressMappingContext, metadata: UiProgressMetadata) -> Self {
        Self {
            event: UiProgressEvent::new(
                context.session_id.clone(),
                Some(context.turn_id.clone()),
                metadata,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct UiProgressMapping {
    pub notifications: Vec<UiNotification>,
    pub status: Option<UiProgressStatus>,
    pub warning: Option<WarningEvent>,
}

impl UiProgressMapping {
    fn notifications(notifications: Vec<UiNotification>) -> Self {
        Self {
            notifications,
            status: None,
            warning: None,
        }
    }

    fn status(context: &ProgressMappingContext, metadata: UiProgressMetadata) -> Self {
        Self {
            notifications: Vec::new(),
            status: Some(UiProgressStatus::new(context, metadata)),
            warning: None,
        }
    }

    fn warning(context: &ProgressMappingContext, code: impl Into<String>, message: String) -> Self {
        Self {
            notifications: Vec::new(),
            status: None,
            warning: Some(WarningEvent {
                session_id: context.session_id.clone(),
                turn_id: Some(context.turn_id.clone()),
                code: code.into(),
                message,
            }),
        }
    }
}

pub(crate) fn map_progress_json(
    context: &ProgressMappingContext,
    event: &Value,
) -> UiProgressMapping {
    let Some(event_type) = event.get("type").and_then(Value::as_str) else {
        return UiProgressMapping::warning(
            context,
            "invalid_progress",
            "progress event is missing string field `type`".to_string(),
        );
    };

    match event_type {
        "token" => map_token(context, event),
        "reasoning_chunk" => map_reasoning_chunk(context, event),
        "tool_start" => map_tool_start(context, event),
        "tool_progress" => map_tool_progress(context, event),
        "tool_end" => map_tool_end(context, event),
        "task_started" => map_task_started(context, event),
        "task_updated" => map_task_updated(context, event),
        "plan_updated" => map_plan_updated(context, event),
        "task_completed" => map_task_completed(context, event),
        "task_interrupted" => map_task_interrupted(context, event),
        "max_iterations_reached" => map_budget_stop(context, event, "max iterations reached"),
        "token_budget_exceeded" => map_budget_stop(context, event, "token budget exceeded"),
        "activity_timeout_reached" => map_budget_stop(context, event, "activity timeout reached"),
        "llm_status" => map_simple_status(context, event, progress_kinds::STATUS),
        "agent_progress" => map_simple_status(context, event, "agent_progress"),
        "stream_retry" => map_stream_retry(context, event),
        "thinking" => map_simple_status(context, event, progress_kinds::THINKING),
        "response" => map_simple_status(context, event, progress_kinds::RESPONSE),
        "cost_update" => map_cost_update(context, event),
        "stream_end" => map_simple_status(context, event, progress_kinds::STREAM_END),
        "retry" | "retry_backoff" => map_retry_backoff(context, event),
        "approval_requested" | "approval_request" => map_approval_requested(context, event),
        "file_modified" | "file_written" | "file_mutation" => {
            map_file_mutation(context, event_type, event)
        }
        other => UiProgressMapping::warning(
            context,
            "unmapped_progress",
            format!("unmapped progress event: {other}"),
        ),
    }
}

fn map_approval_requested(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let tool_name = string_field(event, &["tool", "tool_name"]).unwrap_or_else(|| "tool".into());
    let title = string_field(event, &["title"]).unwrap_or_else(|| "Approval requested".into());
    let body = string_field(event, &["body", "message", "reason"]).unwrap_or_default();
    let approval_id = event
        .get("approval_id")
        .cloned()
        .and_then(|value| serde_json::from_value::<ApprovalId>(value).ok())
        .unwrap_or_default();

    UiProgressMapping::notifications(vec![UiNotification::ApprovalRequested(
        ApprovalRequestedEvent::generic(
            context.session_id.clone(),
            approval_id,
            context.turn_id.clone(),
            tool_name,
            title,
            body,
        ),
    )])
}

fn map_plan_updated(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let Some(plan_value) = event.get("plan").cloned() else {
        return UiProgressMapping::warning(
            context,
            "invalid_plan",
            "plan_updated event is missing `plan`".to_string(),
        );
    };
    let plan: UiPlanRecord = match serde_json::from_value(plan_value) {
        Ok(plan) => plan,
        Err(err) => {
            return UiProgressMapping::warning(
                context,
                "invalid_plan",
                format!("plan_updated `plan` did not deserialize: {err}"),
            );
        }
    };
    UiProgressMapping::notifications(vec![UiNotification::PlanUpdated(PlanUpdatedEvent {
        session_id: context.session_id.clone(),
        topic: None,
        turn_id: Some(context.turn_id.clone()),
        plan,
    })])
}

fn map_task_started(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    if let Some(task_id) = task_id_field(event, &["task_id"]) {
        return UiProgressMapping::notifications(vec![UiNotification::TaskUpdated(
            TaskUpdatedEvent {
                session_id: context.session_id.clone(),
                topic: None,
                task_id,
                tool_call_id: string_field(event, &["tool_call_id"]),
                title: string_field(event, &["title"]).unwrap_or_else(|| "Task".into()),
                state: UiTaskRuntimeState::Running,
                runtime_detail: Some("task started".into()),
                source: string_field(event, &["source"]),
                role: string_field(event, &["role"]),
                summary: string_field(event, &["summary"]),
                artifact_count: u32_field(event, &["artifact_count"]),
                // Codex P2 follow-up: a `null` runtime_policy_stamp from
                // the progress producer must be treated as ABSENT (None),
                // not Some(Value::Null). Otherwise serde emits
                // `"runtime_policy_stamp": null` on `task/updated` and
                // clients that cache the stamp see it wiped on every
                // update tick.
                runtime_policy_stamp: event
                    .get("runtime_policy_stamp")
                    .cloned()
                    .filter(|v| !v.is_null()),
                // C1 step 4: stamp the originating turn id (see map_task_updated).
                turn_id: turn_id_field(event).or_else(|| Some(context.turn_id.clone())),
            },
        )]);
    }

    let mut metadata = UiProgressMetadata::new(progress_kinds::STATUS);
    metadata.message = Some("task started".into());
    if let Some(task_id) = string_field(event, &["task_id"]) {
        metadata.extra.insert("task_id".into(), json!(task_id));
    }
    UiProgressMapping::status(context, metadata)
}

fn map_task_updated(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let Some(task_id) = task_id_field(event, &["task_id", "id"]) else {
        return UiProgressMapping::warning(
            context,
            "invalid_progress",
            "task_updated progress event is missing valid UUID field `task_id`".to_string(),
        );
    };

    let title =
        string_field(event, &["title", "tool", "tool_name"]).unwrap_or_else(|| "Task".into());
    // M9 review fix MEDIUM #4: distinguish "no state field at all" (legacy
    // task_started events) from "state field present but unrecognised".
    // The former keeps the historical default of `Running` so legacy callers
    // continue to render correctly. The latter emits a `warning` notification
    // — the previous unwrap_or fallback masked typos and unmapped terminal
    // states (notably `cancelled` before this PR) by reporting them as
    // running, so adding a recognised variant would only fix one symptom and
    // the next future state would regress the same way.
    let raw_state = string_field(
        event,
        &["state", "lifecycle_state", "status", "runtime_state"],
    );
    let state = match raw_state.as_deref() {
        Some(raw) => match ui_task_runtime_state(raw) {
            Some(state) => state,
            None => {
                return UiProgressMapping::warning(
                    context,
                    "invalid_progress",
                    format!(
                        "task_updated progress event has unrecognised lifecycle state `{raw}`; \
                         dropping notification to avoid rendering it as still running",
                    ),
                );
            }
        },
        None => UiTaskRuntimeState::Running,
    };

    UiProgressMapping::notifications(vec![UiNotification::TaskUpdated(TaskUpdatedEvent {
        session_id: context.session_id.clone(),
        topic: None,
        task_id,
        tool_call_id: string_field(event, &["tool_call_id"]),
        title,
        state,
        runtime_detail: string_field(event, &["runtime_detail", "message", "status_message"]),
        // #1123 codex P2 follow-up to #1113: thread the M13-B
        // projection fields from the underlying progress JSON
        // (produced by `background_task_to_progress_json`) onto the
        // `task/updated` notification. Unset values stay `None`, which
        // skip_serializing_if drops from the wire shape — so legacy
        // emitters that don't supply the fields produce a bare
        // payload identical to the pre-M13-B shape.
        source: string_field(event, &["source"]),
        role: string_field(event, &["role"]),
        summary: string_field(event, &["summary"]),
        artifact_count: u32_field(event, &["artifact_count"]),
        // Codex P2 rev2 follow-up: same null-to-absent filtering as
        // map_task_started above — a `null` stamp from the producer must
        // not pass through, otherwise serde emits
        // `"runtime_policy_stamp": null` on the regular `task/updated`
        // tick and any client that caches the stamp sees it wiped.
        runtime_policy_stamp: event
            .get("runtime_policy_stamp")
            .cloned()
            .filter(|v| !v.is_null()),
        // C1 step 4: stamp the originating turn id so the client can
        // reconcile its per-turn "N running" task count when a sub-agent
        // reaches a terminal state. The source is the in-flight standalone
        // turn's `TurnId` carried on `ProgressMappingContext` — the task
        // snapshot itself records only a `client_message_id` (cmid), not a
        // TurnId, so the context turn is the authoritative turn identity for
        // events emitted during this turn. The producer JSON may override it
        // with an explicit `turn_id` field when present.
        turn_id: turn_id_field(event).or_else(|| Some(context.turn_id.clone())),
    })])
}

fn map_task_completed(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let success = bool_field(event, &["success"]);
    let mut metadata = UiProgressMetadata::new(progress_kinds::STATUS);
    metadata.message = Some(match success {
        Some(false) => "task failed".into(),
        _ => "task completed".into(),
    });
    metadata.iteration = u32_field(event, &["iterations", "iteration"]);
    if let Some(success) = success {
        metadata.extra.insert("success".into(), json!(success));
    }
    if let Some(duration_ms) = u64_field(event, &["duration_ms", "elapsed_ms"]) {
        metadata
            .extra
            .insert("duration_ms".into(), json!(duration_ms));
    }
    UiProgressMapping::status(context, metadata)
}

fn map_task_interrupted(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let mut metadata = UiProgressMetadata::new(progress_kinds::STATUS);
    metadata.message = Some("task interrupted".into());
    metadata.iteration = u32_field(event, &["iterations", "iteration"]);
    UiProgressMapping::status(context, metadata)
}

fn map_budget_stop(
    context: &ProgressMappingContext,
    event: &Value,
    message: &'static str,
) -> UiProgressMapping {
    let mut metadata = UiProgressMetadata::new(progress_kinds::STATUS);
    metadata.message = Some(message.into());
    for key in ["used", "limit", "elapsed_ms", "limit_ms"] {
        if let Some(value) = event.get(key) {
            metadata.extra.insert(key.into(), value.clone());
        }
    }
    UiProgressMapping::status(context, metadata)
}

fn map_stream_retry(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let mut retry = UiRetryBackoff::new();
    retry.reason = string_field(event, &["reason", "message"]);
    let mut metadata = UiProgressMetadata::retry_backoff(retry);
    metadata.message = string_field(event, &["message", "status"]);
    metadata.iteration = u32_field(event, &["iteration"]);
    UiProgressMapping::status(context, metadata)
}

fn map_token(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let Some(text) = string_field(event, &["text"]) else {
        return UiProgressMapping::warning(
            context,
            "invalid_progress",
            "token progress event is missing string field `text`".to_string(),
        );
    };

    UiProgressMapping::notifications(vec![UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: context.session_id.clone(),
        topic: None,
        turn_id: context.turn_id.clone(),
        text,
    })])
}

fn map_reasoning_chunk(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let Some(text) = string_field(event, &["text"]) else {
        return UiProgressMapping::warning(
            context,
            "invalid_progress",
            "reasoning_chunk progress event is missing string field `text`".to_string(),
        );
    };

    UiProgressMapping::notifications(vec![UiNotification::ReasoningDelta(ReasoningDeltaEvent {
        session_id: context.session_id.clone(),
        topic: None,
        turn_id: context.turn_id.clone(),
        text,
    })])
}

fn map_tool_start(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let tool_name = string_field(event, &["tool", "tool_name"]).unwrap_or_else(|| "tool".into());
    let tool_call_id =
        string_field(event, &["tool_call_id", "id"]).unwrap_or_else(|| tool_name.clone());

    UiProgressMapping::notifications(vec![UiNotification::ToolStarted(ToolStartedEvent {
        session_id: context.session_id.clone(),
        topic: context.session_id.topic().map(ToOwned::to_owned),
        turn_id: context.turn_id.clone(),
        tool_call_id,
        tool_name,
        arguments: event.get("arguments").cloned(),
    })])
}

fn map_tool_progress(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let tool_name = string_field(event, &["tool", "tool_name"]).unwrap_or_else(|| "tool".into());
    let tool_call_id =
        string_field(event, &["tool_call_id", "id"]).unwrap_or_else(|| tool_name.clone());

    UiProgressMapping::notifications(vec![UiNotification::ToolProgress(ToolProgressEvent {
        session_id: context.session_id.clone(),
        topic: context.session_id.topic().map(ToOwned::to_owned),
        turn_id: context.turn_id.clone(),
        tool_call_id,
        message: string_field(event, &["message", "status"]),
        progress_pct: f32_field(event, &["progress_pct", "progress"]),
    })])
}

fn map_tool_end(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let tool_name = string_field(event, &["tool", "tool_name"]).unwrap_or_else(|| "tool".into());
    let tool_call_id =
        string_field(event, &["tool_call_id", "id"]).unwrap_or_else(|| tool_name.clone());

    let mut metadata = UiProgressMetadata::new(progress_kinds::TOOL_COMPLETED);
    metadata
        .extra
        .insert("tool".into(), json!(tool_name.clone()));
    metadata
        .extra
        .insert("tool_call_id".into(), json!(tool_call_id.clone()));
    if let Some(success) = bool_field(event, &["success"]) {
        metadata.extra.insert("success".into(), json!(success));
    }
    if let Some(duration_ms) = u64_field(event, &["duration_ms", "elapsed_ms"]) {
        metadata
            .extra
            .insert("duration_ms".into(), json!(duration_ms));
    }

    UiProgressMapping {
        notifications: vec![UiNotification::ToolCompleted(ToolCompletedEvent {
            session_id: context.session_id.clone(),
            topic: context.session_id.topic().map(ToOwned::to_owned),
            turn_id: context.turn_id.clone(),
            tool_call_id,
            tool_name,
            success: bool_field(event, &["success"]),
            // Raw progress JSON can come from arbitrary producers (binary
            // plugins); bound before this lands in the durable ledger.
            output_preview: string_field(event, &["output_preview"]).map(|preview| {
                octos_core::truncated_utf8(
                    &preview,
                    octos_core::ui_protocol::ENVELOPE_TOOL_OUTPUT_PREVIEW_MAX,
                    "…",
                )
            }),
            duration_ms: u64_field(event, &["duration_ms", "elapsed_ms"]),
        })],
        status: Some(UiProgressStatus::new(context, metadata)),
        warning: None,
    }
}

fn map_simple_status(
    context: &ProgressMappingContext,
    event: &Value,
    kind: &'static str,
) -> UiProgressMapping {
    let mut metadata = UiProgressMetadata::new(kind);
    metadata.message = string_field(event, &["message", "status"]);
    metadata.iteration = u32_field(event, &["iteration"]);
    UiProgressMapping::status(context, metadata)
}

fn map_cost_update(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let mut update = UiTokenCostUpdate::new();
    update.input_tokens = u64_field(event, &["input_tokens", "tokens_in"]);
    update.output_tokens = u64_field(event, &["output_tokens", "tokens_out"]);
    update.reasoning_tokens = u64_field(event, &["reasoning_tokens"]);
    update.cache_read_tokens = u64_field(event, &["cache_read_tokens"]);
    update.cache_write_tokens = u64_field(event, &["cache_write_tokens"]);
    update.total_tokens = u64_field(event, &["total_tokens"]);
    update.response_cost = f64_field(event, &["response_cost"]);
    update.session_cost = f64_field(event, &["session_cost"]);
    update.currency = string_field(event, &["currency"]);
    // Carry the model id forward when the agent emit layer populated it
    // — the chat client renders `model · tokens_in / tokens_out · duration`
    // footers from `metadata.token_cost.model`. Legacy clients that
    // sniff `metadata.label` continue to work (we still emit the field
    // omitted when absent).
    update.model = string_field(event, &["model"]);
    // Carry the model context window so clients render an honest ctx-fill
    // gauge against the real window instead of a hardcoded default.
    update.context_window = u64_field(event, &["context_window"]);

    let mut metadata = UiProgressMetadata::token_cost(update);
    metadata.message = string_field(event, &["message", "status"]);
    UiProgressMapping::status(context, metadata)
}

fn map_retry_backoff(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let mut retry = UiRetryBackoff::new();
    retry.attempt = u32_field(event, &["attempt", "retry_round"]);
    retry.max_attempts = u32_field(event, &["max_attempts", "limit"]);
    retry.backoff_ms = u64_field(event, &["backoff_ms", "delay_ms", "retry_after_ms"]);
    retry.reason = string_field(event, &["reason", "message"]);
    retry.provider = string_field(event, &["provider"]);
    retry.next_provider = string_field(event, &["next_provider"]);

    let mut metadata = UiProgressMetadata::retry_backoff(retry);
    metadata.message = string_field(event, &["message", "status"]);
    UiProgressMapping::status(context, metadata)
}

fn map_file_mutation(
    context: &ProgressMappingContext,
    event_type: &str,
    event: &Value,
) -> UiProgressMapping {
    let Some(path) = string_field(event, &["path", "file"]) else {
        return UiProgressMapping::warning(
            context,
            "invalid_progress",
            format!("{event_type} progress event is missing string field `path`"),
        );
    };
    let operation = string_field(event, &["operation", "op"]).unwrap_or_else(|| match event_type {
        "file_written" => file_mutation_operations::WRITE.to_string(),
        "file_modified" => file_mutation_operations::MODIFY.to_string(),
        _ => file_mutation_operations::MODIFY.to_string(),
    });
    let mut notice = UiFileMutationNotice::new(path, operation);
    notice.tool_call_id = string_field(event, &["tool_call_id", "id"]);
    notice.bytes_written = u64_field(event, &["bytes_written", "bytes"]);

    let mut metadata = UiProgressMetadata::file_mutation(notice);
    metadata.message = string_field(event, &["message", "status"]);
    UiProgressMapping::status(context, metadata)
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn u64_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        let value = value.get(*key)?;
        value.as_u64().or_else(|| {
            value
                .as_i64()
                .and_then(|number| u64::try_from(number).ok())
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
    })
}

fn u32_field(value: &Value, keys: &[&str]) -> Option<u32> {
    u64_field(value, keys).and_then(|number| u32::try_from(number).ok())
}

fn f64_field(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        let value = value.get(*key)?;
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    })
}

fn f32_field(value: &Value, keys: &[&str]) -> Option<f32> {
    f64_field(value, keys).map(|number| number as f32)
}

fn bool_field(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_bool))
}

fn task_id_field(value: &Value, keys: &[&str]) -> Option<TaskId> {
    string_field(value, keys).and_then(|task_id| task_id.parse().ok())
}

/// C1 step 4: extract an explicit `turn_id` from the producer progress JSON
/// when present (a UUID string). Callers fall back to the in-flight turn on
/// the `ProgressMappingContext` when this returns `None`.
fn turn_id_field(value: &Value) -> Option<TurnId> {
    string_field(value, &["turn_id"])
        .and_then(|raw| uuid::Uuid::parse_str(&raw).ok())
        .map(TurnId)
}

fn ui_task_runtime_state(state: &str) -> Option<UiTaskRuntimeState> {
    match state {
        "pending" | "queued" | "spawned" => Some(UiTaskRuntimeState::Pending),
        "running" | "executing_tool" | "resolving_outputs" | "verifying_outputs"
        | "delivering_outputs" | "cleaning_up" | "verifying" => Some(UiTaskRuntimeState::Running),
        "completed" | "ready" => Some(UiTaskRuntimeState::Completed),
        "failed" => Some(UiTaskRuntimeState::Failed),
        // M9 review fix (MEDIUM #4) — governed by accepted UPCR-2026-004.
        // The agent emits `TaskLifecycleState::Cancelled` (snake_case
        // `"cancelled"`) for tasks cancelled via the supervisor's `cancel()`
        // primitive (e.g. `POST /api/tasks/{id}/cancel`). The US-spelling
        // alias `"canceled"` is accepted defensively because some upstream
        // sources spell it that way.
        "cancelled" | "canceled" => Some(UiTaskRuntimeState::Cancelled),
        _ => None,
    }
}

pub(crate) fn background_task_to_progress_json(task: &octos_agent::BackgroundTask) -> Value {
    // Carry `tool_call_id` on every `task_updated` snapshot so the
    // mapper below threads it onto `TaskUpdatedEvent`. The client uses
    // the wire-side mapping instead of racing a `task/updated` watcher
    // to build `task_id → tool_call_id` post-hoc.
    //
    // Codex P2 follow-up (#1113): the M13-B projection fields
    // (source/role/summary/artifact_count/runtime_policy_stamp) live
    // on `BackgroundTask` after `set_m13b_projection` runs, but the
    // live `task/updated` JSON used to drop them, so connected
    // subscribers had to refresh `task/list` to see the metadata.
    // Include them here so the live wire matches the snapshot view.
    let mut payload = json!({
        "type": "task_updated",
        "task_id": task.id,
        "tool_call_id": task.tool_call_id,
        "title": task.tool_name,
        "state": task.lifecycle_state(),
        "runtime_detail": stable_task_runtime_detail(task),
    });
    if let Value::Object(obj) = &mut payload {
        if let Some(source) = task.source.as_ref() {
            obj.insert("source".into(), json!(source));
        }
        if let Some(role) = task.role.as_ref() {
            obj.insert("role".into(), json!(role));
        }
        if let Some(summary) = task.summary.as_ref() {
            obj.insert("summary".into(), json!(summary));
        }
        if let Some(count) = task.artifact_count {
            obj.insert("artifact_count".into(), json!(count));
        }
        if let Some(stamp) = task.runtime_policy_stamp.as_ref() {
            obj.insert("runtime_policy_stamp".into(), stamp.clone());
        }
    }
    payload
}

/// C8 / GAP A: build the `task/updated` notification used to REPLAY an
/// already-known supervisor task to a freshly-opened / reconnecting client.
///
/// A reconnecting TUI starts with an empty `session.tasks` and only applies
/// incremental `task/updated` deltas, so without replaying the current task
/// list on `session/open` the existing tasks are invisible until their next
/// live transition. This routes the raw [`octos_agent::BackgroundTask`]
/// through the SAME mapping live updates use
/// ([`background_task_to_progress_json`] -> [`map_progress_json`]), so the
/// replayed wire shape is byte-identical to a live `task/updated`.
///
/// Unlike a live update, a replay is NOT tied to an in-flight turn, so the
/// originating `turn_id` is cleared (`None`) — the client only needs the task
/// to populate its list; the per-turn "N running" reconciliation that
/// `turn_id` drives applies to live transitions during a turn, not to a
/// snapshot replay. Returns `None` if the task JSON cannot be mapped (the
/// only failure mode is an unmappable task_id / lifecycle state, which
/// [`map_progress_json`] turns into a `warning` rather than a notification).
pub(crate) fn replay_task_updated_notification(
    session_id: &SessionKey,
    task: &octos_agent::BackgroundTask,
) -> Option<UiNotification> {
    let event = background_task_to_progress_json(task);
    // The context turn is never surfaced: map_task_updated's fallback stamps
    // it, but we clear it below. A throwaway TurnId keeps the mapper happy.
    let context = ProgressMappingContext::new(session_id.clone(), TurnId::new());
    let mapping = map_progress_json(&context, &event);
    mapping.notifications.into_iter().find_map(|notification| {
        if let UiNotification::TaskUpdated(mut updated) = notification {
            // Replay snapshot — not attributable to a live turn.
            updated.turn_id = None;
            Some(UiNotification::TaskUpdated(updated))
        } else {
            None
        }
    })
}

fn stable_task_runtime_detail(task: &octos_agent::BackgroundTask) -> Option<String> {
    if let Some(error) = task.error.as_deref() {
        return Some(error.to_string());
    }

    let detail = task.runtime_detail.as_deref()?;
    match serde_json::from_str::<Value>(detail) {
        Ok(value) => {
            if let Some(message) = value.get("progress_message").and_then(Value::as_str) {
                return Some(message.to_string());
            }
            let phase = value.get("current_phase").and_then(Value::as_str)?;
            match value.get("workflow_kind").and_then(Value::as_str) {
                Some(kind) => Some(format!("{kind}: {phase}")),
                None => Some(phase.to_string()),
            }
        }
        Err(_) => Some(detail.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Utc;

    use octos_core::ui_protocol::{TaskRuntimeState as UiTaskRuntimeState, TurnId, UiNotification};

    use uuid::Uuid;

    fn context() -> ProgressMappingContext {
        ProgressMappingContext::new(SessionKey("local:demo".into()), TurnId(Uuid::from_u128(7)))
    }

    #[test]
    fn ui_protocol_progress_maps_token_to_message_delta() {
        let mapping = map_progress_json(&context(), &json!({ "type": "token", "text": "hi" }));

        assert_eq!(mapping.status, None);
        assert_eq!(mapping.warning, None);
        let [UiNotification::MessageDelta(delta)] = mapping.notifications.as_slice() else {
            panic!("expected message delta notification");
        };
        assert_eq!(delta.text, "hi");
    }

    #[test]
    fn ui_protocol_progress_maps_plan_updated_to_notification() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "plan_updated",
                "plan": {
                    "title": "Building memory panel…",
                    "updated_at_ms": 42,
                    "items": [
                        { "id": "1", "title": "done", "status": "completed", "priority": "P3" },
                        { "id": "2", "title": "active", "status": "in_progress" }
                    ]
                }
            }),
        );
        assert_eq!(mapping.warning, None);
        let [UiNotification::PlanUpdated(event)] = mapping.notifications.as_slice() else {
            panic!(
                "expected a plan/updated notification, got {:?}",
                mapping.notifications
            );
        };
        assert_eq!(event.session_id, SessionKey("local:demo".into()));
        assert!(event.turn_id.is_some());
        assert_eq!(event.plan.title.as_deref(), Some("Building memory panel…"));
        assert_eq!(event.plan.items.len(), 2);
        assert_eq!(
            event.plan.items[1].status,
            octos_core::ui_protocol::PlanItemStatus::InProgress
        );
    }

    #[test]
    fn ui_protocol_progress_plan_updated_missing_plan_warns() {
        let mapping = map_progress_json(&context(), &json!({ "type": "plan_updated" }));
        assert!(mapping.notifications.is_empty());
        assert!(mapping.warning.is_some());
    }

    #[test]
    fn ui_protocol_progress_maps_reasoning_chunk_to_reasoning_delta() {
        let mapping = map_progress_json(
            &context(),
            &json!({ "type": "reasoning_chunk", "text": "thinking" }),
        );

        assert_eq!(mapping.status, None);
        assert_eq!(mapping.warning, None);
        let [UiNotification::ReasoningDelta(delta)] = mapping.notifications.as_slice() else {
            panic!("expected reasoning delta notification");
        };
        assert_eq!(delta.text, "thinking");
    }

    #[test]
    fn ui_protocol_progress_preserves_tool_progress_call_id() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "tool_progress",
                "tool": "shell",
                "tool_call_id": "call-42",
                "message": "running",
                "progress_pct": 37.5
            }),
        );

        let [UiNotification::ToolProgress(progress)] = mapping.notifications.as_slice() else {
            panic!("expected tool progress notification");
        };
        assert_eq!(progress.tool_call_id, "call-42");
        assert_eq!(progress.message.as_deref(), Some("running"));
        assert_eq!(progress.progress_pct, Some(37.5));
    }

    #[test]
    fn ui_protocol_progress_maps_tool_start() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "tool_start",
                "tool": "shell",
                "tool_call_id": "call-42",
                "arguments": {"command": "cargo test"}
            }),
        );

        let [UiNotification::ToolStarted(started)] = mapping.notifications.as_slice() else {
            panic!("expected tool started notification");
        };
        assert_eq!(started.tool_name, "shell");
        assert_eq!(started.tool_call_id, "call-42");
        assert_eq!(
            started
                .arguments
                .as_ref()
                .and_then(|args| args.get("command")),
            Some(&json!("cargo test"))
        );
        assert_eq!(mapping.status, None);
        assert_eq!(mapping.warning, None);
    }

    #[test]
    fn ui_protocol_progress_bounds_tool_end_preview_from_raw_json() {
        // Raw progress JSON can come from arbitrary producers (binary
        // plugins) — the mapper must bound before the durable ledger.
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "tool_end",
                "tool": "shell",
                "tool_call_id": "call-9",
                "success": false,
                "output_preview": "x".repeat(9000),
            }),
        );
        let [UiNotification::ToolCompleted(completed)] = mapping.notifications.as_slice() else {
            panic!("expected tool completed notification");
        };
        let preview = completed.output_preview.as_deref().expect("preview");
        assert!(
            preview.chars().count()
                <= octos_core::ui_protocol::ENVELOPE_TOOL_OUTPUT_PREVIEW_MAX + 1,
            "mapper must bound raw-JSON previews, got {} chars",
            preview.chars().count()
        );
    }

    #[test]
    fn ui_protocol_progress_maps_task_started_to_status() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "task_started",
                "task_id": "01900000-0000-7000-8000-000000000001"
            }),
        );

        let [UiNotification::TaskUpdated(updated)] = mapping.notifications.as_slice() else {
            panic!("expected task updated notification");
        };
        assert_eq!(
            updated.task_id.to_string(),
            "01900000-0000-7000-8000-000000000001"
        );
        assert_eq!(updated.state, UiTaskRuntimeState::Running);
        assert_eq!(updated.runtime_detail.as_deref(), Some("task started"));
        assert_eq!(mapping.status, None);
        assert_eq!(mapping.warning, None);
    }

    #[test]
    fn ui_protocol_progress_maps_cost_update_to_token_cost_status() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "cost_update",
                "input_tokens": 10,
                "output_tokens": 4,
                "session_cost": 0.0012
            }),
        );

        let status = mapping.status.expect("cost status");
        let cost = status
            .event
            .metadata
            .token_cost
            .expect("token cost metadata");
        assert_eq!(
            status.event.metadata.kind,
            progress_kinds::TOKEN_COST_UPDATE
        );
        assert_eq!(cost.input_tokens, Some(10));
        assert_eq!(cost.output_tokens, Some(4));
        assert_eq!(cost.session_cost, Some(0.0012));
        // Back-compat: payloads without a `model` field land with
        // `cost.model = None` and the client can fall back to the
        // historical `metadata.label` sniff.
        assert_eq!(cost.model, None);
    }

    #[test]
    fn ui_protocol_progress_preserves_tool_end_success_metadata() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "tool_end",
                "tool": "shell",
                "tool_call_id": "call-42",
                "success": false,
                "output_preview": "permission denied",
                "duration_ms": 1250
            }),
        );

        let [UiNotification::ToolCompleted(completed)] = mapping.notifications.as_slice() else {
            panic!("expected tool completed notification");
        };
        assert_eq!(completed.tool_call_id, "call-42");
        assert_eq!(completed.success, Some(false));
        assert_eq!(
            completed.output_preview.as_deref(),
            Some("permission denied")
        );
        assert_eq!(completed.duration_ms, Some(1250));

        let status = mapping.status.expect("tool completion status");
        assert_eq!(status.event.metadata.kind, progress_kinds::TOOL_COMPLETED);
        assert_eq!(
            status.event.metadata.extra.get("success"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            status.event.metadata.extra.get("duration_ms"),
            Some(&json!(1250))
        );
    }

    #[test]
    fn ui_protocol_progress_maps_retry_and_file_mutation_status() {
        let retry = map_progress_json(
            &context(),
            &json!({
                "type": "retry_backoff",
                "attempt": 2,
                "max_attempts": 4,
                "backoff_ms": 750,
                "reason": "rate limit"
            }),
        );
        let retry = retry
            .status
            .expect("retry status")
            .event
            .metadata
            .retry
            .expect("retry metadata");
        assert_eq!(retry.attempt, Some(2));
        assert_eq!(retry.backoff_ms, Some(750));

        let file = map_progress_json(
            &context(),
            &json!({
                "type": "file_written",
                "path": "src/lib.rs",
                "bytes_written": 128
            }),
        );
        let notice = file
            .status
            .expect("file status")
            .event
            .metadata
            .file_mutation
            .expect("file metadata");
        assert_eq!(notice.path, "src/lib.rs");
        assert_eq!(notice.operation, file_mutation_operations::WRITE);
        assert_eq!(notice.bytes_written, Some(128));
    }

    #[test]
    fn ui_protocol_progress_maps_unknown_to_warning() {
        let mapping = map_progress_json(&context(), &json!({ "type": "surprise" }));

        let warning = mapping.warning.expect("warning");
        assert_eq!(warning.code, "unmapped_progress");
        assert!(warning.message.contains("surprise"));
        assert!(mapping.notifications.is_empty());
        assert_eq!(mapping.status, None);
    }

    #[test]
    fn ui_protocol_progress_maps_task_updated_to_notification() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "task_updated",
                "task_id": "01900000-0000-7000-8000-000000000002",
                "title": "search",
                "state": "verifying",
                "runtime_detail": "checking outputs"
            }),
        );

        let [UiNotification::TaskUpdated(updated)] = mapping.notifications.as_slice() else {
            panic!("expected task updated notification");
        };
        assert_eq!(
            updated.task_id.to_string(),
            "01900000-0000-7000-8000-000000000002"
        );
        assert_eq!(updated.title, "search");
        assert_eq!(updated.state, UiTaskRuntimeState::Running);
        assert_eq!(updated.runtime_detail.as_deref(), Some("checking outputs"));
        // C1 step 4: when the producer JSON omits an explicit turn_id, the
        // emitter falls back to the in-flight turn on the context.
        assert_eq!(
            updated.turn_id,
            Some(TurnId(Uuid::from_u128(7))),
            "task_updated must stamp the originating turn from the context",
        );
    }

    #[test]
    fn ui_protocol_progress_warns_on_unknown_task_state_instead_of_silently_running() {
        // M9 review fix MEDIUM #4: codex 2nd-opinion finding — the old
        // `unwrap_or(UiTaskRuntimeState::Running)` fallback masked future
        // unmapped terminal states (e.g. before this PR, `cancelled` rendered
        // as still running). With a recognised state list, an unrecognised
        // lifecycle string now emits a structured `invalid_progress` warning
        // instead of synthesising a misleading `Running` notification.
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "task_updated",
                "task_id": "01900000-0000-7000-8000-000000000004",
                "title": "spawn_only_runner",
                "state": "definitely_not_a_real_lifecycle_state"
            }),
        );

        assert!(
            mapping.notifications.is_empty(),
            "unrecognised lifecycle state must not synthesise a TaskUpdated notification",
        );
        let warning = mapping
            .warning
            .expect("expected invalid_progress warning for unknown lifecycle state");
        assert_eq!(warning.code, "invalid_progress");
        assert!(
            warning
                .message
                .contains("definitely_not_a_real_lifecycle_state"),
            "warning should surface the offending raw state string, got: {}",
            warning.message,
        );
    }

    #[test]
    fn background_task_progress_json_uses_stable_detail() {
        let task = octos_agent::BackgroundTask {
            id: "01900000-0000-7000-8000-000000000003".into(),
            tool_name: "search".into(),
            tool_call_id: "call-1".into(),
            parent_session_key: Some("local:demo".into()),
            child_session_key: None,
            child_terminal_state: None,
            child_join_state: None,
            child_joined_at: None,
            child_failure_action: None,
            task_ledger_path: None,
            status: octos_agent::TaskStatus::Running,
            runtime_state: octos_agent::TaskRuntimeState::DeliveringOutputs,
            runtime_detail: Some(
                json!({
                    "workflow_kind": "research",
                    "current_phase": "writing",
                    "progress_message": "Writing report"
                })
                .to_string(),
            ),
            started_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
            output_files: Vec::new(),
            error: None,
            final_output: None,
            failed_by_observer: false,
            session_key: Some("local:demo".into()),
            tool_input: None,
            originating_client_message_id: None,
            source: None,
            role: None,
            summary: None,
            artifact_count: None,
            runtime_policy_stamp: None,
            projection_metadata: None,
            workspace_root: None,
        };

        let event = background_task_to_progress_json(&task);
        assert_eq!(event["type"], "task_updated");
        assert_eq!(event["task_id"], "01900000-0000-7000-8000-000000000003");
        assert_eq!(event["title"], "search");
        assert_eq!(event["state"], "verifying");
        assert_eq!(event["runtime_detail"], "Writing report");
    }
}
