//! LLM provider abstraction for octos.
//!
//! This crate provides a unified interface for interacting with LLM providers:
//! - Anthropic (Claude)
//! - OpenAI (GPT-4)
//! - Google Gemini
//! - DeepSeek / Z.AI / Moonshot coding-plan families

mod config;
pub mod context;
mod context_override;
pub mod embedding;
mod failover;
mod fallback;
pub mod pricing;
mod provider;
mod retry;
pub mod sse;
pub mod stream_accumulator;
mod types;
pub mod vision;

mod cache_manifest;
pub mod catalog;
pub mod error;

pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod openai_responses;
pub mod registry;
pub mod vertex_auth;

pub use cache_manifest::{
    PromptCacheInputComparison, PromptCacheInputManifest, PromptCacheInputSegment,
    PromptCacheObservation, PromptCacheObservedUsage, PromptCacheObserver, RouterContext,
    current_router_context, record_prompt_cache_usage, with_prompt_cache_observation_context,
    with_router_context,
};
pub use catalog::{
    ModelCapabilities, ModelCatalog, ModelCatalogEntry, ModelCost, ModelInfo, ModelType, QosCatalog,
};
pub use config::{
    CacheRetention, ChatConfig, PromptCacheContext, ReasoningEffort, ResponseFormat,
    SemanticCheckpointHint, ToolChoice,
};
pub use context_override::ContextWindowOverride;
pub use embedding::{EmbeddingProvider, OpenAIEmbedder};
pub use error::{LlmError, LlmErrorKind, StreamError};
pub use failover::ProviderChain;
pub use fallback::FallbackProvider;
pub use provider::{
    ApiStyle, DEFAULT_EMBEDDING_CONNECT_TIMEOUT_SECS, DEFAULT_EMBEDDING_TIMEOUT_SECS,
    DEFAULT_LLM_CONNECT_TIMEOUT_SECS, DEFAULT_LLM_TIMEOUT_SECS, LaneFailure, LlmProvider,
    OperationalStage, attribute_lane_failures, build_http_client, lane_failure_summary, lane_label,
    operational_error_message, transport_error_message,
};
pub use retry::{RetryConfig, RetryProvider};
pub use stream_accumulator::StreamAccumulator;
pub use types::{
    CacheLane, ChatResponse, ChatStream, ProviderMetadata, SemanticCheckpointReport, StopReason,
    StreamEvent, ThinkTagStreamSplitter, TokenUsage, ToolSpec, strip_think_tags,
};
