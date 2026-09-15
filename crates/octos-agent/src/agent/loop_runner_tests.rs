use super::*;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

use async_trait::async_trait;
use octos_core::{AgentId, MessageRole, TaskContext, TaskKind, ToolCall};

// --- compose_turn_user_content (video-call context hint) ---

#[test]
fn should_use_video_call_hint_when_turn_is_flagged_live_video() {
    // `is_video_call` is now the EXPLICIT live-video signal (set from the
    // turn ingress via `inbound.metadata.live_video`), no longer inferred
    // from audio+image attachments.
    let out = compose_turn_user_content("what am I holding", true, true, None);
    assert!(out.starts_with(VIDEO_CALL_NOTE), "got: {out}");
    assert!(out.contains("what am I holding"));
}

#[test]
fn should_not_add_hint_when_no_image() {
    // No image and not flagged → plain transcript passes through.
    let out = compose_turn_user_content("hello there", false, false, None);
    assert_eq!(out, "hello there");
}
use octos_llm::{
    ChatResponse, LlmError, LlmErrorKind, LlmProvider, StopReason, TokenUsage as LlmTokenUsage,
    ToolChoice,
};
use octos_memory::EpisodeStore;

#[cfg(unix)]
use crate::{AgentConfig, AgentVerifierConfig};

fn tool_use(tool_calls: Vec<ToolCall>, input_tokens: u32, output_tokens: u32) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls,
        stop_reason: StopReason::ToolUse,
        usage: LlmTokenUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn end_turn(content: &str, input_tokens: u32, output_tokens: u32) -> ChatResponse {
    ChatResponse {
        content: Some(content.to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: LlmTokenUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        },
        provider_index: None,
    }
}

struct ScriptedProvider {
    responses: StdMutex<Vec<ChatResponse>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: StdMutex::new(responses.into_iter().rev().collect()),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.responses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop()
            .ok_or_else(|| eyre::eyre!("scripted provider exhausted"))
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct StaticResultTool {
    name: &'static str,
    output: &'static str,
    success: bool,
    calls: Arc<AtomicUsize>,
}

/// Unlike the default mock stream adapter, preserve the reasoning channel.
struct TerminalScript(ScriptedProvider);

#[async_trait]
impl LlmProvider for TerminalScript {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.0.chat(messages, tools, config).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<octos_llm::ChatStream> {
        use octos_llm::StreamEvent;
        let response = self.chat(messages, tools, config).await?;
        let events = vec![
            StreamEvent::ReasoningDelta(response.reasoning_content.unwrap_or_default()),
            StreamEvent::TextDelta(response.content.unwrap_or_default()),
            StreamEvent::Usage(response.usage),
            StreamEvent::Done(response.stop_reason),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
    fn model_id(&self) -> &str {
        "terminal-test"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn terminal_integrity_reasoning_only_recovers_to_actual_answer() {
    let dir = tempfile::tempdir().unwrap();
    let mut reasoning = end_turn("", 10, 20);
    reasoning.reasoning_content = Some("Need to inspect the image.".into());
    let provider = Arc::new(TerminalScript(ScriptedProvider::new(vec![
        reasoning,
        end_turn("Actual final answer", 30, 40),
    ])));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("terminal-recovery"),
        provider.clone(),
        ToolRegistry::new(),
        memory,
    )
    .with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    });
    let result = agent
        .process_message("Inspect the image", &[], vec![])
        .await
        .unwrap();
    assert_eq!(result.content, "Actual final answer");
    assert!(provider.0.responses.lock().unwrap().is_empty());
    assert_eq!(result.token_usage.input_tokens, 40);
    assert_eq!(result.token_usage.output_tokens, 60);
}

#[tokio::test]
async fn terminal_integrity_truncated_answer_is_error_with_usage() {
    let dir = tempfile::tempdir().unwrap();
    let mut truncated = end_turn("First I need to inspect the image and then I will", 12, 7);
    truncated.stop_reason = StopReason::MaxTokens;
    let provider = Arc::new(ScriptedProvider::new(vec![truncated]));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("terminal-truncated"),
        provider,
        ToolRegistry::new(),
        memory,
    )
    .with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    });
    let error = agent
        .process_message("Inspect the image", &[], vec![])
        .await
        .expect_err("a truncated response is not a completed answer");
    assert!(error.to_string().contains("max_tokens"), "{error:?}");
    let usage = error.downcast_ref::<crate::PartialTurnUsage>().unwrap();
    assert_eq!(usage.total.input_tokens, 12);
    assert_eq!(usage.total.output_tokens, 7);
}

impl StaticResultTool {
    fn new(
        name: &'static str,
        output: &'static str,
        success: bool,
        calls: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            name,
            output,
            success,
            calls,
        }
    }
}

#[tokio::test]
async fn should_preserve_tool_carrier_text_in_durable_log_when_final_answer_repeats_it() {
    let dir = tempfile::tempdir().unwrap();
    let answer = "这次我完整读了论文原文。\n\n先纠错。\n\n论文解决了三个问题。";
    let final_content = format!("curl 被拒绝，改用 fetch 工具读取正文：\n\n{answer}");
    let mut tool_response = tool_use(
        vec![ToolCall {
            id: "call_fetch".into(),
            name: "fetch_paper".into(),
            arguments: serde_json::json!({}),
            metadata: None,
        }],
        10,
        20,
    );
    tool_response.content = Some(answer.into());
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response,
        end_turn(&final_content, 30, 40),
    ]));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fetch_paper",
        "paper body",
        true,
        calls.clone(),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent =
        Agent::new(AgentId::new("dedupe-test"), provider, tools, memory).with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        });

    let response = agent
        .process_message("请读这篇论文", &[], vec![])
        .await
        .expect("turn should complete");

    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(response.content, final_content);
    assert_eq!(response.assistant_segments.message_iterations, vec![(1, 1)]);
    assert_eq!(
        response.assistant_segments.final_iteration, 2,
        "final reply carries its own producer iteration, not the earlier tool carrier"
    );
    assert_eq!(
        response.clone().assistant_segments.message_iterations,
        vec![(1, 1)]
    );
    assert_eq!(response.messages.len(), 3);
    assert_eq!(response.messages[0].role, MessageRole::User);
    assert_eq!(response.messages[1].role, MessageRole::Assistant);
    assert_eq!(response.messages[1].content, answer);
    assert!(
        response.messages[1]
            .tool_calls
            .as_ref()
            .is_some_and(|tool_calls| tool_calls.len() == 1)
    );
    assert_eq!(response.messages[2].role, MessageRole::Tool);
    assert_eq!(
        response.messages[2].tool_call_id.as_deref(),
        Some("call_fetch")
    );
}

/// `(messages, tools, config)` of every provider request, in call order.
type RecordedConfigRequests =
    Arc<StdMutex<Vec<(Vec<Message>, Vec<octos_llm::ToolSpec>, ChatConfig)>>>;

/// Like [`RequestRecordingProvider`] but also keeps the `ChatConfig` of each
/// call, so a test can compare the cache-relevant request controls of the
/// checkpoint reflection with those of the action call it shadows.
struct ConfigRecordingProvider {
    responses: StdMutex<Vec<ChatResponse>>,
    requests: RecordedConfigRequests,
}

impl ConfigRecordingProvider {
    fn new(responses: Vec<ChatResponse>, requests: RecordedConfigRequests) -> Self {
        Self {
            responses: StdMutex::new(responses.into_iter().rev().collect()),
            requests,
        }
    }
}

#[async_trait]
impl LlmProvider for ConfigRecordingProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((messages.to_vec(), tools.to_vec(), config.clone()));
        self.responses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop()
            .ok_or_else(|| eyre::eyre!("scripted provider exhausted"))
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// The checkpoint request must be the action request plus appended rows:
/// same `context_management`, same reasoning effort (Anthropic derives the
/// `thinking` budget from `max_tokens`, so the output cap must not change it
/// when an effort is configured), and `tool_choice = none` on the wire.
#[tokio::test]
async fn should_send_checkpoint_with_identical_cache_relevant_config_and_tool_choice_none() {
    let dir = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = Arc::new(ConfigRecordingProvider::new(
        vec![
            tool_use(
                vec![ToolCall {
                    id: "call_fetch_1".into(),
                    name: "fetch_paper".into(),
                    arguments: serde_json::json!({ "page": 1 }),
                    metadata: None,
                }],
                10,
                20,
            ),
            tool_use(
                vec![ToolCall {
                    id: "call_fetch_2".into(),
                    name: "fetch_paper".into(),
                    arguments: serde_json::json!({ "page": 2 }),
                    metadata: None,
                }],
                10,
                20,
            ),
            end_turn("REFLECTION: keep going", 5, 5),
            end_turn("final answer", 10, 10),
        ],
        requests.clone(),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fetch_paper",
        "paper body",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("convergence-config"), provider, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            max_tokens: Some(8_192),
            reasoning_effort: Some(octos_llm::ReasoningEffort::High),
            ..Default::default()
        })
        .with_convergence_intervals(2, 100_000_000, std::time::Duration::from_secs(86_400));

    let response = agent
        .process_message("read the paper", &[], vec![])
        .await
        .expect("turn should complete");
    assert_eq!(response.content, "final answer");

    let requests = requests.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(requests.len(), 4);
    let (_, _, action) = &requests[1];
    let (_, _, checkpoint) = &requests[2];
    let (_, _, next_action) = &requests[3];
    assert!(matches!(action.tool_choice, octos_llm::ToolChoice::Auto));
    assert!(
        matches!(checkpoint.tool_choice, octos_llm::ToolChoice::None),
        "the reflection must forbid tool use on the wire"
    );
    assert!(matches!(
        next_action.tool_choice,
        octos_llm::ToolChoice::Auto
    ));
    assert_eq!(checkpoint.reasoning_effort, action.reasoning_effort);
    assert_eq!(
        checkpoint.max_tokens, action.max_tokens,
        "with a reasoning effort configured the output cap must not change the thinking budget"
    );
    assert_eq!(checkpoint.context_management, action.context_management);
    assert_eq!(checkpoint.prompt_cache_context, action.prompt_cache_context);
}

#[async_trait]
impl Tool for StaticResultTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "test tool for verifier loop tests"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(ToolResult {
            output: self.output.to_string(),
            success: self.success,
            ..Default::default()
        })
    }
}

/// Planner that keeps requesting a tool call (so the loop runs to its
/// iteration cap) and records whether it ever received the in-band budget
/// reminder (#1691).
struct BudgetProbePlanner {
    calls: Arc<AtomicUsize>,
    saw_notice: Arc<AtomicBool>,
}

#[async_trait]
impl LlmProvider for BudgetProbePlanner {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        if messages
            .iter()
            .any(|m| m.content.contains("[budget notice]"))
        {
            self.saw_notice.store(true, AtomicOrdering::SeqCst);
        }
        let n = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        // Never end the turn — force the loop toward its iteration cap.
        Ok(tool_use(
            vec![ToolCall {
                id: format!("call_{n}"),
                name: "noop_tool".to_string(),
                arguments: serde_json::json!({ "n": n }),
                metadata: None,
            }],
            10,
            5,
        ))
    }

    fn model_id(&self) -> &str {
        "budget-probe"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn budget_reminder_injected_before_iteration_cap() {
    // #1691: as a run approaches its iteration cap the model must receive an
    // in-band "wrap up and deliver" reminder, so it converges on a
    // deliverable instead of silently hitting the wall (the mini4 review
    // worker burned all 50 iterations and wrote nothing).
    let dir = tempfile::tempdir().unwrap();
    let saw_notice = Arc::new(AtomicBool::new(false));
    let planner = Arc::new(BudgetProbePlanner {
        calls: Arc::new(AtomicUsize::new(0)),
        saw_notice: saw_notice.clone(),
    });
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "noop_tool",
        "ok",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent =
        Agent::new(AgentId::new("budget-probe"), planner, tools, memory).with_config(AgentConfig {
            max_iterations: 5,
            save_episodes: false,
            ..Default::default()
        });

    let _ = agent.process_message("do a long task", &[], vec![]).await;

    assert!(
        saw_notice.load(AtomicOrdering::SeqCst),
        "the model never received the pre-cap budget reminder (#1691)"
    );
}

struct RepeatAwarePlanner {
    calls: AtomicUsize,
    saw_repeating_note: AtomicBool,
}

impl RepeatAwarePlanner {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            saw_repeating_note: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl LlmProvider for RepeatAwarePlanner {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        let saw_repeating = messages.iter().any(|message| {
            message.content.contains("[verifier]") && message.content.contains("verdict: Repeating")
        });
        if saw_repeating {
            self.saw_repeating_note.store(true, AtomicOrdering::SeqCst);
        }
        let fix_ran = messages.iter().any(|message| {
            message.role == MessageRole::Tool && message.content.contains("fixed style")
        });
        if fix_ran {
            return Ok(end_turn("fixed answer", 12, 6));
        }
        if saw_repeating {
            return Ok(tool_use(
                vec![ToolCall {
                    id: "fix_call".into(),
                    name: "fix_tool".into(),
                    arguments: serde_json::json!({"path": "style.toml", "repair": true}),
                    metadata: None,
                }],
                10,
                5,
            ));
        }
        Ok(tool_use(
            vec![ToolCall {
                id: format!("fail_call_{call}"),
                name: "fail_tool".into(),
                arguments: serde_json::json!({"path": "style.toml"}),
                metadata: None,
            }],
            10,
            5,
        ))
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct LedgerDrivenVerifier {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for LedgerDrivenVerifier {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        assert!(
            matches!(config.tool_choice, ToolChoice::None),
            "verifier call must not expose tools"
        );
        let prompt = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let verdict = if prompt.contains("tool=fix_tool") {
            r#"{"verdict":"ReadyToAnswer"}"#
        } else if prompt.contains("repeating=true") {
            r#"{"verdict":"Repeating","error_class":"ContractFail"}"#
        } else {
            r#"{"verdict":"Insufficient","reason":"need a different action"}"#
        };
        Ok(ChatResponse {
            content: Some(verdict.to_string()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: LlmTokenUsage {
                input_tokens: 3,
                output_tokens: 2,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "haiku-test"
    }

    fn provider_name(&self) -> &str {
        "mock-verifier"
    }
}

struct PrematureEndPlanner {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for PrematureEndPlanner {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        if call == 0 {
            return Ok(tool_use(
                vec![ToolCall {
                    id: "fail_once".into(),
                    name: "fail_tool".into(),
                    arguments: serde_json::json!({"path": "style.toml"}),
                    metadata: None,
                }],
                8,
                4,
            ));
        }
        if call == 1 {
            return Ok(end_turn("premature answer", 8, 4));
        }
        Ok(end_turn("ready answer", 8, 4))
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct GateVerifier {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for GateVerifier {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        let prompt = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let verdict = if prompt.contains("Proposed answer:\nready answer") {
            r#"{"verdict":"ReadyToAnswer"}"#
        } else if call == 0 {
            r#"{"verdict":"Blocked","reason":"tool failed"}"#
        } else {
            r#"{"verdict":"Insufficient","reason":"not ready yet"}"#
        };
        Ok(ChatResponse {
            content: Some(verdict.to_string()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: LlmTokenUsage {
                input_tokens: 3,
                output_tokens: 2,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "haiku-test"
    }

    fn provider_name(&self) -> &str {
        "mock-verifier"
    }
}
use crate::tools::{Tool, ToolRegistry, ToolResult};

struct MaxTokensThenEndProvider {
    calls: AtomicUsize,
    observed_prompts: Arc<StdMutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl LlmProvider for MaxTokensThenEndProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        self.observed_prompts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(
                messages
                    .iter()
                    .map(|message| message.content.clone())
                    .collect(),
            );
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(if call == 0 {
            ChatResponse {
                content: Some("part one".to_string()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::MaxTokens,
                usage: LlmTokenUsage {
                    input_tokens: 3,
                    output_tokens: 10,
                    ..Default::default()
                },
                provider_index: None,
            }
        } else {
            ChatResponse {
                content: Some("part two".to_string()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: LlmTokenUsage {
                    input_tokens: 4,
                    output_tokens: 11,
                    ..Default::default()
                },
                provider_index: None,
            }
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct NamedEchoTool {
    name: &'static str,
    output: &'static str,
}

#[async_trait]
impl Tool for NamedEchoTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Echo a fixed tool response"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        Ok(ToolResult {
            output: self.output.to_string(),
            success: true,
            ..Default::default()
        })
    }
}

struct MultiToolThenEndProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for MultiToolThenEndProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        let response = match call {
            0 => ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![
                    ToolCall {
                        id: "call_alpha".to_string(),
                        name: "alpha".to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    },
                    ToolCall {
                        id: "call_beta".to_string(),
                        name: "beta".to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    },
                ],
                stop_reason: StopReason::ToolUse,
                usage: LlmTokenUsage::default(),
                provider_index: None,
            },
            1 => ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ToolCall {
                    id: "call_gamma".to_string(),
                    name: "gamma".to_string(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: LlmTokenUsage::default(),
                provider_index: None,
            },
            _ => ChatResponse {
                content: Some("done".to_string()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: LlmTokenUsage::default(),
                provider_index: None,
            },
        };
        Ok(response)
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct TerminalFailureTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for TerminalFailureTool {
    fn name(&self) -> &str {
        "lesson_generate"
    }

    fn description(&self) -> &str {
        "Generate one complete lesson"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "tutor_context": { "type": "string" } }
        })
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(ToolResult {
            output: "lesson generation exhausted its internal attempts".to_string(),
            success: false,
            structured_metadata: Some(serde_json::json!({
                "retryable": false,
                "do_not_retry_same_turn": true
            })),
            ..Default::default()
        })
    }
}

struct TerminalLessonRetryProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for TerminalLessonRetryProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        let suffix = if call == 0 { "first" } else { "rewritten" };
        Ok(ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: format!("call_lesson_{call}"),
                name: "lesson_generate".to_string(),
                // The actual incident changed context strings on every retry.
                // The guard must key on terminal tool identity, not exact args.
                arguments: serde_json::json!({ "tutor_context": suffix }),
                metadata: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
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
async fn run_task_continues_after_max_tokens_in_same_loop() {
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let observed_prompts = Arc::new(StdMutex::new(Vec::new()));
    let provider = Arc::new(MaxTokensThenEndProvider {
        calls: AtomicUsize::new(0),
        observed_prompts: Arc::clone(&observed_prompts),
    });
    let provider_for_agent: Arc<dyn LlmProvider> = provider.clone();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("max-tokens-test"),
        provider_for_agent,
        tools,
        memory,
    );
    let task = Task::new(
        TaskKind::Code {
            instruction: "Write a long report".to_string(),
            files: vec![],
        },
        TaskContext {
            working_dir: dir.path().to_path_buf(),
            ..Default::default()
        },
    );

    let result = agent.run_task(&task).await.unwrap();

    assert!(result.success);
    assert_eq!(
        result.output,
        "part one
part two"
    );
    assert_eq!(provider.calls.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(result.token_usage.input_tokens, 7);
    assert_eq!(result.token_usage.output_tokens, 21);
    let prompts = observed_prompts
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].iter().any(|content| content == "part one"));
    assert!(
        prompts[1]
            .iter()
            .any(|content| content.contains("Continue directly from where you stopped"))
    );
}

#[tokio::test]
async fn process_message_preserves_tool_pair_order_across_iterations() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(NamedEchoTool {
        name: "alpha",
        output: "alpha ok",
    });
    tools.register(NamedEchoTool {
        name: "beta",
        output: "beta ok",
    });
    tools.register(NamedEchoTool {
        name: "gamma",
        output: "gamma ok",
    });

    let provider: Arc<dyn LlmProvider> = Arc::new(MultiToolThenEndProvider {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory);

    let result = agent.process_message("do work", &[], vec![]).await.unwrap();
    let roles: Vec<MessageRole> = result.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Tool,
            MessageRole::Assistant,
            MessageRole::Tool,
        ]
    );
    assert_eq!(result.content, "done");
    assert_eq!(result.messages[1].tool_calls.as_ref().unwrap().len(), 2);
    assert_eq!(result.messages[4].tool_calls.as_ref().unwrap().len(), 1);
    assert_eq!(
        result.messages[2].tool_call_id.as_deref(),
        Some("call_alpha")
    );
    assert_eq!(
        result.messages[3].tool_call_id.as_deref(),
        Some("call_beta")
    );
    assert_eq!(
        result.messages[5].tool_call_id.as_deref(),
        Some("call_gamma")
    );
}

#[tokio::test]
async fn terminal_tool_failure_blocks_a_rewritten_retry_in_the_same_turn() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(TerminalFailureTool {
        calls: Arc::clone(&calls),
    });

    let provider: Arc<dyn LlmProvider> = Arc::new(TerminalLessonRetryProvider {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory);

    let result = agent
        .process_message("teach me", &[], vec![])
        .await
        .unwrap();

    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(
        result.content,
        terminal_tool_retry_message("lesson_generate"),
    );
    assert_eq!(
        result
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .count(),
        1,
        "the rewritten second call must be stopped before it creates another tool result",
    );
}

#[test]
fn split_tool_calls_caps_parallel_batches() {
    let tool_calls: Vec<ToolCall> = (0..9)
        .map(|i| ToolCall {
            id: format!("call_{i}"),
            name: format!("tool_{i}"),
            arguments: serde_json::json!({}),
            metadata: None,
        })
        .collect();

    let batches = split_tool_calls(&tool_calls, MAX_PARALLEL_TOOL_CALLS_PER_BATCH);
    let batch_sizes: Vec<_> = batches.iter().map(|batch| batch.len()).collect();

    assert_eq!(batch_sizes, vec![8, 1]);
    assert_eq!(batches[0][0].id, "call_0");
    assert_eq!(batches[1][0].id, "call_8");
}

#[test]
fn recover_shell_retry_output_prefers_diff_like_success() {
    let messages = vec![
            Message::user("show a diff"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "git diff -- notes.txt"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "fatal: not a git repository\n\nExit code: 128".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_2".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cd /tmp && git diff -- notes.txt"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "diff --git a/notes.txt b/notes.txt\n--- a/notes.txt\n+++ b/notes.txt\n@@ -1,2 +1,2 @@\n alpha\n-beta\n+gamma\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_3".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "git status --short"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "(no output)\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_4".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "git diff -- notes.txt"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "fatal: not a git repository\n\nExit code: 128".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

    let recovered = recover_shell_retry(&messages, 4).expect("should recover");
    assert_eq!(recovered.kind, ShellRetryRecoveryKind::DiffLikeSuccess);
    assert!(recovered.content.contains("diff --git"));
    assert!(!recovered.content.contains("Exit code: 0"));
}

#[test]
fn recover_shell_retry_output_requires_failure_before_useful_success() {
    let messages = vec![
        Message::user("inspect the repo"),
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: "call_shell_1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "pwd"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Tool,
            content: "/tmp/octos\n\nExit code: 0".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_shell_1".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: "call_shell_2".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "ls src"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Tool,
            content: "lib.rs\nmain.rs\n\nExit code: 0".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_shell_2".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: "call_shell_3".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "git status --short"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Tool,
            content: " M src/lib.rs\n?? notes.txt\n\nExit code: 0".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_shell_3".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: "call_shell_4".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "cat Cargo.toml"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Tool,
            content: "[package]\nname = \"octos\"\n\nExit code: 0".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_shell_4".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
    ];

    assert!(recover_shell_retry(&messages, 4).is_none());
}

// ── Fix #1+#2 (2026-05-10, codex r2): intra-turn scoping + correct splice ─

/// `current_user_turn_start` returns the index of the most recent User
/// message — the slice from there onward is the current turn, the
/// scan window for the spiral detector.
#[test]
fn current_user_turn_start_returns_index_of_last_user_message() {
    let mut messages = stale_shell_failure_streak("call_shell");
    // first User is at index 0; nothing else; so current_user_turn_start
    // returns 0.
    assert_eq!(current_user_turn_start(&messages), 0);

    // Push a NEW user message simulating a new turn the user types
    // after the original streak.
    messages.push(Message::user("now ask me about weather"));
    let new_user_idx = messages.len() - 1;
    assert_eq!(current_user_turn_start(&messages), new_user_idx);
}

/// Multi-tool batch awareness: the LLM can emit
/// `[shell, read_file]` in a single response. Both Tool results are
/// appended consecutively. The gate must see "this batch contains
/// shell" — checking only the latest Tool name would suppress
/// legitimate detection.
#[test]
fn latest_tool_batch_contains_picks_up_shell_in_mixed_batch() {
    let messages = vec![
        Message::user("repair"),
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_shell".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                    metadata: None,
                },
                ToolCall {
                    id: "call_read".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "x"}),
                    metadata: None,
                },
            ]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Tool,
            content: "failed".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_shell".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Tool,
            content: "{ \"x\": 1 }".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_read".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
    ];

    assert!(latest_tool_batch_contains(&messages, "shell"));
    assert!(latest_tool_batch_contains(&messages, "read_file"));
}

/// Regression for the 2026-05-10 mini1 incident. A session that
/// accumulated a 4-call shell streak with failures in turn N must NOT
/// have turn N+1 force-ended when turn N+1 (a) starts with a fresh
/// User message and (b) only ran `read_file`.
///
/// With Fix #1 v2 (intra-turn window scan), `recover_shell_retry`
/// applied to the windowed slice from the new User message onward
/// sees zero shell calls — the threshold (4) is not met — so the
/// detector returns None at the SCAN layer. The batch-aware gate is
/// belt-and-suspenders for the case of mixed batches.
#[test]
fn intra_turn_window_skips_stale_shell_history_from_prior_turn() {
    let mut messages = stale_shell_failure_streak("call_shell");
    // New user turn after the stale streak.
    messages.push(Message::user("now read manifest.json"));
    // This turn ran read_file only.
    messages.push(Message {
        role: MessageRole::Assistant,
        content: String::new(),
        media: vec![],
        tool_calls: Some(vec![ToolCall {
            id: "call_read_now".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "manifest.json"}),
            metadata: None,
        }]),
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    });
    messages.push(Message {
        role: MessageRole::Tool,
        content: "{ ... 6kb manifest ... }".into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: Some("call_read_now".into()),
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    });

    // Whole-history scan still matches the stale streak — that's the
    // BUG we're fixing. The window is what restores correctness.
    assert!(recover_shell_retry(&messages, 4).is_some());

    let window_start = current_user_turn_start(&messages);
    let window = &messages[window_start..];
    // Inside the new-turn window, there are zero shell calls.
    assert!(!latest_tool_batch_contains(window, "shell"));
    // ...so the windowed scan finds no streak.
    assert!(recover_shell_retry(window, 4).is_none());
}

/// Codex round-2 #e: terminal RetryLimit + Exhausted user message
/// must not be the raw system-shaped instruction. The sanitizer
/// strips the prefix and frames the latest output for the user.
#[test]
fn shell_retry_terminal_user_message_strips_system_prefix() {
    let raw = "[SHELL RETRY LIMIT] Repeated shell repair attempts did not converge. Stop retrying shell and summarize the blocker.\n\nLatest shell output:\nerror: could not find Cargo.toml\n\nExit code: 101";
    let sanitized = shell_retry_terminal_user_message(raw);
    assert!(!sanitized.contains("[SHELL RETRY LIMIT]"));
    assert!(!sanitized.contains("Stop retrying shell and summarize"));
    assert!(sanitized.contains("could not find Cargo.toml"));
    assert!(
        sanitized.starts_with("I tried multiple shell approaches"),
        "expected user-facing framing, got: {sanitized}"
    );
}

/// Helper: builds a 4-call shell-streak with all failures, exactly the
/// shape the live mini1 session had at 19:35–19:36 PDT on 2026-05-10
/// before the user asked unrelated questions.
fn stale_shell_failure_streak(id_prefix: &str) -> Vec<Message> {
    let mut out = vec![Message::user("repair the repo")];
    for i in 1..=4 {
        let id = format!("{id_prefix}_{i}");
        out.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: id.clone(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "cargo test"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
        out.push(Message {
            role: MessageRole::Tool,
            content: "error: could not find Cargo.toml\n\nExit code: 101".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(id),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
    }
    out
}

#[tokio::test]
async fn verifier_repeating_note_changes_next_planner_action() {
    let dir = tempfile::tempdir().unwrap();
    let ledger_path = dir.path().join("turn_ledger.jsonl");
    let planner = Arc::new(RepeatAwarePlanner::new());
    let verifier = Arc::new(LedgerDrivenVerifier {
        calls: AtomicUsize::new(0),
    });
    let fail_calls = Arc::new(AtomicUsize::new(0));
    let fix_calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fail_tool",
        "[VALIDATION FAILED] style TOML is malformed",
        false,
        fail_calls.clone(),
    ));
    tools.register(StaticResultTool::new(
        "fix_tool",
        "fixed style\n\nExit code: 0",
        true,
        fix_calls.clone(),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("verifier-repeat"),
        planner.clone(),
        tools,
        memory,
    )
    .with_config(AgentConfig {
        max_iterations: 12,
        save_episodes: false,
        ..Default::default()
    })
    .with_verifier_config(
        AgentVerifierConfig::with_provider(verifier.clone(), "haiku-test")
            .with_ledger_path(&ledger_path),
    );

    let response = agent
        .process_message("repair the generated style", &[], vec![])
        .await
        .unwrap();

    assert_eq!(response.content, "fixed answer");
    assert!(
        planner.saw_repeating_note.load(AtomicOrdering::SeqCst),
        "planner must observe the injected Repeating verifier note"
    );
    assert_eq!(fail_calls.load(AtomicOrdering::SeqCst), 3);
    assert_eq!(fix_calls.load(AtomicOrdering::SeqCst), 1);
    assert!(
        verifier.calls.load(AtomicOrdering::SeqCst) >= 4,
        "verifier should classify failures and the ready gate"
    );
    let persisted = std::fs::read_to_string(&ledger_path).expect("turn ledger persisted");
    assert!(persisted.contains("\"tool\":\"fail_tool\""));
    assert!(persisted.contains("\"tool\":\"fix_tool\""));
}

#[tokio::test]
async fn verifier_ready_to_answer_gates_endturn_after_problem_signal() {
    let dir = tempfile::tempdir().unwrap();
    let planner = Arc::new(PrematureEndPlanner {
        calls: AtomicUsize::new(0),
    });
    let verifier = Arc::new(GateVerifier {
        calls: AtomicUsize::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fail_tool",
        "[VALIDATION FAILED] artifact missing",
        false,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("verifier-gate"),
        planner.clone(),
        tools,
        memory,
    )
    .with_config(AgentConfig {
        max_iterations: 8,
        save_episodes: false,
        ..Default::default()
    })
    .with_verifier_config(AgentVerifierConfig::with_provider(
        verifier.clone(),
        "haiku-test",
    ));

    let response = agent
        .process_message("try then answer too early", &[], vec![])
        .await
        .unwrap();

    assert_eq!(response.content, "ready answer");
    assert!(
        planner.calls.load(AtomicOrdering::SeqCst) >= 3,
        "premature EndTurn must be rejected until ReadyToAnswer"
    );
    assert!(
        verifier.calls.load(AtomicOrdering::SeqCst) >= 3,
        "failure classification plus two termination checks expected"
    );
}

// ── is_productive_tool_message (M6.2) ───────────────────────────────

#[test]
fn productive_message_rejects_known_failure_prefixes() {
    assert!(!is_productive_tool_message("Error: boom"));
    assert!(!is_productive_tool_message("[HOOK DENIED] blocked"));
    assert!(!is_productive_tool_message("[SESSION LIMIT] cap"));
    assert!(!is_productive_tool_message("[SHELL RETRY LIMIT] stop"));
    assert!(!is_productive_tool_message(
        "Path outside working directory: /etc/passwd"
    ));
    assert!(!is_productive_tool_message("(no output)"));
    assert!(!is_productive_tool_message("File not found: missing.txt"));
    assert!(!is_productive_tool_message(
        "Tool 'shell' panicked: bad state"
    ));
    assert!(!is_productive_tool_message(
        "Tool 'shell' timed out after 30 seconds"
    ));
}

// ─────────────────────────────────────────────────────────────────────
// Review A F-001 — dispatch_loop_error wiring.
// ─────────────────────────────────────────────────────────────────────

/// Minimal placeholder provider for F-001 dispatch tests. The tests drive
/// `handle_loop_error_with_dispatch` directly and never call `chat()`, so
/// the provider's only requirement is to satisfy the trait bounds.
struct InertProvider;

#[async_trait]
impl LlmProvider for InertProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        unreachable!("InertProvider::chat must not be called in F-001 dispatch tests");
    }

    fn model_id(&self) -> &str {
        "inert"
    }

    fn provider_name(&self) -> &str {
        "inert"
    }
}

/// Counting summarizer used to prove the `CompactAndRetry` arm of
/// `handle_loop_error_with_dispatch` actually drives `maybe_run_turn_compaction`.
struct CountingSummarizer {
    calls: Arc<AtomicUsize>,
}

impl crate::summarizer::Summarizer for CountingSummarizer {
    fn kind(&self) -> &'static str {
        "counting_spy"
    }

    fn summarize(&self, messages: &[Message], budget_tokens: u32) -> Result<String> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(crate::compaction::compact_messages(messages, budget_tokens))
    }
}

async fn build_dispatch_test_agent() -> Agent {
    let dir = tempfile::tempdir().unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(InertProvider);
    let tools = ToolRegistry::new();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    Agent::new(AgentId::new("test-dispatch"), provider, tools, memory)
}

// ─────────────────────────────────────────────────────────────────────
// M8.10-C — LOOP DETECTED dedup.
// ─────────────────────────────────────────────────────────────────────

/// Mock LLM that always returns the same shell tool call with the same
/// arguments, forcing the loop detector to fire on iteration 4.
struct AlwaysSameToolProvider;

#[async_trait]
impl LlmProvider for AlwaysSameToolProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        Ok(ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "call_loop".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "loopy.txt"}),
                metadata: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
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

async fn build_agent_with_mock(dir: &std::path::Path) -> Agent {
    let tools = ToolRegistry::with_builtins(dir);
    let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysSameToolProvider);
    let memory = Arc::new(EpisodeStore::open(dir.join("memory")).await.unwrap());
    Agent::new(AgentId::new("loop-dedup"), provider, tools, memory)
}

#[tokio::test]
async fn dedup_loop_warning_returns_warning_on_first_fire() {
    let dir = tempfile::tempdir().unwrap();
    let agent = build_agent_with_mock(dir.path()).await;

    assert!(!agent.is_loop_detected_recently());
    let result = agent.dedup_loop_warning("[LOOP DETECTED] cycle".to_string());
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "[LOOP DETECTED] cycle");
    assert!(agent.is_loop_detected_recently());
}

#[tokio::test]
async fn dedup_loop_warning_returns_terminal_error_on_second_fire() {
    let dir = tempfile::tempdir().unwrap();
    let agent = build_agent_with_mock(dir.path()).await;

    let first = agent.dedup_loop_warning("[LOOP DETECTED] one".to_string());
    assert!(first.is_ok());
    let second = agent.dedup_loop_warning("[LOOP DETECTED] two".to_string());
    assert!(second.is_err());
    let err = second.err().unwrap().to_string();
    assert!(
        err.contains("agent loop got stuck"),
        "expected terminal error, got: {err}"
    );
    // Flag stays set after the terminal error so further fires keep
    // returning terminal errors until the next process_message reset.
    assert!(agent.is_loop_detected_recently());
}

#[tokio::test]
async fn shell_spiral_dispatch_marks_loop_detected_recently() {
    // #1656: a firing shell spiral must mark the two-stage dedup flag, so a
    // generic loop detection later in the SAME turn is treated as the second
    // fire (terminal) instead of restarting the warn-then-terminate ladder.
    let dir = tempfile::tempdir().unwrap();
    let agent = build_agent_with_mock(dir.path()).await;

    // Four consecutive failing shell exchanges inside the current user turn —
    // the spiral detector's threshold.
    let mut messages = vec![Message::user("fix the build")];
    for i in 0..4 {
        let call_id = format!("call_shell_{i}");
        messages.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: call_id.clone(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "cargo build"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
        messages.push(Message {
            role: MessageRole::Tool,
            content: "error[E0999]: broken\n\nExit code: 101".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(call_id),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
    }

    assert!(!agent.is_loop_detected_recently());
    let mut retry_state = LoopRetryState::new();
    let outcome = agent.dispatch_shell_retry_recovery(&messages, &mut retry_state, 1);
    assert!(
        outcome.is_some(),
        "the spiral must fire on 4 failing shells"
    );
    assert!(
        agent.is_loop_detected_recently(),
        "a firing spiral must mark the loop-detected flag (#1656)"
    );

    // The NEXT generic loop detection in this turn is the SECOND fire —
    // terminal, not a fresh warning.
    let second = agent.dedup_loop_warning("[LOOP DETECTED] generic".to_string());
    assert!(
        second.is_err(),
        "generic detection after a spiral must be terminal, got {second:?}"
    );
}

// ─── Back to Review A F-001 dispatch tests ───────────────────────────

#[tokio::test]
async fn should_compact_and_retry_on_context_overflow() {
    // F-001 coverage #1: a ContextOverflow error must drive the
    // CompactAndRetry arm, which runs `maybe_run_turn_compaction` (via
    // the wired CompactionRunner) and returns Retry so the outer loop
    // continues instead of bailing.
    use crate::compaction::{CompactionPolicy, CompactionRunner};
    use crate::workspace_policy::{CompactionSummarizerKind, WorkspacePolicy};

    let policy = CompactionPolicy {
        schema_version: crate::abi_schema::COMPACTION_POLICY_SCHEMA_VERSION,
        // Budget sized so recent+system fits (≈6 kept messages at 400
        // words ≈ 2.4k tokens) but overall messages still overflow the
        // budget, which forces the runner into its summarise branch
        // rather than the fallback-trim branch.
        token_budget: 8_000,
        preflight_threshold: Some(1_000),
        prune_tool_results_after_turns: None,
        preserved_artifacts: vec![],
        preserved_invariants: vec![],
        summarizer: CompactionSummarizerKind::Extractive,
    };
    let spy = Arc::new(AtomicUsize::new(0));
    let runner =
        CompactionRunner::new(policy).with_summarizer(CountingSummarizer { calls: spy.clone() });
    let workspace = WorkspacePolicy::for_session();
    let agent = build_dispatch_test_agent()
        .await
        .with_compaction_runner(Arc::new(runner))
        .with_compaction_workspace(workspace);

    let mut retry_state = LoopRetryState::new();
    // Build an eyre::Report wrapping a typed LlmError so the harness
    // classifier downcasts it to HarnessError::ContextOverflow rather
    // than the Internal fallback.
    let raw_error: eyre::Report = LlmError::new(
        LlmErrorKind::ContextOverflow {
            limit: Some(200_000),
            used: Some(201_000),
        },
        "prompt too long for model window",
    )
    .into();

    // Conversation large enough that the compaction runner enters its
    // summarise branch rather than the oldest-first fallback trim.
    let filler = "word ".repeat(400);
    let mut messages = vec![Message {
        role: MessageRole::System,
        content: "sys".to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    }];
    for i in 0..14 {
        messages.push(Message {
            role: MessageRole::User,
            content: format!("turn {i} user question {filler}"),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
        messages.push(Message {
            role: MessageRole::Assistant,
            content: format!("turn {i} assistant reply {filler}"),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
    }

    // iteration=2 so maybe_run_turn_compaction actually runs (iteration=1
    // is reserved for the preflight path).
    let action =
        agent.handle_loop_error_with_dispatch(&raw_error, &mut retry_state, 2, &mut messages);
    assert_eq!(
        action,
        LoopErrorAction::Retry,
        "ContextOverflow must land on the Retry arm after compaction"
    );
    assert!(
        spy.load(AtomicOrdering::SeqCst) >= 1,
        "CompactAndRetry must invoke maybe_run_turn_compaction → summarizer at least once; got {}",
        spy.load(AtomicOrdering::SeqCst)
    );
    assert_eq!(
        retry_state.counters().context_overflow,
        1,
        "first ContextOverflow observation must bump the bucket counter once"
    );
}

#[tokio::test]
async fn should_escalate_when_bucket_exhausted() {
    // F-001 coverage #2: once the retry bucket for a variant is
    // saturated, the next observation MUST land on the Bail arm so the
    // caller surfaces Err(report) instead of looping. Pre-fix the
    // classified error was ignored and only Escalate was reachable;
    // Exhausted was dead.
    let agent = build_dispatch_test_agent().await;
    let mut retry_state = LoopRetryState::with_limits(crate::agent::loop_state::LoopRetryLimits {
        rate_limited: 1,
        ..Default::default()
    });
    let mut messages: Vec<Message> = Vec::new();

    // First observation: transient rate-limit → Continue → Retry.
    // Typed LlmError so classify_report maps to RateLimited rather than
    // the Internal fallback.
    let rate_limit_error: eyre::Report = LlmError::rate_limited(Some(2)).into();
    let first_action = agent.handle_loop_error_with_dispatch(
        &rate_limit_error,
        &mut retry_state,
        1,
        &mut messages,
    );
    assert_eq!(
        first_action,
        LoopErrorAction::Retry,
        "first rate-limit observation must land on Retry"
    );

    // Second observation: bucket exhausted (limit=1) → Exhausted → Bail.
    let second_action = agent.handle_loop_error_with_dispatch(
        &rate_limit_error,
        &mut retry_state,
        2,
        &mut messages,
    );
    assert_eq!(
        second_action,
        LoopErrorAction::Bail,
        "exhausted rate-limit bucket must land on Bail so the outer loop surfaces Err"
    );
    assert!(
        retry_state.counters().rate_limited >= 2,
        "bucket must be bumped for every observation, not just the first",
    );
}

#[tokio::test]
async fn should_bail_on_authentication_error_without_compaction() {
    // F-001 coverage #3: FailFast-hint variants (Authentication) must
    // land on Bail immediately, regardless of whether a compaction
    // runner is wired. Proves the Escalate arm reaches Bail.
    let agent = build_dispatch_test_agent().await;
    let mut retry_state = LoopRetryState::new();
    let mut messages: Vec<Message> = Vec::new();

    let auth_error: eyre::Report = LlmError::auth("invalid API key").into();
    let action =
        agent.handle_loop_error_with_dispatch(&auth_error, &mut retry_state, 1, &mut messages);
    assert_eq!(
        action,
        LoopErrorAction::Bail,
        "Authentication errors must never retry; they must bail"
    );
}

// ─────────────────────────────────────────────────────────────────────
// PR `fix/news-fetch-loop-and-detect-recovery` —
// LOOP DETECTED non-terminal recovery (`session web-1779494658716-mxrxe8`,
// ledger seq 214-562). On first fire we now inject a synthetic tool
// result carrying the warning and continue the loop for one more LLM
// iteration; on second fire we return a terminal `ConversationResponse`.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn inject_synthetic_results_pushes_assistant_then_tool_for_every_call() {
    let response = ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls: vec![
            ToolCall {
                id: "call_a".to_string(),
                name: "news_fetch".to_string(),
                arguments: serde_json::json!({"categories": ["tech"]}),
                metadata: None,
            },
            ToolCall {
                id: "call_b".to_string(),
                name: "news_fetch".to_string(),
                arguments: serde_json::json!({"categories": ["world"]}),
                metadata: None,
            },
        ],
        stop_reason: StopReason::ToolUse,
        usage: LlmTokenUsage::default(),
        provider_index: None,
    };

    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysSameToolProvider);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let memory = runtime
        .block_on(async { Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap()) });
    let agent = Agent::new(AgentId::new("inject-test"), provider, tools, memory);

    let mut messages: Vec<Message> = Vec::new();
    super::super::loop_runner::inject_loop_detected_synthetic_results(
        &mut messages,
        &response,
        "[LOOP DETECTED] cycle length 1.",
        &agent,
    );

    // 1 assistant + 2 tool results (one per tool_call).
    assert_eq!(messages.len(), 3, "expected 1 assistant + 2 tool results");
    assert_eq!(messages[0].role, MessageRole::Assistant);
    assert_eq!(
        messages[0]
            .tool_calls
            .as_ref()
            .map(|tcs| tcs.len())
            .unwrap_or(0),
        2,
        "assistant message must carry the looping tool_calls so providers \
             can bind the synthetic tool-result messages back to them"
    );

    for (idx, msg) in messages[1..].iter().enumerate() {
        assert_eq!(msg.role, MessageRole::Tool, "tool message #{idx}");
        let id_expected = if idx == 0 { "call_a" } else { "call_b" };
        assert_eq!(msg.tool_call_id.as_deref(), Some(id_expected));
    }

    // First tool-result carries the warning + synthesis hint; second is
    // a short companion stub so the LLM doesn't think the second call
    // actually executed.
    assert!(
        messages[1].content.contains("[LOOP DETECTED]"),
        "primary tool result must echo the warning: got `{}`",
        messages[1].content
    );
    assert!(
        messages[1].content.contains("synthesise")
            || messages[1].content.contains("different tool"),
        "primary tool result must contain a synthesis hint so the LLM \
             knows how to course-correct: got `{}`",
        messages[1].content
    );
    assert!(
        messages[2].content.contains("[LOOP DETECTED]")
            && messages[2].content.contains("companion"),
        "companion tool result should mark itself as such: got `{}`",
        messages[2].content
    );
}

#[test]
fn loop_detected_terminal_message_is_user_facing_and_non_empty() {
    let msg = super::super::loop_runner::loop_detected_terminal_message();
    assert!(msg.contains("[LOOP DETECTED]"));
    assert!(
        msg.contains("rephrase") || msg.contains("different angle"),
        "terminal message should guide the user to rephrase: got `{msg}`"
    );
}

/// LLM mock that always returns the SAME tool call so the loop
/// detector fires repeatedly. Counts invocations so the test can
/// assert how many LLM calls happened across the recovery window.
struct CountingAlwaysSameToolProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for CountingAlwaysSameToolProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "call_loopy".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "loopy.txt"}),
                metadata: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
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
async fn doom_loop_aborts_turn_when_third_identical_call_arrives() {
    // #1765 doom-loop guard: when the LLM issues the SAME tool call
    // (same name + identical arguments JSON) 3 times in a row, the
    // conversation loop must abort the turn with a clear message
    // instead of issuing the next LLM call.
    //
    // We assert via:
    //   - The terminal `content` matches `doom_loop_terminal_message`
    //     (proves the doom guard fired, not the older two-stage
    //     warn-then-terminate cycle path).
    //   - The mock LLM was called EXACTLY 3 times: calls 1 and 2
    //     execute the tool; the 3rd identical call trips the guard and
    //     no further LLM call is issued.
    //   - The flag (`is_loop_detected_recently`) is set after the run.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("loopy.txt"), b"x").unwrap();
    let provider = Arc::new(CountingAlwaysSameToolProvider {
        calls: AtomicUsize::new(0),
    });
    let provider_arc: Arc<dyn LlmProvider> = provider.clone();
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("recover"), provider_arc, tools, memory).with_config(
        crate::AgentConfig {
            max_iterations: 30,
            save_episodes: false,
            ..Default::default()
        },
    );

    let result = agent
        .process_message("please loop", &[], vec![])
        .await
        .expect("process_message should return Ok even when the doom guard aborts");

    assert_eq!(
        result.content,
        doom_loop_terminal_message("read_file", 3),
        "expected the doom-loop abort message when the 3rd identical \
             call arrives"
    );
    assert!(agent.is_loop_detected_recently());

    let total_calls = provider.calls.load(AtomicOrdering::SeqCst);
    assert_eq!(
        total_calls, 3,
        "expected exactly 3 LLM calls — the doom guard must stop the \
             loop instead of issuing a 4th; got {total_calls}"
    );
}
/// LLM mock that alternates between two different argument sets for the
/// same tool. The doom guard (consecutive identical) must never fire;
/// the cycle detector (`LoopDetector::record`) owns alternating
/// patterns and still runs its two-stage warn-then-terminate recovery.
struct CountingAlternatingArgsProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for CountingAlternatingArgsProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        let n = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        let path = if n % 2 == 0 { "a.txt" } else { "b.txt" };
        Ok(ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: format!("call_alt_{n}"),
                name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": path }),
                metadata: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
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
async fn alternating_cycle_still_uses_two_stage_warning_not_doom_abort() {
    // #1765: the doom guard counts CONSECUTIVE identical calls only —
    // an A,B,A,B,… alternation resets the streak every call, so the
    // existing cycle detector must keep owning that pattern with its
    // two-stage recovery (first fire injects a warning + one more LLM
    // iteration; second fire terminates with
    // `loop_detected_terminal_message`).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
    let provider = Arc::new(CountingAlternatingArgsProvider {
        calls: AtomicUsize::new(0),
    });
    let provider_arc: Arc<dyn LlmProvider> = provider.clone();
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("recover"), provider_arc, tools, memory).with_config(
        crate::AgentConfig {
            max_iterations: 30,
            save_episodes: false,
            ..Default::default()
        },
    );

    let result = agent
        .process_message("please alternate", &[], vec![])
        .await
        .expect("process_message should return Ok when the cycle detector terminates");

    assert_eq!(
        result.content,
        loop_detected_terminal_message(),
        "alternating A,B cycles belong to the two-stage cycle detector, \
             not the doom guard"
    );
    let total_calls = provider.calls.load(AtomicOrdering::SeqCst);
    assert!(
        total_calls >= 7,
        "expected >= 7 LLM calls (6 to reach a cycle-2 first fire + 1 \
             recovery iteration before the terminating fire); got {total_calls}"
    );
}

/// LLM stub that always returns a single EndTurn — used to drive
/// `run_task` straight to the completion branch without iterating
/// through tool calls.
struct EndTurnOnlyProvider;
#[async_trait]
impl LlmProvider for EndTurnOnlyProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        Ok(ChatResponse {
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: LlmTokenUsage::default(),
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
async fn run_task_keeps_success_when_workspace_has_no_policy() {
    // No-policy workspace must stay Success (no regression).
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(EndTurnOnlyProvider);
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("no-contract"), provider, tools, memory);
    let task = Task::new(
        TaskKind::Code {
            instruction: "Hi".into(),
            files: vec![],
        },
        TaskContext {
            working_dir: dir.path().to_path_buf(),
            ..Default::default()
        },
    );

    let result = agent.run_task(&task).await.unwrap();
    assert!(
        result.success,
        "no-policy workspace must keep Success (got {:?})",
        result.output
    );
}

// ── Fleet-UX soak B4 (mini1 / dspfac, 2026-05-22) ─────────────────
//
// Suite for the spawn_only synthesized-ack suppression. When the LLM
// calls a spawn_only tool whose dispatcher returns an error, the agent
// must NOT fabricate a "Background work started for `<tool>`."
// acknowledgement — the user already sees a red error chip on the tool
// card and the synthesized ack reads as a confusing dual signal.

#[test]
fn is_error_tool_message_classifies_error_envelopes() {
    // Positive cases — every well-known error convention emitted by
    // crate::agent::execution must classify as an error.
    assert!(is_error_tool_message("Error: tool dispatch failed"));
    assert!(is_error_tool_message(
        "[VALIDATION FAILED] Tool 'bg_research' rejected input: bad DOT"
    ));
    assert!(is_error_tool_message(
        "[POLICY DENIED] Tool 'foo' is blocked by provider policy (deny)"
    ));
    assert!(is_error_tool_message(
        "[HOOK DENIED] Tool 'foo' was blocked by a lifecycle hook."
    ));
    assert!(is_error_tool_message("[SESSION LIMIT] cap"));
    assert!(is_error_tool_message("[SHELL RETRY LIMIT] stop"));
    assert!(is_error_tool_message("Tool 'foo' panicked: boom"));
    assert!(is_error_tool_message(
        "Tool 'foo' timed out after 30 seconds"
    ));
    assert!(is_error_tool_message(
        "Tool 'foo' cancelled due to earlier sibling error in the same batch."
    ));

    // Leading whitespace must not defeat the prefix check.
    assert!(is_error_tool_message("   Error: trimmed"));

    // Negative cases — successful and neutral bodies must NOT be flagged.
    assert!(!is_error_tool_message(""));
    assert!(!is_error_tool_message("   "));
    assert!(!is_error_tool_message("ok"));
    assert!(!is_error_tool_message(
        "{\"task_handle\": \"abc\", \"output_dir\": \"/tmp\"}"
    ));
    assert!(!is_error_tool_message(
        "Background research kicked off; results pending."
    ));
    // A "Tool '...'" message that doesn't match panicked/timed-out/
    // cancelled-due-to-earlier is informational, not an error envelope.
    assert!(!is_error_tool_message(
        "Tool 'spawn' produced files: report.md"
    ));
}

fn spawn_only_tool_call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: serde_json::json!({}),
        metadata: None,
    }
}

fn spawn_only_tool_result(tool_call_id: &str, content: &str) -> Message {
    Message {
        role: MessageRole::Tool,
        content: content.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: Some(tool_call_id.to_string()),
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    }
}

fn spawn_only_chat_response(tool_calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls,
        stop_reason: StopReason::ToolUse,
        usage: LlmTokenUsage::default(),
        provider_index: None,
    }
}

#[test]
fn any_tool_invocation_errored_detects_error_envelope() {
    let response = spawn_only_chat_response(vec![spawn_only_tool_call("call_1", "any_tool")]);
    let messages = vec![spawn_only_tool_result(
        "call_1",
        "Error: any_tool dispatch failed",
    )];

    // Empty success-map exercises the content-classifier fallback path
    // (the success bit is the post-#1187 authoritative input; absence
    // means the call bypassed execute_tools, e.g. session-limit block).
    assert!(any_tool_invocation_errored(&messages, &response, &[]));
}

#[test]
fn any_tool_invocation_errored_mixed_batch_one_failed() {
    // The realistic production shape: spawn_only tool returned its
    // task-handle envelope (foreground always reports success for
    // spawn_only) AND a sibling regular tool errored in the same batch.
    // The gate MUST fire so the synthesized "Background work started"
    // ack is suppressed — otherwise the user sees a successful-looking
    // ack alongside the red error chip from the sibling tool.
    let response = spawn_only_chat_response(vec![
        spawn_only_tool_call("call_pipeline", "bg_research"),
        spawn_only_tool_call("call_shell", "shell"),
    ]);
    let messages = vec![
        spawn_only_tool_result(
            "call_pipeline",
            "{\"task_handle\": \"deep-research-xyz\", \"output_dir\": \"/tmp/dr\"}",
        ),
        spawn_only_tool_result("call_shell", "Error: command not found: foo"),
    ];

    assert!(any_tool_invocation_errored(&messages, &response, &[]));
}

/// Provider that always fails with a (typed) server-side error. A 5xx
/// ServerError classifies as a retriable LLM error without tripping the
/// agent's `1 << attempt` backoff under FailFast (retry_max = 0).
struct AlwaysErrorProvider {
    chat_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for AlwaysErrorProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.chat_calls.fetch_add(1, AtomicOrdering::SeqCst);
        Err(LlmError::new(
            LlmErrorKind::ServerError { status: 503 },
            "provider unavailable",
        )
        .into())
    }

    fn model_id(&self) -> &str {
        "mock-error"
    }

    fn provider_name(&self) -> &str {
        "mock-error"
    }
}

fn task_for(instruction: &str, dir: &std::path::Path) -> Task {
    Task::new(
        TaskKind::Code {
            instruction: instruction.to_string(),
            files: vec![],
        },
        TaskContext {
            working_dir: dir.to_path_buf(),
            ..Default::default()
        },
    )
}

/// #2249 — a `before_llm_call` deny is expected policy behaviour, not a
/// harness bug: the classified error event must carry `variant="policy"
/// recovery="expected"` so operator dashboards stop paging on it.
#[tokio::test]
#[cfg(unix)]
async fn should_classify_hook_deny_as_policy_not_internal_bug() {
    use crate::hooks::{HookConfig, HookEvent, HookExecutor};

    let dir = tempfile::tempdir().unwrap();
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysErrorProvider {
        chat_calls: chat_calls.clone(),
    });
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let hooks = Arc::new(HookExecutor::new(vec![HookConfig {
        event: HookEvent::BeforeLlmCall,
        command: vec!["false".into()],
        timeout_ms: 5000,
        tool_filter: vec![],
        path_filter: vec![],
        requires_bin: None,
    }]));
    let sink_path = dir.path().join("harness-events.jsonl");
    let agent = Agent::new(AgentId::new("hookdeny-policy"), provider, tools, memory)
        .with_hooks(hooks)
        .with_harness_event_sink(sink_path.to_string_lossy().into_owned());

    let result = agent.run_task(&task_for("hi", dir.path())).await;

    assert!(result.is_err(), "hook-deny must still bail with Err");
    assert_eq!(
        chat_calls.load(AtomicOrdering::SeqCst),
        0,
        "hook denied the call before the provider was reached"
    );
    let sink = std::fs::read_to_string(&sink_path).expect("error event written to sink");
    let error_events: Vec<_> = sink
        .lines()
        .filter_map(|line| crate::harness_events::HarnessEvent::from_json_line(line).ok())
        .filter_map(|event| match event.payload {
            crate::harness_events::HarnessEventPayload::Error { data } => Some(data),
            _ => None,
        })
        .collect();
    assert_eq!(error_events.len(), 1, "exactly one classified error event");
    assert_eq!(error_events[0].variant, "policy");
    assert_eq!(error_events[0].recovery, "expected");
    assert!(error_events[0].message.contains("denied by hook"));
}

/// Records the message contents of every LLM call and returns EndTurn
/// immediately. Used to assert what the model actually saw and that it was
/// (or was not) called at all.
#[cfg(unix)]
struct RecordingEndProvider {
    chat_calls: Arc<AtomicUsize>,
    observed: Arc<StdMutex<Vec<Vec<String>>>>,
}

#[cfg(unix)]
#[async_trait]
impl LlmProvider for RecordingEndProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.chat_calls.fetch_add(1, AtomicOrdering::SeqCst);
        self.observed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(messages.iter().map(|m| m.content.clone()).collect());
        Ok(ChatResponse {
            content: Some("ok".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: LlmTokenUsage::default(),
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
#[cfg(unix)]
async fn user_prompt_submit_hook_deny_blocks_turn_before_llm() {
    use crate::hooks::{HookConfig, HookEvent, HookExecutor};

    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(RecordingEndProvider {
        chat_calls: chat_calls.clone(),
        observed: Arc::clone(&observed),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    // Hook writes its reason on stdout and exits 1 → prompt denied.
    let hooks = Arc::new(HookExecutor::new(vec![HookConfig {
        event: HookEvent::UserPromptSubmit,
        command: vec![
            "sh".into(),
            "-c".into(),
            "echo 'no coding on fridays'; exit 1".into(),
        ],
        timeout_ms: 5000,
        tool_filter: vec![],
        path_filter: vec![],
        requires_bin: None,
    }]));
    let agent = Agent::new(AgentId::new("ups-deny"), provider, tools, memory).with_hooks(hooks);

    let result = agent
        .process_message("write code", &[], vec![])
        .await
        .unwrap();

    // The turn is blocked and the hook's reason is surfaced...
    assert!(
        result.content.contains("[HOOK DENIED]"),
        "deny should be clearly surfaced; got {:?}",
        result.content
    );
    assert!(
        result.content.contains("no coding on fridays"),
        "deny reason (hook stdout) should be surfaced; got {:?}",
        result.content
    );
    // ...and the LLM is never reached.
    assert_eq!(
        chat_calls.load(AtomicOrdering::SeqCst),
        0,
        "denied prompt must not reach the model"
    );
    assert!(
        observed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    );
}

// --- Mid-turn steer injection (codex `TurnState.pending_input` parity) ---

/// Per-request prompt capture: `(role, content)` per message, one vec per
/// LLM call.
type ObservedRolePrompts = Arc<StdMutex<Vec<Vec<(MessageRole, String)>>>>;

/// Records every prompt as `(role, content)` pairs. Call 0 pushes the given
/// steer inputs into the shared buffer MID-CALL (simulating a `turn/steer`
/// racing in while the model streams), then returns a tool call; call 1
/// returns EndTurn.
struct SteerDuringToolRoundProvider {
    calls: AtomicUsize,
    observed: ObservedRolePrompts,
    buffer: crate::steering::SharedSteerBuffer,
    steers: Vec<String>,
}

#[async_trait]
impl LlmProvider for SteerDuringToolRoundProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        self.observed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(
                messages
                    .iter()
                    .map(|message| (message.role, message.content.clone()))
                    .collect(),
            );
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(if call == 0 {
            for steer in &self.steers {
                self.buffer.push(steer.clone());
            }
            tool_use(
                vec![ToolCall {
                    id: "call_alpha".to_string(),
                    name: "alpha".to_string(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }],
                1,
                1,
            )
        } else {
            end_turn("done", 1, 1)
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Steer landing between tool rounds is drained at the top of the next
/// iteration, BEFORE the next LLM call, as a plain `role: user` message
/// appended after the previous round's tool output — no wrapper text.
#[tokio::test]
async fn should_fold_steer_into_next_llm_call_when_injected_between_rounds() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(NamedEchoTool {
        name: "alpha",
        output: "alpha ok",
    });
    let buffer: crate::steering::SharedSteerBuffer =
        Arc::new(crate::steering::SteerBuffer::default());
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(SteerDuringToolRoundProvider {
        calls: AtomicUsize::new(0),
        observed: Arc::clone(&observed),
        buffer: Arc::clone(&buffer),
        steers: vec!["also check the tests".to_string()],
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("steer-agent"), provider, tools, memory)
        .with_steer_buffer(Arc::clone(&buffer));

    let result = agent.process_message("do work", &[], vec![]).await.unwrap();
    assert_eq!(result.content, "done");

    let observed = observed.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(observed.len(), 2, "tool round + follow-up round");
    // First call: no steer yet (it lands mid-call).
    assert!(
        !observed[0]
            .iter()
            .any(|(_, content)| content.contains("also check the tests")),
        "steer must not time-travel into the request that was already built"
    );
    // Second call: the steer is a plain user message with NO wrapper text,
    // appended AFTER the previous round's tool output.
    let steer_idx = observed[1]
        .iter()
        .position(|(role, content)| *role == MessageRole::User && content == "also check the tests")
        .expect("second request must carry the steer as a plain role:user message");
    let tool_idx = observed[1]
        .iter()
        .position(|(role, _)| *role == MessageRole::Tool)
        .expect("second request must carry the tool result");
    assert!(
        steer_idx > tool_idx,
        "steer must append after the prior round's tool output (append-only history)"
    );
    // Buffer fully drained.
    assert!(buffer.is_empty());
    // No callback registered → the steer row rides the turn output log so
    // the host's end-of-turn persistence writes it exactly once.
    assert!(
        result
            .messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.content == "also check the tests"),
        "without a drained-callback the steer row must land in the turn output log"
    );
}

/// Assistant shell call + its Tool result, as one exchange (spiral-signature
/// port, spec kv-cache era fixes 2026-08-03).
fn spiral_shell_exchange(id: &str, command: &str, output: &str) -> [Message; 2] {
    [
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: id.into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": command}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Tool,
            content: output.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(id.into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
    ]
}

#[test]
fn distinct_failing_exploration_commands_are_not_a_retry_spiral() {
    // Observed live (2026-08-02, kimi k3): agentic models fan out many
    // DIFFERENT exploratory commands per turn, several exiting non-zero
    // (grep with no match exits 1, ls on a guessed path fails). The
    // RetryLimit arm counted ANY >=3 failures as a spiral and killed
    // legitimate exploration turns mid-task. A retry spiral means the
    // SAME command failing over and over — distinct failures are work,
    // not a loop, and must not trip it.
    let mut messages = vec![Message::user("analyze the makepad repo")];
    // The grep outputs model the REAL shell tool: a no-match grep prints
    // nothing, and the tool renders empty output as "(no output)" plus the
    // exit-code suffix (shell.rs), not as a bare "Exit code: 1".
    messages.extend(spiral_shell_exchange(
        "call_1",
        "grep -rn 'Widget' src/",
        "(no output)\n\nExit code: 1",
    ));
    messages.extend(spiral_shell_exchange(
        "call_2",
        "ls examples/aichat",
        "No such file or directory\n\nExit code: 1",
    ));
    messages.extend(spiral_shell_exchange(
        "call_3",
        "grep -rn 'live_design' docs/",
        "(no output)\n\nExit code: 1",
    ));
    messages.extend(spiral_shell_exchange(
        "call_4",
        "cat platform/README.md",
        "No such file\n\nExit code: 1",
    ));

    assert!(
        recover_shell_retry(&messages, 4).is_none(),
        "distinct failing commands are exploration, not a retry spiral"
    );
}

/// Large enough that `truncate_old_tool_results` collapses it.

/// A tool whose output overflows the cap and which knows how to resume.
struct OverflowingPagedTool;

#[async_trait]
impl Tool for OverflowingPagedTool {
    fn name(&self) -> &str {
        "overflowing_paged"
    }

    fn description(&self) -> &str {
        "Return more output than the per-tool cap allows"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        Ok(ToolResult {
            // Comfortably past the 50_000-byte default cap.
            output: "y".repeat(120_000),
            success: true,
            ..Default::default()
        })
    }

    fn truncation_recovery(
        &self,
        args: &serde_json::Value,
        omitted_bytes: usize,
    ) -> Option<String> {
        let page = args
            .get("page")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        Some(format!(
            "[{omitted_bytes} bytes omitted] Continue with page: {}.",
            page + 1
        ))
    }
}

struct CallsOverflowingToolThenEnds {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for CallsOverflowingToolThenEnds {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(if call == 0 {
            ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ToolCall {
                    id: "call_overflow".to_string(),
                    name: "overflowing_paged".to_string(),
                    arguments: serde_json::json!({ "page": 0 }),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: LlmTokenUsage::default(),
                provider_index: None,
            }
        } else {
            ChatResponse {
                content: Some("done".to_string()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: LlmTokenUsage::default(),
                provider_index: None,
            }
        })
    }

    fn model_id(&self) -> &str {
        "test-model"
    }

    fn provider_name(&self) -> &str {
        "test-provider"
    }
}

/// The wiring test: a truncated tool result must reach the model carrying its
/// recovery advice.
///
/// The unit tests prove `truncation_recovery` returns good text. They prove
/// nothing about whether the execution loop ever CALLS it — and an unwired
/// hook that returns perfect advice into the void is the failure mode this
/// whole change exists to remove. So this drives the real loop and inspects
/// the tool message the model actually received.
#[tokio::test]
async fn truncated_tool_output_reaches_the_model_with_its_recovery_advice() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(OverflowingPagedTool);

    let provider: Arc<dyn LlmProvider> = Arc::new(CallsOverflowingToolThenEnds {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("truncation-recovery"), provider, tools, memory);

    let result = agent.process_message("go", &[], vec![]).await.unwrap();

    let tool_message = result
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("the turn must contain the tool result");

    assert!(
        tool_message.content.len() < 120_000,
        "the cap must still apply: got {} bytes",
        tool_message.content.len()
    );
    assert!(
        tool_message.content.contains("Continue with page: 1."),
        "the truncated result must carry the tool's recovery advice, otherwise the model is \
         left at a dead end and can only re-run the same call; tail was: {:?}",
        &tool_message.content[tool_message.content.len().saturating_sub(200)..]
    );
}

// ─────────────────────────────────────────────────────────────────────────
// #27d (R4) — malformed tool-call feedback buffer
// ─────────────────────────────────────────────────────────────────────────

/// A provider whose first N `chat` calls fail with
/// `StreamError::MalformedArgs` and whose subsequent calls succeed with the
/// scripted response — models "the model emitted broken tool-call JSON, then
/// self-corrected after seeing the diagnostic".
struct MalformedThenOkProvider {
    malformed_first: StdMutex<usize>,
    ok_response: ChatResponse,
}

#[async_trait]
impl LlmProvider for MalformedThenOkProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let mut guard = self
            .malformed_first
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *guard > 0 {
            *guard -= 1;
            return Err(eyre::Report::new(octos_llm::StreamError::MalformedArgs {
                tool_id: "call_bad".to_string(),
                tool_name: "shell".to_string(),
                error: "expected `,` or `}` at line 1 column 4123".to_string(),
            }));
        }
        Ok(self.ok_response.clone())
    }

    fn model_id(&self) -> &str {
        "malformed-then-ok"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn plain_text_response(content: &str) -> ChatResponse {
    ChatResponse {
        content: Some(content.to_owned()),
        reasoning_content: None,
        tool_calls: Vec::new(),
        stop_reason: octos_llm::StopReason::EndTurn,
        usage: octos_llm::TokenUsage {
            input_tokens: 5,
            output_tokens: 5,
            ..Default::default()
        },
        provider_index: None,
    }
}

/// #27d — a MalformedArgs failure is fed back as a diagnostic message; the
/// model self-corrects on the next call and the TURN SURVIVES (pre-#27d the
/// same stream error terminated the turn instantly).
#[tokio::test]
async fn malformed_toolcall_feedback_lets_model_self_correct_and_survive() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MalformedThenOkProvider {
        malformed_first: StdMutex::new(1),
        ok_response: plain_text_response("recovered: valid tool call emitted"),
    });
    let tools = ToolRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("malformed-feedback"), provider, tools, memory);
    let response = agent
        .process_message("do the thing with a tool", &[], vec![])
        .await
        .expect("turn survives the malformed tool call after feedback");
    assert_eq!(
        response.content, "recovered: valid tool call emitted",
        "the model's post-correction reply is the turn's answer"
    );
}

/// #27d — after MALFORMED_TOOLCALL_FEEDBACK_LIMIT (3) fed-back diagnostics
/// the buffer is exhausted and the turn terminates with the error (the
/// pinned pre-#27d behavior).
#[tokio::test]
async fn malformed_toolcall_feedback_exhausts_and_terminates() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MalformedThenOkProvider {
        malformed_first: StdMutex::new(10), // never self-corrects
        ok_response: plain_text_response("unreachable"),
    });
    let tools = ToolRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("malformed-exhaust"), provider, tools, memory);
    let result = agent
        .process_message("never produces valid JSON", &[], vec![])
        .await;
    let err = result.expect_err("exhausted malformed budget terminates the turn");
    assert!(
        err.to_string().contains("MalformedArgs")
            || err.to_string().contains("malformed")
            || err.to_string().contains("arguments"),
        "the terminal error names the malformed-args failure: {err}"
    );
}

#[test]
fn build_chat_config_applies_temperature_override() {
    let cfg = AgentConfig {
        chat_temperature: Some(0.7),
        ..AgentConfig::default()
    };
    let chat = build_chat_config(&cfg, false);
    assert_eq!(chat.temperature, Some(0.7));
}

#[test]
fn build_chat_config_local_provider_unsets_temperature() {
    // #2229: on a local provider with no explicit chat_temperature, temperature
    // is left UNSET (None) so the server samples — the request omits it — rather
    // than forcing greedy 0.0 (which degenerates local reasoning models).
    let cfg = AgentConfig {
        chat_temperature: None,
        ..AgentConfig::default()
    };
    let chat = build_chat_config(&cfg, true);
    assert_eq!(chat.temperature, None);
    // Cloud path is unchanged: still the built-in 0.0.
    assert_eq!(build_chat_config(&cfg, false).temperature, Some(0.0));
}

// --- #2174: conversation-loop recovery from a degenerate empty MaxTokens ---

fn empty_max_tokens_response() -> ChatResponse {
    // Models the REAL degenerate case: the whole output budget was spent on
    // reasoning, so `content` is empty and there are no tool calls. Private
    // reasoning is not an answer: terminal-integrity retries treat this as
    // empty and eventually return an explicit error if no answer arrives.
    ChatResponse {
        content: None,
        reasoning_content: Some("(long internal reasoning, no final answer)".to_string()),
        tool_calls: vec![],
        stop_reason: StopReason::MaxTokens,
        usage: LlmTokenUsage {
            input_tokens: 5,
            output_tokens: 128,
            ..Default::default()
        },
        provider_index: None,
    }
}

async fn run_conversation_response(responses: Vec<ChatResponse>) -> Result<ConversationResponse> {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(responses));
    let tools = ToolRegistry::new();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("empty-maxtokens"), provider, tools, memory).with_config(
        AgentConfig {
            max_iterations: 10,
            save_episodes: false,
            ..Default::default()
        },
    );
    agent.process_message("go", &[], vec![]).await
}

#[tokio::test]
async fn empty_max_tokens_recovers_when_retry_succeeds() {
    // A degenerate empty MaxTokens (no content, no tool call) must trigger a
    // nudge-and-retry instead of returning empty; the retry succeeds and its
    // content is returned — not a silent empty exit.
    let response = run_conversation_response(vec![
        empty_max_tokens_response(),
        end_turn("recovered answer", 4, 6),
    ])
    .await
    .unwrap();
    assert_eq!(response.content, "recovered answer");
}

#[test]
fn should_write_back_exact_state_when_no_concurrent_writer() {
    // Single-agent regression: with no concurrent writer the drop must
    // reproduce today's byte-for-byte write-back, including the grace-call
    // reset of `productive_tool_calls_since_last_grace` (a non-monotonic
    // field, so a naive max-merge would corrupt it).
    let handle = Arc::new(StdMutex::new(LoopRetryState {
        productive_tool_calls_since_last_grace: 3,
        ..Default::default()
    }));
    {
        let mut guard = PersistentRetryStateGuard::new(Some(handle.clone()));
        guard.observe_budget_exhaustion(); // fires the grace call, resets the counter
        guard.counters.timeout += 1;
    }

    let shared = handle.lock().unwrap();
    assert_eq!(shared.productive_tool_calls_since_last_grace, 0);
    assert_eq!(shared.grace_calls_fired, 1);
    assert_eq!(shared.counters.timeout, 1);
}

/// #48b — the exhausted error text STARTS WITH the stable marker and carries
/// the limit/observed payload (the CLI terminal path keys on this prefix to
/// emit `malformed_exhausted` instead of a generic turn_error row).
#[tokio::test]
async fn malformed_exhaustion_error_carries_marker() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MalformedThenOkProvider {
        malformed_first: StdMutex::new(10), // never self-corrects → exhausted
        ok_response: plain_text_response("unreachable"),
    });
    let tools = ToolRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("malformed-marker"), provider, tools, memory);
    let err = agent
        .process_message("never produces valid JSON", &[], vec![])
        .await
        .expect_err("exhausted malformed budget terminates the turn");
    let text = err.to_string();
    assert!(
        text.starts_with(crate::MALFORMED_TOOLCALL_EXHAUSTED_MARKER),
        "must START with the marker: {text}"
    );
    assert!(
        text.contains("feedback_limit=3 observed_malformed=4"),
        "carries the limit/observed payload: {text}"
    );
}
