//! LLM call orchestration with lifecycle hooks and retry logic.

use std::time::{Duration, Instant};

use eyre::Result;
use octos_core::Message;
use octos_core::TokenUsage;
use octos_llm::{
    ChatConfig, ChatResponse, StopReason, ToolSpec, record_prompt_cache_usage,
    with_prompt_cache_observation_context,
};
use tracing::{debug, info, trace, warn};

use super::Agent;
use super::prompt_cache::{build_prompt_cache_context, fingerprint_prompt};
use super::turn_state::{LoopRetryReason, LoopTurnState};
use crate::hooks::{HookEvent, HookPayload, HookResult};
use crate::progress::ProgressEvent;

impl Agent {
    /// Maximum retries for transient LLM failures (empty responses, stream errors).
    const LLM_RETRY_MAX: u32 = 3;

    /// Call the LLM with before/after lifecycle hooks.
    /// Automatically retries on empty responses and retryable stream errors.
    pub(super) async fn call_llm_with_hooks(
        &self,
        messages: &[Message],
        tools_spec: &[ToolSpec],
        config: &ChatConfig,
        iteration: u32,
        total_usage: &TokenUsage,
        turn: &mut LoopTurnState,
    ) -> Result<(ChatResponse, bool, Option<f64>)> {
        self.call_llm_with_hooks_mode(
            messages,
            tools_spec,
            config,
            iteration,
            total_usage,
            turn,
            true,
        )
        .await
    }

    /// Internal checkpoint call: retain hooks, retries, usage attribution and
    /// provider failover, but do not stream private reflection into the user's
    /// transcript.
    pub(super) async fn call_llm_with_hooks_silent(
        &self,
        messages: &[Message],
        tools_spec: &[ToolSpec],
        config: &ChatConfig,
        iteration: u32,
        total_usage: &TokenUsage,
        turn: &mut LoopTurnState,
    ) -> Result<(ChatResponse, bool, Option<f64>)> {
        self.call_llm_with_hooks_mode(
            messages,
            tools_spec,
            config,
            iteration,
            total_usage,
            turn,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_llm_with_hooks_mode(
        &self,
        messages: &[Message],
        tools_spec: &[ToolSpec],
        config: &ChatConfig,
        iteration: u32,
        total_usage: &TokenUsage,
        turn: &mut LoopTurnState,
        emit_progress: bool,
        // Returns `(response, streamed, attributed_cost_usd)`. The response's
        // `usage` MERGES discarded retry attempts into the final attempt, and
        // those attempts can come from DIFFERENT provider slots (an empty
        // stream falling through to a fallback). `attributed_cost_usd` prices
        // each attempt's tokens at the provider that actually consumed them,
        // so callers must record it instead of re-pricing the merged total at
        // the winner's rate (codex #1632 P2). `None` = no attempt was priced.
    ) -> Result<(ChatResponse, bool, Option<f64>)> {
        // Measurement only (#pi/dsh append-only study): report whether this
        // turn's request history is still a prefix-extension of the last one.
        // Off unless OCTOS_APPEND_ONLY_AUDIT=1, and never alters the request —
        // a rewrite here means the sent history stopped being reconstructable
        // from what came before, which is the drift that makes a resumed
        // session differ from the one the model actually had.
        if crate::agent::append_only_audit::enabled() {
            let rewrites = {
                let mut audit = self
                    .append_only_audit
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                audit.observe(messages)
            };
            for rewrite in rewrites {
                let description = rewrite.describe();
                warn!(
                    iteration,
                    model = %self.llm.model_id(),
                    "append-only audit: {description}"
                );
                // Also recorded out-of-band under test: a `tracing` line with
                // no subscriber installed is not a measurement you can verify.
                #[cfg(test)]
                crate::agent::append_only_audit::record_finding(format!(
                    "iteration {iteration}: {description}"
                ));
            }
        }

        let ctx = self.hook_ctx();
        if let Some(ref hooks) = self.hooks {
            let payload = HookPayload::before_llm(
                self.llm.model_id(),
                messages.len(),
                iteration,
                ctx.as_ref(),
            );
            if let HookResult::Deny(reason) = hooks.run(HookEvent::BeforeLlmCall, &payload).await {
                // #2249: a typed error (not a bare `eyre::bail!`) so the loop
                // boundary classifies the deny as policy/expected instead of
                // internal/bug. `HookDeniedError`'s Display keeps the exact
                // user-facing wording this bail used to produce.
                return Err(crate::hooks::HookDeniedError { reason }.into());
            }
        }

        // Phase 0 of the OUP semantic-cache plan: capture the final agent-side
        // prompt shape immediately before provider dispatch. The normal DEBUG
        // row contains aggregate hashes/counts only; TRACE adds per-boundary
        // hashes so an offline soak analyzer can measure the longest common
        // prefix across turns. Neither level contains prompt/tool-schema text.
        let prompt_fingerprint = fingerprint_prompt(messages, tools_spec);
        debug!(
            session = ?self.parent_session_key(),
            provider = self.llm.provider_name(),
            model = self.llm.model_id(),
            iteration,
            input_hash = %prompt_fingerprint.input_hash,
            stable_prefix_hash = %prompt_fingerprint.stable_prefix_hash,
            conversation_hash = %prompt_fingerprint.conversation_hash,
            messages = prompt_fingerprint.message_count(),
            tools = prompt_fingerprint.tool_count(),
            estimated_tokens = prompt_fingerprint.estimated_tokens,
            "model prompt cache fingerprint"
        );
        trace!(
            session = ?self.parent_session_key(),
            provider = self.llm.provider_name(),
            model = self.llm.model_id(),
            iteration,
            manifest = %prompt_fingerprint.redacted_manifest(),
            "model prompt cache fingerprint manifest"
        );
        let mut provider_config = config.clone();
        let prompt_cache_epoch_id = self
            .prompt_context_manager
            .as_ref()
            .and_then(|manager| manager.prompt_cache_epoch_id())
            .or_else(|| self.prompt_cache_epoch_id.clone());
        // Session identity only: `self.llm` may be a failover chain whose
        // `provider_name()`/`model_id()` re-run slot selection per call, and
        // cache affinity must not flap with it (route is logged above instead).
        provider_config.prompt_cache_context = Some(build_prompt_cache_context(
            &prompt_fingerprint,
            messages,
            self.parent_session_key(),
            prompt_cache_epoch_id.as_deref(),
            self.llm.supports_semantic_checkpoint_hints(),
        ));

        let mut last_error: Option<eyre::Report> = None;
        // Track token usage from retried (discarded) attempts so cost reporting
        // reflects actual consumption, not just the final successful call.
        let mut retry_usage = TokenUsage::default();
        // Spend already attributed to DISCARDED attempts, each priced at the
        // provider slot that produced it (see the return-value doc above).
        let mut retry_spend: Option<f64> = None;

        let retry_max = Self::LLM_RETRY_MAX;

        // #1712: after a truncated tool call (the turn hit the output cap
        // mid-call), the NEXT attempt requests with the model's full output
        // budget so the call has room to complete — retrying the same capped
        // request would just re-truncate. Only populated on a truncation retry;
        // the happy path never clones the config.
        let mut bumped_config: Option<ChatConfig> = None;

        // All unsuccessful exits settle the rejected responses exactly once.
        // Keep success settlement with the caller: successful responses below
        // already carry the merged retry usage and attributed spend.
        let result = async {
        for attempt in 0..=retry_max {
            let call_start = Instant::now();
            // Try the full LLM call (stream creation + consumption)
            // Estimate input tokens from message bytes (rough: ~4 chars per token
            // for English, ~1.5 for CJK). Use bytes/3 as a conservative estimate.
            let input_bytes: usize = messages.iter().map(|m| m.content.len()).sum();
            let input_estimate = (input_bytes / 3) as u32;

            let attempt_config: &ChatConfig = bumped_config.as_ref().unwrap_or(&provider_config);
            let build_and_consume = with_prompt_cache_observation_context(
                attempt_config.prompt_cache_context.as_ref(),
                iteration,
                attempt,
                async {
                    let stream = self
                        .llm
                        .chat_stream(messages, tools_spec, attempt_config)
                        .await?;
                    if emit_progress {
                        self.consume_stream_with_input_estimate(stream, iteration, input_estimate)
                            .await
                    } else {
                        self.consume_stream_with_input_estimate_mode(
                            stream,
                            iteration,
                            input_estimate,
                            false,
                        )
                        .await
                    }
                },
            );
            let call_result = build_and_consume.await;

            match call_result {
                Ok((response, streamed)) => {
                    self.record_prompt_cache_usage(&response, attempt_config, iteration, attempt);
                    if !Self::is_retriable_response(&response) {
                        // Genuine success -- merge retry usage into response.
                        // Price the FINAL attempt at its own slot BEFORE the
                        // merge, then add the pre-priced retry spend.
                        let final_cost = self.response_usage_cost(
                            response.usage.input_tokens,
                            response.usage.output_tokens,
                            response.usage.cache_read_tokens,
                            response.usage.cache_write_tokens,
                            response.provider_index,
                        );
                        let attributed_cost = match (retry_spend, final_cost) {
                            (None, None) => None,
                            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
                        };
                        let mut response = response;
                        response.usage.input_tokens += retry_usage.input_tokens;
                        response.usage.output_tokens += retry_usage.output_tokens;
                        response.usage.reasoning_tokens += retry_usage.reasoning_tokens;
                        response.usage.cache_read_tokens += retry_usage.cache_read_tokens;
                        response.usage.cache_write_tokens += retry_usage.cache_write_tokens;
                        self.observe_semantic_checkpoint_report(&response, &provider_config);
                        self.observe_effective_provider_route(&response);

                        if let Some(ref hooks) = self.hooks {
                            let latency_ms = call_start.elapsed().as_millis() as u64;
                            let cum_in = total_usage.input_tokens + response.usage.input_tokens;
                            let cum_out = total_usage.output_tokens + response.usage.output_tokens;
                            // #2194 review: the payload carries the ATTRIBUTED
                            // spend — each attempt priced at the slot that
                            // produced it — never a reprice of the merged
                            // usage at `self.llm.model_id()`, which misprices
                            // cross-provider retries. `turn` has not recorded
                            // THIS response yet, so its spend is exactly the
                            // turn's prior responses; without a figure for
                            // this call the turn's prior attributed spend is
                            // still the honest cumulative.
                            let response_cost = attributed_cost;
                            let session_cost = match attributed_cost {
                                Some(cost) => Some(turn.spend_usd() + cost),
                                None => turn.priced_spend(),
                            };
                            let payload = HookPayload::after_llm(
                                self.llm.model_id(),
                                iteration,
                                &format!("{:?}", response.stop_reason),
                                !response.tool_calls.is_empty(),
                                response.usage.input_tokens,
                                response.usage.output_tokens,
                                self.llm.provider_name(),
                                latency_ms,
                                cum_in,
                                cum_out,
                                session_cost,
                                response_cost,
                                ctx.as_ref(),
                            );
                            let _ = hooks.run(HookEvent::AfterLlmCall, &payload).await;
                        }
                        return Ok((response, streamed, attributed_cost));
                    }

                    // Every rejected response consumed usage, including the
                    // last streaming attempt.
                    if let Some(cost) = self.response_usage_cost(
                        response.usage.input_tokens,
                        response.usage.output_tokens,
                        response.usage.cache_read_tokens,
                        response.usage.cache_write_tokens,
                        response.provider_index,
                    ) {
                        retry_spend = Some(retry_spend.unwrap_or(0.0) + cost);
                    }
                    retry_usage.input_tokens += response.usage.input_tokens;
                    retry_usage.output_tokens += response.usage.output_tokens;
                    retry_usage.reasoning_tokens += response.usage.reasoning_tokens;
                    retry_usage.cache_read_tokens += response.usage.cache_read_tokens;
                    retry_usage.cache_write_tokens += response.usage.cache_write_tokens;

                    if attempt == retry_max {
                        // All streaming retries exhausted.
                        let reason = if response.stop_reason == StopReason::ContentFiltered {
                            "content filtered by safety/moderation"
                        } else {
                            "empty response (no content or tool_calls)"
                        };
                        turn.record_retry(LoopRetryReason::ProviderFailover {
                            reason: format!("streaming retries exhausted: {reason}"),
                        });
                        self.llm.report_late_failure();

                        // Try one final non-streaming call — this goes through
                        // the failover chain (RetryProvider → ProviderChain),
                        // not just the primary.
                        warn!(
                            attempts = Self::LLM_RETRY_MAX + 1,
                            reason, "streaming retries exhausted, trying non-streaming fallback"
                        );

                        // Non-streaming call traverses the full failover chain
                        match with_prompt_cache_observation_context(
                            provider_config.prompt_cache_context.as_ref(),
                            iteration,
                            attempt + 1,
                            self.llm.chat(messages, tools_spec, &provider_config),
                        )
                        .await
                        {
                            Ok(fallback_resp) if !Self::is_retriable_response(&fallback_resp) => {
                                self.record_prompt_cache_usage(
                                    &fallback_resp,
                                    &provider_config,
                                    iteration,
                                    attempt + 1,
                                );
                                info!("non-streaming fallback succeeded");
                                let final_cost = self.response_usage_cost(
                                    fallback_resp.usage.input_tokens,
                                    fallback_resp.usage.output_tokens,
                                    fallback_resp.usage.cache_read_tokens,
                                    fallback_resp.usage.cache_write_tokens,
                                    fallback_resp.provider_index,
                                );
                                let attributed_cost = match (retry_spend, final_cost) {
                                    (None, None) => None,
                                    (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
                                };
                                let mut fallback_resp = fallback_resp;
                                fallback_resp.usage.input_tokens += retry_usage.input_tokens;
                                fallback_resp.usage.output_tokens += retry_usage.output_tokens;
                                fallback_resp.usage.reasoning_tokens += retry_usage.reasoning_tokens;
                                fallback_resp.usage.cache_read_tokens +=
                                    retry_usage.cache_read_tokens;
                                fallback_resp.usage.cache_write_tokens +=
                                    retry_usage.cache_write_tokens;
                                self.observe_semantic_checkpoint_report(
                                    &fallback_resp,
                                    &provider_config,
                                );
                                self.observe_effective_provider_route(&fallback_resp);
                                return Ok((fallback_resp, false, attributed_cost));
                            }
                            Ok(fallback_resp) => {
                                self.record_prompt_cache_usage(
                                    &fallback_resp,
                                    &provider_config,
                                    iteration,
                                    attempt + 1,
                                );
                                if let Some(cost) = self.response_usage_cost(
                                    fallback_resp.usage.input_tokens,
                                    fallback_resp.usage.output_tokens,
                                    fallback_resp.usage.cache_read_tokens,
                                    fallback_resp.usage.cache_write_tokens,
                                    fallback_resp.provider_index,
                                ) {
                                    retry_spend = Some(retry_spend.unwrap_or(0.0) + cost);
                                }
                                retry_usage.input_tokens += fallback_resp.usage.input_tokens;
                                retry_usage.output_tokens += fallback_resp.usage.output_tokens;
                                retry_usage.reasoning_tokens += fallback_resp.usage.reasoning_tokens;
                                retry_usage.cache_read_tokens +=
                                    fallback_resp.usage.cache_read_tokens;
                                retry_usage.cache_write_tokens +=
                                    fallback_resp.usage.cache_write_tokens;
                                warn!("non-streaming fallback also returned empty response");
                            }
                            Err(e) => {
                                warn!(error = %e, "non-streaming fallback failed");
                            }
                        }

                        return Err(eyre::eyre!(
                            "LLM returned empty response after {} retries: {}",
                            Self::LLM_RETRY_MAX + 1,
                            reason
                        ));
                    }

                    // Retry after accounting for this rejected response above.
                    let delay = Duration::from_secs(1 << attempt);
                    let reason = if response.stop_reason == StopReason::ContentFiltered {
                        "content filtered by safety/moderation"
                    } else {
                        "empty response (no content/tool_calls)"
                    };
                    turn.record_retry(LoopRetryReason::EmptyResponse {
                        attempt: attempt + 1,
                        reason: reason.to_string(),
                    });
                    warn!(
                        attempt = attempt + 1,
                        max = retry_max,
                        delay_s = delay.as_secs(),
                        iteration,
                        stop_reason = ?response.stop_reason,
                        reason,
                        "abnormal LLM response, retrying"
                    );
                    // Clear stream forwarder buffer before retry so partial
                    // text from this attempt isn't concatenated with the next.
                    if emit_progress {
                        self.reporter()
                            .report(ProgressEvent::StreamRetry { iteration });
                        self.reporter().report(ProgressEvent::LlmStatus {
                            message: format!(
                                "Retrying ({}/{})... {}",
                                attempt + 1,
                                retry_max + 1,
                                reason,
                            ),
                            iteration,
                        });
                    }
                    tokio::time::sleep(delay).await;
                }
                Err(e) => {
                    if attempt < retry_max && Self::is_retryable_stream_error(&e) {
                        let delay = Duration::from_secs(1 << attempt);
                        // #1712: a truncated tool call means the model needed
                        // more output room than the per-turn cap allowed. Retry
                        // with the model's full output budget so the next
                        // attempt can complete the call. Only bump upward.
                        if Self::is_truncated_tool_call_error(&e) {
                            let model_max = self.llm.max_output_tokens();
                            let current = provider_config.max_tokens.unwrap_or(0);
                            if model_max > current {
                                let mut c = provider_config.clone();
                                c.max_tokens = Some(model_max);
                                warn!(
                                    from = current,
                                    to = model_max,
                                    iteration,
                                    "truncated tool call — raising output budget for retry (#1712)"
                                );
                                bumped_config = Some(c);
                            }
                        }
                        turn.record_retry(LoopRetryReason::StreamError {
                            attempt: attempt + 1,
                            error: e.to_string(),
                        });
                        warn!(
                            attempt = attempt + 1,
                            max = retry_max,
                            delay_s = delay.as_secs(),
                            error = %e,
                            iteration,
                            "retryable stream error, retrying"
                        );
                        // Clear stream forwarder buffer before retry so partial
                        // text from this attempt isn't concatenated with the next.
                        if emit_progress {
                            self.reporter()
                                .report(ProgressEvent::StreamRetry { iteration });
                            self.reporter().report(ProgressEvent::LlmStatus {
                                message: format!(
                                    "Retrying ({}/{})... stream error",
                                    attempt + 1,
                                    retry_max + 1,
                                ),
                                iteration,
                            });
                        }
                        last_error = Some(e);
                        tokio::time::sleep(delay).await;
                    } else if attempt == retry_max {
                        // Stream retries exhausted.
                        turn.record_retry(LoopRetryReason::ProviderFailover {
                            reason: "stream retries exhausted".to_string(),
                        });
                        self.llm.report_late_failure();

                        // Try non-streaming with full fallback chain
                        warn!(
                            error = %e,
                            "stream retries exhausted, trying non-streaming fallback"
                        );
                        match with_prompt_cache_observation_context(
                            provider_config.prompt_cache_context.as_ref(),
                            iteration,
                            attempt + 1,
                            self.llm.chat(messages, tools_spec, &provider_config),
                        )
                        .await
                        {
                            Ok(resp) if !Self::is_retriable_response(&resp) => {
                                self.record_prompt_cache_usage(
                                    &resp,
                                    &provider_config,
                                    iteration,
                                    attempt + 1,
                                );
                                info!("non-streaming fallback succeeded after stream failures");
                                // Codex #1632 r2 P2: merge the accumulated
                                // retry usage here like the empty-response
                                // fallback does — earlier EMPTY attempts (not
                                // just stream errors) can be behind us on
                                // this path, and `retry_spend` already prices
                                // them; returning fallback-only tokens next
                                // to a spend that includes both would skew
                                // the persisted token/cost pairing.
                                let final_cost = self.response_usage_cost(
                                    resp.usage.input_tokens,
                                    resp.usage.output_tokens,
                                    resp.usage.cache_read_tokens,
                                    resp.usage.cache_write_tokens,
                                    resp.provider_index,
                                );
                                let attributed_cost = match (retry_spend, final_cost) {
                                    (None, None) => None,
                                    (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
                                };
                                let mut resp = resp;
                                resp.usage.input_tokens += retry_usage.input_tokens;
                                resp.usage.output_tokens += retry_usage.output_tokens;
                                resp.usage.reasoning_tokens += retry_usage.reasoning_tokens;
                                resp.usage.cache_read_tokens += retry_usage.cache_read_tokens;
                                resp.usage.cache_write_tokens += retry_usage.cache_write_tokens;
                                self.observe_semantic_checkpoint_report(&resp, &provider_config);
                                self.observe_effective_provider_route(&resp);
                                return Ok((resp, false, attributed_cost));
                            }
                            Ok(resp) => {
                                self.record_prompt_cache_usage(
                                    &resp,
                                    &provider_config,
                                    iteration,
                                    attempt + 1,
                                );
                                if let Some(cost) = self.response_usage_cost(
                                    resp.usage.input_tokens,
                                    resp.usage.output_tokens,
                                    resp.usage.cache_read_tokens,
                                    resp.usage.cache_write_tokens,
                                    resp.provider_index,
                                ) {
                                    retry_spend = Some(retry_spend.unwrap_or(0.0) + cost);
                                }
                                retry_usage.input_tokens += resp.usage.input_tokens;
                                retry_usage.output_tokens += resp.usage.output_tokens;
                                retry_usage.reasoning_tokens += resp.usage.reasoning_tokens;
                                retry_usage.cache_read_tokens += resp.usage.cache_read_tokens;
                                retry_usage.cache_write_tokens += resp.usage.cache_write_tokens;
                                warn!("non-streaming fallback also returned empty");
                            }
                            Err(fb_err) => {
                                warn!(error = %fb_err, "non-streaming fallback also failed");
                            }
                        }
                        return Err(e);
                    } else {
                        // Non-retryable error -- propagate immediately
                        return Err(e);
                    }
                }
            }
        }

        // All retries exhausted with errors
        Err(last_error.unwrap_or_else(|| eyre::eyre!("LLM call failed after retries")))
        }
        .await;
        if result.is_err() {
            turn.record_usage(&retry_usage, None, retry_spend);
        }
        result
    }

    fn record_prompt_cache_usage(
        &self,
        response: &ChatResponse,
        config: &ChatConfig,
        agent_iteration: u32,
        attempt: u32,
    ) {
        let route = self
            .llm
            .provider_metadata_for_index(response.provider_index);
        record_prompt_cache_usage(
            config.prompt_cache_context.as_ref(),
            &route.provider,
            &route.model,
            agent_iteration,
            attempt,
            &response.usage,
        );
    }

    fn observe_semantic_checkpoint_report(&self, response: &ChatResponse, config: &ChatConfig) {
        let Some(report) = response.usage.semantic_checkpoint.as_ref() else {
            return;
        };
        let offered = report
            .restored_boundary_id
            .as_deref()
            .is_none_or(|restored| {
                config.prompt_cache_context.as_ref().is_some_and(|context| {
                    context
                        .semantic_boundaries
                        .iter()
                        .any(|hint| hint.boundary_id == restored)
                })
            });
        if offered {
            debug!(
                provider = self.llm.provider_name(),
                model = self.llm.model_id(),
                restored_boundary_id = ?report.restored_boundary_id,
                restored_prefix_tokens = report.restored_prefix_tokens,
                re_prefill_tokens = report.re_prefill_tokens,
                "semantic checkpoint restore report"
            );
        } else {
            warn!(
                provider = self.llm.provider_name(),
                model = self.llm.model_id(),
                restored_boundary_id = ?report.restored_boundary_id,
                "provider reported a semantic checkpoint that was not offered for this exact prefix"
            );
        }
    }

    fn observe_effective_provider_route(&self, response: &ChatResponse) {
        let Some(manager) = self.prompt_context_manager.as_ref() else {
            return;
        };
        let route = self
            .llm
            .provider_metadata_for_index(response.provider_index);
        manager.observe_effective_provider_route(&route.provider, &route.model);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use async_trait::async_trait;
    use futures::stream;
    use octos_core::{AgentId, Message};
    use octos_llm::{
        ChatConfig, ChatResponse, ChatStream, LlmError, LlmErrorKind, LlmProvider, ProviderChain,
        StopReason, StreamEvent, TokenUsage as LlmTokenUsage, ToolSpec,
    };
    use octos_memory::EpisodeStore;

    use super::super::Agent;
    use super::super::turn_state::LoopTurnState;
    use crate::prompt_context::{PromptContextManager, PromptContextReport, PromptContextRequest};
    use crate::tools::ToolRegistry;

    // ── Shared call counters ──────────────────────────────────────────────────

    #[derive(Default)]
    struct CallCounters {
        chat_stream: AtomicU32,
        chat: AtomicU32,
    }

    // ── Provider that truncates a tool call on attempt 1, succeeds on 2 ───────
    // #1712: models the real failure — a native streaming tool call cut off by
    // the output cap (Done(MaxTokens) + unterminated args) on the first attempt,
    // then a clean call on the retry. Records the `max_tokens` it saw each call
    // so the test can assert the retry was issued with a RAISED budget.

    struct TruncateThenSucceedProvider {
        counters: Arc<CallCounters>,
        seen_max_tokens: Arc<std::sync::Mutex<Vec<Option<u32>>>>,
    }

    struct NamedRouteProvider {
        provider: &'static str,
        model: &'static str,
        fail: bool,
    }

    #[derive(Default)]
    struct EffectiveRouteObserver {
        routes: Mutex<Vec<(String, String)>>,
    }

    impl PromptContextManager for EffectiveRouteObserver {
        fn prepare_prompt(
            &self,
            _request: PromptContextRequest,
            _messages: &mut Vec<Message>,
        ) -> std::result::Result<PromptContextReport, String> {
            Ok(PromptContextReport::default())
        }

        fn observe_effective_provider_route(&self, provider_name: &str, model_id: &str) {
            self.routes
                .lock()
                .unwrap()
                .push((provider_name.to_owned(), model_id.to_owned()));
        }
    }

    #[async_trait]
    impl LlmProvider for NamedRouteProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            if self.fail {
                return Err(LlmError::new(
                    LlmErrorKind::ServerError { status: 503 },
                    "forced primary failure",
                )
                .into());
            }
            Ok(ChatResponse {
                content: Some("fallback response".to_owned()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
                usage: LlmTokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            self.model
        }

        fn provider_name(&self) -> &str {
            self.provider
        }
    }

    #[async_trait]
    impl LlmProvider for TruncateThenSucceedProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            self.counters.chat.fetch_add(1, Ordering::SeqCst);
            eyre::bail!("non-streaming fallback should not be reached in this test")
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            config: &ChatConfig,
        ) -> eyre::Result<ChatStream> {
            let n = self.counters.chat_stream.fetch_add(1, Ordering::SeqCst);
            self.seen_max_tokens.lock().unwrap().push(config.max_tokens);
            if n == 0 {
                // Attempt 1: truncated mid-args, finished on the output cap.
                let events = vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("write_file_26".to_string()),
                        name: Some("write_file".to_string()),
                        arguments_delta: "{\"path\":\"r.md\",\"content\":\"# Rev".to_string(),
                    },
                    StreamEvent::Usage(LlmTokenUsage::default()),
                    StreamEvent::Done(StopReason::MaxTokens),
                ];
                Ok(Box::pin(stream::iter(events)))
            } else {
                // Attempt 2 (bumped budget): a clean, complete tool call.
                let events = vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("write_file_27".to_string()),
                        name: Some("write_file".to_string()),
                        arguments_delta: "{\"path\":\"r.md\",\"content\":\"# Review\"}".to_string(),
                    },
                    StreamEvent::Usage(LlmTokenUsage::default()),
                    StreamEvent::Done(StopReason::ToolUse),
                ];
                Ok(Box::pin(stream::iter(events)))
            }
        }

        fn model_id(&self) -> &str {
            // Resolves to a large max_output_tokens via context::max_output_tokens.
            "minimax-m3"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    // ── Test helpers ──────────────────────────────────────────────────────────

    async fn build_agent(provider: Arc<dyn LlmProvider>) -> (Agent, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let tools = ToolRegistry::new();
        let agent = Agent::new(AgentId::new("llm-call-test"), provider, tools, memory);
        (agent, dir)
    }

    fn msgs() -> Vec<Message> {
        vec![Message::user("hello")]
    }

    fn turn() -> LoopTurnState {
        LoopTurnState::new(Instant::now())
    }

    /// One clean streamed tool-call response with cache-bearing usage, on a
    /// priced model under an anthropic label — for hook payload cost tests.
    struct PricedGoodStreamProvider;

    #[async_trait]
    impl LlmProvider for PricedGoodStreamProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            eyre::bail!("non-streaming fallback should not be reached in this test")
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatStream> {
            let events = vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("read_file_1".to_string()),
                    name: Some("read_file".to_string()),
                    arguments_delta: "{\"path\":\"a.md\"}".to_string(),
                },
                StreamEvent::Usage(LlmTokenUsage {
                    input_tokens: 100_000,
                    output_tokens: 10_000,
                    cache_read_tokens: 10_000,
                    cache_write_tokens: 2_000,
                    ..Default::default()
                }),
                StreamEvent::Done(StopReason::ToolUse),
            ];
            Ok(Box::pin(stream::iter(events)))
        }

        fn model_id(&self) -> &str {
            "claude-opus-4"
        }

        fn provider_name(&self) -> &str {
            "anthropic"
        }
    }

    /// #2194 review: `attributed_cost` prices each attempt at its actual
    /// provider slot, but the after-LLM hook payload used to REPRICE the
    /// merged usage at `self.llm.model_id()` — wrong for anything the
    /// attribution already priced differently. The payload must carry the
    /// attributed figures: response_cost = this call's attributed spend,
    /// session_cost = the turn's previously attributed spend + this call's.
    #[cfg(unix)]
    #[tokio::test]
    async fn should_report_attributed_spend_in_after_llm_hook_payload() {
        use crate::hooks::{HookConfig, HookExecutor};

        let capture_dir = tempfile::tempdir().unwrap();
        let capture = capture_dir.path().join("after_llm_payload.json");
        let hook = HookConfig {
            event: crate::hooks::HookEvent::AfterLlmCall,
            command: vec![
                "sh".into(),
                "-c".into(),
                format!("cat > {}", capture.display()),
            ],
            timeout_ms: 5_000,
            tool_filter: vec![],
            path_filter: vec![],
            requires_bin: None,
        };

        let (agent, _dir) = build_agent(Arc::new(PricedGoodStreamProvider)).await;
        let agent = agent.with_hooks(Arc::new(HookExecutor::new(vec![hook])));

        // Prior turn spend recorded with a DELIBERATELY off-catalog figure:
        // the hook's cumulative cost must be built from this attribution,
        // never from repricing the cumulative tokens at the current model.
        let mut turn = turn();
        turn.record_usage(
            &octos_core::TokenUsage {
                input_tokens: 5_000,
                output_tokens: 1_000,
                ..Default::default()
            },
            None,
            Some(0.005),
        );
        let total_usage = turn.total_usage().clone();

        let (_response, _streamed, attributed) = agent
            .call_llm_with_hooks(
                &msgs(),
                &[],
                &ChatConfig::default(),
                1,
                &total_usage,
                &mut turn,
            )
            .await
            .expect("clean streamed response");
        let attributed = attributed.expect("claude-opus-4 has catalog pricing");

        let raw = std::fs::read_to_string(&capture).expect("hook captured the payload");
        let payload: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let response_cost = payload["response_cost"]
            .as_f64()
            .expect("response_cost present");
        let session_cost = payload["session_cost"]
            .as_f64()
            .expect("session_cost present");
        assert!(
            (response_cost - attributed).abs() < 1e-12,
            "hook response_cost must be the attributed spend, got {response_cost} vs {attributed}"
        );
        assert!(
            (session_cost - (0.005 + attributed)).abs() < 1e-12,
            "hook session_cost must be prior attributed turn spend + this call's, \
             got {session_cost} vs {}",
            0.005 + attributed
        );
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn provider_chain_reports_the_concrete_route_that_won_failover() {
        let chain: Arc<dyn LlmProvider> = Arc::new(ProviderChain::new(vec![
            Arc::new(NamedRouteProvider {
                provider: "primary",
                model: "model-a",
                fail: true,
            }),
            Arc::new(NamedRouteProvider {
                provider: "fallback",
                model: "model-b",
                fail: false,
            }),
        ]));
        let observer = Arc::new(EffectiveRouteObserver::default());
        let (agent, _dir) = build_agent(chain).await;
        let agent = agent.with_prompt_context_manager(observer.clone());

        let (response, streamed, _cost) = agent
            .call_llm_with_hooks(
                &msgs(),
                &[],
                &ChatConfig::default(),
                1,
                &octos_core::TokenUsage::default(),
                &mut turn(),
            )
            .await
            .expect("fallback lane should complete the request");

        assert!(streamed);
        assert_eq!(response.content.as_deref(), Some("fallback response"));
        assert_eq!(response.provider_index, Some(1));
        assert_eq!(
            *observer.routes.lock().unwrap(),
            vec![("fallback".to_owned(), "model-b".to_owned())],
            "the durable context hook must see the winner once, not the failed primary"
        );
    }

    /// #1712: a truncated tool call (Done(MaxTokens) + unterminated args) is
    /// RETRYABLE — the loop retries (not instant death) AND raises the output
    /// budget for the retry so it can complete. Asserts: two stream attempts,
    /// the second issued with a bumped max_tokens, and the call ultimately
    /// succeeds with the completed tool call.
    #[tokio::test]
    async fn truncated_tool_call_retries_with_raised_output_budget() {
        let counters = Arc::new(CallCounters::default());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(TruncateThenSucceedProvider {
            counters: counters.clone(),
            seen_max_tokens: seen.clone(),
        });
        let (agent, _dir) = build_agent(provider).await;

        // Start with a small per-turn cap (what cuts the call off).
        let small_cap = 1200u32;
        let config = ChatConfig {
            max_tokens: Some(small_cap),
            ..Default::default()
        };

        let result = agent
            .call_llm_with_hooks(
                &msgs(),
                &[],
                &config,
                1,
                &octos_core::TokenUsage::default(),
                &mut turn(),
            )
            .await;

        let (response, _streamed, _cost) = result.expect("truncation must recover, not fail");
        assert_eq!(
            counters.chat_stream.load(Ordering::SeqCst),
            2,
            "must retry once after the truncated call"
        );
        assert_eq!(
            response.tool_calls.len(),
            1,
            "the retry's completed tool call must surface"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "two stream attempts");
        assert_eq!(seen[0], Some(small_cap), "attempt 1 uses the small cap");
        assert!(
            seen[1].unwrap() > small_cap,
            "attempt 2 must raise the output budget (was {:?})",
            seen[1]
        );
    }
}
