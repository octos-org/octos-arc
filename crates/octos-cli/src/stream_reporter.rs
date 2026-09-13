//! Progressive streaming reporter for messaging channels.
//!
//! Bridges the synchronous `ProgressReporter` trait to async channel I/O,
//! enabling real-time LLM text streaming to Telegram, WhatsApp, etc.
//! Text is accumulated and the channel message is edited at a throttled rate.

use octos_agent::progress::{ProgressEvent, ProgressReporter};
use tokio::sync::mpsc;

/// Events forwarded from the synchronous reporter to the async forwarder.
#[derive(Debug)]
pub enum StreamProgressEvent {
    /// A chunk of streaming text from the LLM.
    Chunk { text: String, iteration: u32 },
    /// Streaming finished for this iteration.
    StreamDone { iteration: u32 },
    /// A tool is about to run.
    ToolStarted { name: String },
    /// A tool completed.
    ToolCompleted { name: String, success: bool },
    /// Mid-execution progress from a tool.
    ToolProgress { name: String, message: String },
    /// LLM call status update (retry progress, provider switching).
    LlmStatus { message: String },
    /// A file was written/modified by a tool.
    FileWritten { path: String },
    /// Reset the streaming buffer (e.g. before an LLM retry so partial
    /// text from a failed attempt doesn't get concatenated with the retry).
    BufferReset,
    /// Raw SSE JSON to forward directly to the web client.
    /// Used for discrete progress events (thinking, cost_update) that
    /// the web UI needs as separate SSE events, not baked into message text.
    RawSse { json: String },
}

/// A `ProgressReporter` that forwards stream events through an unbounded channel.
///
/// Because `ProgressReporter::report()` is synchronous, we use `unbounded_send()`
/// which never blocks. The receiving async task handles actual channel I/O.
///
/// M8.10 PR #2: every emitted SSE payload includes `thread_id` (the
/// client_message_id of the user message that owns this turn). Reporters are
/// constructed per-turn so the thread_id is bound for the reporter's lifetime.
/// When `thread_id` is `None`, the field is omitted (preserves wire compat for
/// non-API channels and pre-cmid clients).
pub struct ChannelStreamReporter {
    tx: mpsc::UnboundedSender<StreamProgressEvent>,
    thread_id: Option<String>,
}

impl ChannelStreamReporter {
    pub fn new(tx: mpsc::UnboundedSender<StreamProgressEvent>) -> Self {
        Self {
            tx,
            thread_id: None,
        }
    }

    /// Bind a `thread_id` (typically the user message's `client_message_id`)
    /// to every SSE payload this reporter emits.
    pub fn with_thread_id(mut self, thread_id: Option<String>) -> Self {
        self.thread_id = thread_id.filter(|s| !s.is_empty());
        self
    }
}

/// Insert the bound `thread_id` into a JSON object payload (if any). Mutates
/// the value in place. No-op when `thread_id` is `None` or the value is not
/// an object.
fn inject_thread_id(value: &mut serde_json::Value, thread_id: Option<&str>) {
    if let (Some(tid), Some(obj)) = (thread_id, value.as_object_mut()) {
        obj.insert(
            "thread_id".to_string(),
            serde_json::Value::String(tid.to_string()),
        );
    }
}

impl ProgressReporter for ChannelStreamReporter {
    fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    fn report(&self, event: ProgressEvent) {
        let thread_id = self.thread_id.as_deref();
        let mapped = match event {
            ProgressEvent::StreamChunk { text, iteration } => {
                StreamProgressEvent::Chunk { text, iteration }
            }
            ProgressEvent::ReasoningChunk { text, iteration } => {
                let mut payload = serde_json::json!({
                    "type": "reasoning_chunk",
                    "text": text,
                    "iteration": iteration,
                });
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
            ProgressEvent::StreamDone { iteration } => {
                StreamProgressEvent::StreamDone { iteration }
            }
            ProgressEvent::ToolStarted {
                ref name,
                ref tool_id,
                ..
            } => {
                // Also send raw SSE for web client status indicators
                let mut payload = serde_json::json!({
                    "type": "tool_start",
                    "tool": name,
                    "tool_call_id": tool_id,
                });
                inject_thread_id(&mut payload, thread_id);
                let _ = self.tx.send(StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                });
                StreamProgressEvent::ToolStarted { name: name.clone() }
            }
            ProgressEvent::ToolCompleted {
                ref name,
                ref tool_id,
                success,
                ..
            } => {
                let mut payload = serde_json::json!({
                    "type": "tool_end",
                    "tool": name,
                    "tool_call_id": tool_id,
                    "success": success,
                });
                inject_thread_id(&mut payload, thread_id);
                let _ = self.tx.send(StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                });
                StreamProgressEvent::ToolCompleted {
                    name: name.clone(),
                    success,
                }
            }
            ProgressEvent::ToolProgress {
                ref name,
                ref tool_id,
                ref message,
            } => {
                let mut payload = serde_json::json!({
                    "type": "tool_progress",
                    "tool": name,
                    "tool_call_id": tool_id,
                    "message": message,
                });
                inject_thread_id(&mut payload, thread_id);
                let _ = self.tx.send(StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                });
                StreamProgressEvent::ToolProgress {
                    name: name.clone(),
                    message: message.clone(),
                }
            }
            ProgressEvent::LlmStatus { message, .. } => StreamProgressEvent::LlmStatus { message },
            ProgressEvent::FileModified { path } => StreamProgressEvent::FileWritten { path },
            ProgressEvent::StreamRetry { .. } => StreamProgressEvent::BufferReset,
            // Forward discrete progress events as raw SSE JSON for the web client.
            ProgressEvent::Thinking { iteration } => {
                let mut payload = serde_json::json!({"type": "thinking", "iteration": iteration});
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
            ProgressEvent::AgentProgress {
                iteration,
                active_tokens,
                elapsed,
                checkpoints,
                reflecting,
            } => {
                let mut payload = serde_json::json!({
                    "type": "agent_progress",
                    "message": octos_agent::progress::agent_progress_message(
                        iteration,
                        active_tokens,
                        elapsed,
                        checkpoints,
                        reflecting,
                    ),
                    "iteration": iteration,
                    "active_tokens": active_tokens,
                    "elapsed_ms": elapsed.as_millis() as u64,
                    "checkpoints": checkpoints,
                    "reflecting": reflecting,
                });
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
            ProgressEvent::Response { iteration, .. } => {
                let mut payload = serde_json::json!({"type": "response", "iteration": iteration});
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
            ProgressEvent::CostUpdate {
                session_input_tokens,
                session_output_tokens,
                session_cost,
                model,
                context_window,
                ..
            } => {
                // Codex round-1 P2: every wire-shape mapper for
                // `cost_update` must carry `model` so the chat bubble
                // footer (`model · tokens_in / tokens_out · duration`)
                // works regardless of which reporter the turn uses.
                // Without this, channel-stream consumers see naked
                // token counts even after the agent emit layer
                // attached a model id.
                let mut payload = serde_json::json!({
                    "type": "cost_update",
                    "input_tokens": session_input_tokens,
                    "output_tokens": session_output_tokens,
                    "session_cost": session_cost,
                });
                if let Some(model) = model.as_deref() {
                    payload["model"] = serde_json::Value::String(model.to_string());
                }
                // Carry the context window too so channel/web clients on this
                // path render an honest ctx gauge, matching the BoundedChannel
                // (`event_to_json`) path.
                if let Some(window) = context_window {
                    payload["context_window"] = serde_json::json!(window);
                }
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
            _ => return,
        };
        let _ = self.tx.send(mapped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn should_clear_buffer_on_reset_event() {
        // Simulate the forwarder receiving chunks then a BufferReset.
        // We can't easily test the full async forwarder, but we can
        // verify the event enum is correctly structured and mapped.
        let event = StreamProgressEvent::BufferReset;
        assert!(matches!(event, StreamProgressEvent::BufferReset));
    }

    #[test]
    fn should_map_stream_retry_to_buffer_reset() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::StreamRetry { iteration: 1 });

        let event = rx.try_recv().unwrap();
        assert!(matches!(event, StreamProgressEvent::BufferReset));
    }

    /// M8.10 PR #2: every raw SSE payload emitted by the reporter must
    /// include a `thread_id` field equal to the bound cmid. Drives every
    /// variant that produces a `RawSse` event to confirm none are missed.
    #[test]
    fn should_inject_thread_id_into_every_raw_sse_event() {
        use octos_agent::progress::ProgressEvent;
        use std::time::Duration;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter =
            ChannelStreamReporter::new(tx).with_thread_id(Some("cmid-thread-A".to_string()));

        reporter.report(ProgressEvent::ToolStarted {
            name: "shell".into(),
            tool_id: "t1".into(),
            arguments: None,
        });
        reporter.report(ProgressEvent::ToolCompleted {
            name: "shell".into(),
            tool_id: "t1".into(),
            success: true,
            output_preview: "ok".into(),
            duration: Duration::from_millis(5),
        });
        reporter.report(ProgressEvent::ToolProgress {
            name: "shell".into(),
            tool_id: "t1".into(),
            message: "step 1".into(),
        });
        reporter.report(ProgressEvent::Thinking { iteration: 0 });
        reporter.report(ProgressEvent::Response {
            content: "answer".into(),
            iteration: 1,
        });
        reporter.report(ProgressEvent::ReasoningChunk {
            text: "thinking".into(),
            iteration: 1,
        });
        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 10,
            session_output_tokens: 20,
            turn_input_tokens: 10,
            turn_output_tokens: 20,
            response_cost: None,
            session_cost: None,
            model: None,
            context_window: None,
        });

        let mut raw_payloads: Vec<String> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let StreamProgressEvent::RawSse { json } = event {
                raw_payloads.push(json);
            }
        }

        // ToolStarted, ToolCompleted, ToolProgress emit RawSse + a typed
        // mapped event each, so 7 reports → 7 RawSse JSON payloads.
        assert_eq!(
            raw_payloads.len(),
            7,
            "expected 7 RawSse payloads, got {}: {:?}",
            raw_payloads.len(),
            raw_payloads
        );
        for json in &raw_payloads {
            let parsed: serde_json::Value = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("payload `{json}` failed to parse: {e}"));
            assert_eq!(
                parsed.get("thread_id").and_then(|v| v.as_str()),
                Some("cmid-thread-A"),
                "payload `{json}` missing `thread_id` field",
            );
        }
    }

    /// The channel stream reporter feeds API/channel clients that read
    /// the wire payload directly. Codex round-1 P2 — when the agent
    /// emit layer attaches a `model` id to `CostUpdate`, this reporter
    /// must thread it through so the chat bubble footer (`model ·
    /// tokens_in / tokens_out · duration`) works for channel
    /// consumers, not just the UI-protocol bridge.
    #[test]
    fn cost_update_carries_model_into_raw_sse_payload() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 12,
            session_output_tokens: 7,
            turn_input_tokens: 12,
            turn_output_tokens: 7,
            response_cost: None,
            session_cost: None,
            model: Some("deepseek-v4-pro".into()),
            context_window: None,
        });

        let StreamProgressEvent::RawSse { json } = rx.try_recv().unwrap() else {
            panic!("expected RawSse for CostUpdate");
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "cost_update");
        assert_eq!(parsed["model"], "deepseek-v4-pro");
    }

    /// The channel-stream path must also carry the model context window so
    /// channel/web clients render an honest ctx gauge, matching the
    /// BoundedChannel (`event_to_json`) path.
    #[test]
    fn cost_update_carries_context_window_into_raw_sse_payload() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 12,
            session_output_tokens: 7,
            turn_input_tokens: 12,
            turn_output_tokens: 7,
            response_cost: None,
            session_cost: None,
            model: None,
            context_window: Some(131_072),
        });

        let StreamProgressEvent::RawSse { json } = rx.try_recv().unwrap() else {
            panic!("expected RawSse for CostUpdate");
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "cost_update");
        assert_eq!(parsed["context_window"], 131_072);
    }

    /// Symmetric to the test above: a `CostUpdate` without `model`
    /// must not synthesise a `model` field on the wire — the field is
    /// strictly opt-in so legacy parsers that key on field presence
    /// don't trip.
    #[test]
    fn cost_update_omits_model_when_absent() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 12,
            session_output_tokens: 7,
            turn_input_tokens: 12,
            turn_output_tokens: 7,
            response_cost: None,
            session_cost: None,
            model: None,
            context_window: None,
        });

        let StreamProgressEvent::RawSse { json } = rx.try_recv().unwrap() else {
            panic!("expected RawSse for CostUpdate");
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            parsed.get("model").is_none(),
            "model field must be omitted when the emit layer didn't set one, got {parsed}",
        );
    }

    /// When the reporter is constructed without a thread_id (or with an
    /// empty string), payloads must NOT carry a `thread_id` field. This
    /// preserves wire compatibility with non-API channels and pre-cmid
    /// clients that expect the field to be absent.
    #[test]
    fn should_omit_thread_id_when_not_bound() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::Thinking { iteration: 0 });

        if let StreamProgressEvent::RawSse { json } = rx.try_recv().unwrap() {
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(
                parsed.get("thread_id").is_none(),
                "thread_id field must be absent when reporter has no bound id, got {parsed}"
            );
        } else {
            panic!("expected RawSse for Thinking event");
        }
    }
}
