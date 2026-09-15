//! Context window limits, token estimation, and model metadata.
//!
//! All model-specific data (context window, max output, descriptions) comes from
//! `model_catalog.json` at runtime. Hardcoded defaults are only used as a
//! conservative fallback when the catalog hasn't been loaded or doesn't contain
//! the requested model.

use octos_core::Message;
use std::collections::HashMap;
use std::sync::RwLock;

// ── Runtime catalog (loaded from model_catalog.json) ─────────

/// Cached model info from the runtime catalog.
struct CatalogModel {
    context_window: u64,
    max_output: u64,
}

/// Global runtime catalog, populated by `seed_from_catalog()`.
static CATALOG: RwLock<Option<HashMap<String, CatalogModel>>> = RwLock::new(None);

/// Seed the runtime catalog from model_catalog.json entries.
/// Called once at startup by the gateway after loading the catalog.
/// The `entries` parameter is a list of (provider_slash_model, context_window, max_output).
pub fn seed_from_catalog(entries: &[(String, u64, u64)]) {
    *CATALOG.write().unwrap_or_else(|e| e.into_inner()) = Some(build_catalog_map(entries));
}

/// The provider/host prefix of a catalog key: everything before the first `/`
/// (or the whole key when unqualified). Used to break bare-alias ties on the
/// native HOST rather than the raw key, so a `zai-coding/…` re-host cannot
/// out-sort the native `zai/…` row just because `-` sorts before `/`. Shared
/// with `pricing::seed_pricing_catalog` so the two catalogs agree.
pub(crate) fn provider_prefix(key: &str) -> &str {
    key.split_once('/').map(|(prefix, _)| prefix).unwrap_or(key)
}

/// Build the runtime catalog lookup map from `(provider/model, ctx, max_out)`
/// entries. Pure (touches no global state) so the alias-selection rules below
/// are unit-testable without racing the shared `CATALOG`.
fn build_catalog_map(entries: &[(String, u64, u64)]) -> HashMap<String, CatalogModel> {
    let mut map = HashMap::new();
    // See `pricing::seed_pricing_catalog`: a bare model id can be shared by the
    // native provider and its re-hosts, so award the bare alias to the row with
    // the FEWEST path segments (most canonical/native), deterministically. On an
    // EQUAL segment count the more-native HOST wins: compare the provider prefix
    // (the segment before the first `/`) FIRST, then the full lowercased key. A
    // plain full-key comparison is wrong when one host is a lexical prefix of
    // another — `zai-coding/glm-5.2` (200K) sorts BEFORE `zai/glm-5.2` (1M)
    // because `-` (0x2D) < `/` (0x2F), so the coding-plan re-host would steal the
    // bare `glm-5.2` alias from the native `zai` row. Splitting on the separator
    // makes `zai` < `zai-coding` (the shorter native prefix wins), keeping the
    // 1M window for a bare `glm-5.2` lookup. `minimax/MiniMax-M3` still beats
    // `r9s/minimax-m3` (both one slash), so the bare alias stays order-independent.
    let mut bare_owner: HashMap<String, (usize, String)> = HashMap::new();
    for (key, ctx, max_out) in entries {
        // Store by full key ("dashscope/qwen3.5-plus") and by model name alone ("qwen3.5-plus")
        let key_lower = key.to_lowercase();
        map.insert(
            key_lower.clone(),
            CatalogModel {
                context_window: *ctx,
                max_output: *max_out,
            },
        );
        if let Some(model) = key.split('/').next_back() {
            let bare = model.to_lowercase();
            let segments = key.matches('/').count();
            let take = match bare_owner.get(&bare) {
                None => true,
                Some((owned_seg, owner_key)) => {
                    segments < *owned_seg
                        || (segments == *owned_seg
                            && (provider_prefix(&key_lower), key_lower.as_str())
                                < (provider_prefix(owner_key), owner_key.as_str()))
                }
            };
            if take {
                map.insert(
                    bare.clone(),
                    CatalogModel {
                        context_window: *ctx,
                        max_output: *max_out,
                    },
                );
                bare_owner.insert(bare, (segments, key_lower));
            }
        }
    }
    map
}

/// Look up a value from the runtime catalog by model ID.
fn catalog_lookup(model_id: &str) -> Option<(u64, u64)> {
    let guard = CATALOG.read().ok()?;
    let map = guard.as_ref()?;
    catalog_lookup_in(map, model_id)
}

/// Pure catalog matcher over a supplied map (no global state), so the matching
/// rules are unit-testable without racing the shared `CATALOG`.
fn catalog_lookup_in(map: &HashMap<String, CatalogModel>, model_id: &str) -> Option<(u64, u64)> {
    let m = model_id.to_lowercase();
    // Try exact match first, then substring match.
    if let Some(entry) = map.get(&m) {
        return Some((entry.context_window, entry.max_output));
    }
    // Deterministic substring match, mirroring pricing.rs so both agree:
    //   1. Among keys the model id CONTAINS (families the id extends), the
    //      LONGEST (most specific) wins.
    //   2. Otherwise, among keys that CONTAIN the model id, the SHORTEST is the
    //      closest match.
    // Ties break lexicographically so equal-length keys stay stable. Returning
    // the first HashMap hit (as before) was nondeterministic across processes.
    if let Some((_, entry)) = map
        .iter()
        .filter(|(key, _)| m.contains(key.as_str()))
        .max_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| b.cmp(a)))
    {
        return Some((entry.context_window, entry.max_output));
    }
    map.iter()
        .filter(|(key, _)| key.contains(&m))
        .min_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
        .map(|(_, entry)| (entry.context_window, entry.max_output))
}

// ── Public API ────────────────────────────────────────────────

/// Context window size for a model. Checks runtime catalog first.
pub fn context_window_tokens(model_id: &str) -> u32 {
    if let Some((ctx, _)) = catalog_lookup(model_id) {
        if ctx > 0 {
            return ctx as u32;
        }
    }
    // Model-specific defaults for known long-context models when the catalog
    // is unavailable or lacks the exact variant (e.g. deepseek-v4-flash, which
    // has no dedicated catalog lane). DeepSeek V4, MiniMax M3 and Kimi K3 are
    // 1M-context.
    // The Kimi coding plan (family `moonshot-coding`) exposes K3 under the bare
    // ids `k3` / `kimi-for-coding*`, which don't contain `kimi-k3` — match them
    // too so the `ctx N%` gauge shows K3's real ~1M window, not the 128K default.
    let m = model_id.to_lowercase();
    if m.contains("deepseek-v4")
        || m.contains("minimax-m3")
        || m.contains("kimi-k3")
        || m == "k3"
        || m.starts_with("kimi-for-coding")
    {
        return 1_048_576;
    }
    // Conservative default for unknown models
    128_000
}

/// Maximum output tokens for a model. Checks runtime catalog first.
pub fn max_output_tokens(model_id: &str) -> u32 {
    if let Some((_, max_out)) = catalog_lookup(model_id) {
        if max_out > 0 {
            return max_out as u32;
        }
    }
    // Model-specific defaults when catalog is unavailable.
    // Use the model's native max output to avoid truncation.
    let m = model_id.to_lowercase();
    // Check the newest model families first so they win over the broader
    // substring branches below (e.g. minimax-m3 before the generic minimax).
    if m.contains("deepseek-v4") {
        384_000
    } else if m.contains("minimax-m3") || m.contains("kimi-k3") {
        // kimi-k3 default max completion is 131072 (settable up to 1M);
        // must win over the broader "kimi" branch below.
        131_072
    } else if m.contains("kimi") || m.contains("qwen") || m.contains("gemini") {
        65_535
    } else if m.contains("glm") || m.contains("minimax") {
        128_000
    } else if m.contains("gpt-4") || m.contains("gpt-5") || m.contains("claude") {
        32_768
    } else if m.contains("deepseek") {
        8_000
    } else {
        // Conservative default for unknown models
        16_384
    }
}

/// Default max tokens per LLM call.
pub fn default_max_tokens() -> u32 {
    16_384
}

/// Estimate token count from text using character heuristic.
///
/// Uses ~4 chars/token for ASCII (English/code) and ~1.5 chars/token for
/// non-ASCII (CJK, emoji, etc.). This is a rough guard — not a precise
/// tokenizer — so it intentionally overestimates slightly to be safe.
pub fn estimate_tokens(text: &str) -> u32 {
    let ascii_chars = text.bytes().filter(|b| b.is_ascii()).count() as u32;
    let non_ascii_chars = text.chars().count() as u32 - ascii_chars;
    let tokens = ascii_chars / 4 + (non_ascii_chars as f32 / 1.5) as u32;
    tokens.max(1)
}

/// Estimate tokens for a message (content + serialized tool calls + overhead).
pub fn estimate_message_tokens(msg: &Message) -> u32 {
    let mut tokens = estimate_tokens(&msg.content);
    if let Some(ref calls) = msg.tool_calls {
        for call in calls {
            tokens += estimate_tokens(&call.name);
            tokens += estimate_tokens(&call.arguments.to_string());
        }
    }
    // Role/structural overhead (~4 tokens)
    tokens + 4
}

/// Rough token cost of ONE tool declaration in the request: providers
/// serialize every name, description, and full input schema (#2135
/// round-8 P1 — a guard that only totals messages passes a tiny prompt
/// whose tool schemas alone overflow a small window).
pub(crate) fn estimate_tool_tokens(tool: &crate::types::ToolSpec) -> u32 {
    estimate_tokens(&tool.name)
        + estimate_tokens(&tool.description)
        + estimate_tokens(&tool.input_schema.to_string())
        + 8 // serialization scaffolding per tool
}

/// Provider-agnostic base request-size estimate (#2143 part 3): messages plus
/// serialized tool declarations. This is what the route-fit guard summed
/// inline before #2143; it is now the DEFAULT body of
/// [`crate::provider::LlmProvider::estimate_request_tokens`], which concrete
/// providers override to add their own request-envelope overhead (a separate
/// system block, per-message content-block framing, cache-control metadata)
/// that this base does not model.
pub fn estimate_request_tokens_base(
    messages: &[octos_core::Message],
    tools: &[crate::types::ToolSpec],
) -> u32 {
    messages
        .iter()
        .map(estimate_message_tokens)
        .chain(tools.iter().map(estimate_tool_tokens))
        .sum()
}

/// Route-fit guard for failover/fallback dispatch (#2135 rounds 7-8, P1):
/// before re-sending an UNCHANGED request to an alternate route, resolve
/// that route's readiness and check the ACTUAL request — messages plus
/// serialized tool declarations — plausibly fits its window. A route that
/// cannot fit is SKIPPED like a failed route — an oversized request would
/// be truncated or rejected server-side anyway, with worse failure modes
/// than trying the next lane.
pub(crate) async fn route_fits_request(
    provider: &std::sync::Arc<dyn crate::provider::LlmProvider>,
    messages: &[octos_core::Message],
    tools: &[crate::types::ToolSpec],
) -> bool {
    provider.ensure_ready().await;
    let window = provider.context_window();
    // #2143 part 3: ask the provider itself, so its request-envelope overhead
    // (system-block framing, per-message metadata) is counted instead of only
    // approximated by the margin below. The default impl is the base estimate,
    // so unspecialized providers behave exactly as before.
    let estimated = provider.estimate_request_tokens(messages, tools);
    let margin = (window / 8).clamp(64, 1024);
    estimated <= window.saturating_sub(margin)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2135 round-8 P1: the fit guard measures the ACTUAL request — a
    /// tiny message with a large tool schema must fail a small window.
    #[tokio::test]
    async fn should_count_tool_schemas_in_route_fit() {
        use std::sync::Arc;
        struct Tiny;
        #[async_trait::async_trait]
        impl crate::provider::LlmProvider for Tiny {
            async fn chat(
                &self,
                _m: &[octos_core::Message],
                _t: &[crate::types::ToolSpec],
                _c: &crate::config::ChatConfig,
            ) -> eyre::Result<crate::types::ChatResponse> {
                unreachable!()
            }
            fn model_id(&self) -> &str {
                "tiny"
            }
            fn provider_name(&self) -> &str {
                "local"
            }
        }
        let provider: Arc<dyn crate::provider::LlmProvider> =
            Arc::new(crate::ContextWindowOverride::new(Arc::new(Tiny), 2_000));
        let msg = [octos_core::Message::user("hi")];
        let fat_tool = crate::types::ToolSpec {
            name: "big".into(),
            description: "d".into(),
            input_schema: serde_json::json!({"properties": {"x": "y".repeat(20_000)}}),
        };
        assert!(
            route_fits_request(&provider, &msg, &[]).await,
            "message alone fits"
        );
        assert!(
            !route_fits_request(&provider, &msg, &[fat_tool]).await,
            "tool schema must count against the window"
        );
    }

    #[test]
    fn test_context_window_default() {
        assert_eq!(context_window_tokens("unknown-model"), 128_000);
    }

    #[test]
    fn should_use_1m_context_for_kimi_k3_when_not_in_catalog() {
        // kimi-k3 is never seeded by the unit-test catalog fixtures, so this
        // exercises the hardcoded fallbacks regardless of CATALOG state:
        // 1M window and 131072 default max completion, checked before the
        // broader "kimi" substring branch (65_535).
        assert_eq!(context_window_tokens("kimi-k3"), 1_048_576);
        assert_eq!(context_window_tokens("moonshot/kimi-k3"), 1_048_576);
        assert_eq!(max_output_tokens("kimi-k3"), 131_072);
        assert_eq!(max_output_tokens("moonshot/kimi-k3"), 131_072);
        // The coding plan's bare `k3` / `kimi-for-coding*` ids are also 1M.
        assert_eq!(context_window_tokens("k3"), 1_048_576);
        assert_eq!(
            context_window_tokens("kimi-for-coding-highspeed"),
            1_048_576
        );
        // Guard: a substring `k3` in an unrelated model is NOT the coding plan.
        assert_eq!(context_window_tokens("mock-k3000"), 128_000);
    }

    #[test]
    fn test_catalog_seed_and_lookup() {
        // Hold the write lock across seed + verify to prevent races with
        // parallel tests that also touch the global CATALOG.
        let mut guard = CATALOG.write().unwrap_or_else(|e| e.into_inner());
        let mut map = HashMap::new();
        for (key, ctx, max_out) in [
            ("minimax/minimax-m2.7", 1_000_000u64, 65_536u64),
            ("deepseek/deepseek-chat", 128_000, 8_192),
        ] {
            let entry = CatalogModel {
                context_window: ctx,
                max_output: max_out,
            };
            map.insert(key.to_lowercase(), entry);
            if let Some(model) = key.split('/').next_back() {
                map.insert(
                    model.to_lowercase(),
                    CatalogModel {
                        context_window: ctx,
                        max_output: max_out,
                    },
                );
            }
        }
        *guard = Some(map);

        // Verify lookups while still holding the lock
        let map_ref = guard.as_ref().unwrap();
        let mm = map_ref.get("minimax-m2.7").unwrap();
        assert_eq!(mm.context_window, 1_000_000);
        assert_eq!(mm.max_output, 65_536);
        let ds = map_ref.get("deepseek-chat").unwrap();
        assert_eq!(ds.context_window, 128_000);
        assert_eq!(ds.max_output, 8_192);

        // Clean up
        *guard = None;
    }

    #[test]
    fn test_estimate_tokens_ascii() {
        assert_eq!(estimate_tokens("hello world"), 2);
        assert_eq!(estimate_tokens("a"), 1);
    }

    #[test]
    fn test_estimate_message_tokens() {
        let msg = Message {
            role: octos_core::MessageRole::User,
            content: "Hello, how are you today?".to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        let tokens = estimate_message_tokens(&msg);
        assert_eq!(tokens, estimate_tokens("Hello, how are you today?") + 4);
    }
}
