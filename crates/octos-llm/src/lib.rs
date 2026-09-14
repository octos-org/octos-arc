//! LLM provider abstraction for octos.
//!
//! This crate provides a unified interface for interacting with LLM providers:
//! - Anthropic (Claude)
//! - OpenAI (GPT-4)
//! - Google Gemini
//! - Ollama (local models)

pub mod adaptive;
mod config;
pub mod context;
mod context_override;
pub mod credential_pool;
pub mod discovery;
pub mod embedding;
mod failover;
mod fallback;
pub mod lane;
mod local_context_probe;
pub mod local_discovery;
pub mod pricing;
mod provider;
pub mod responsiveness;
mod retry;
pub mod router;
pub mod sse;
pub mod stream_accumulator;
mod swappable;
mod throttle;
mod types;
pub mod vision;

mod cache_manifest;
pub mod catalog;
pub mod error;

pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod openai_responses;
pub mod openrouter;
pub mod registry;
pub mod vertex_auth;

pub use adaptive::{
    AdaptiveConfig, AdaptiveMode, AdaptiveRouter, AdaptiveStatus, AutoEscalationCallback,
    AutoEscalationConfig, AutoEscalationDecision, AutoEscalationEvent, BaselineEntry,
    FailoverEvent, MetricsSnapshot, ModelCatalogEntry, ModelType, QosCatalog, RouterContext,
    SharedMetrics, SharedPolicy, SharedProviderMetrics, StatusCallback, current_router_context,
    derive_cold_start_catalog, with_router_context,
};
pub use cache_manifest::{
    PromptCacheInputComparison, PromptCacheInputManifest, PromptCacheInputSegment,
    PromptCacheObservation, PromptCacheObservedUsage, PromptCacheObserver,
    record_prompt_cache_usage, with_prompt_cache_observation_context,
};
pub use catalog::{ModelCapabilities, ModelCatalog, ModelCost, ModelInfo};
pub use config::{
    CacheRetention, ChatConfig, PromptCacheContext, ReasoningEffort, ResponseFormat,
    SemanticCheckpointHint, ToolChoice,
};
pub use context_override::ContextWindowOverride;
pub use credential_pool::{
    CREDENTIAL_POOL_SCHEMA_VERSION, Credential, CredentialPool, CredentialRotationEvent,
    CredentialState, DEFAULT_COOLDOWN_US, DEFAULT_CREDENTIAL_POOL_DB_FILENAME, ErrorId,
    InMemoryRotationEventSink, NullOAuthRefresher, NullRotationEventSink, OAuthRefresher,
    PersistentCredentialPool, PersistentCredentialPoolOptions, RotationEventSink, RotationStrategy,
    default_credential_pool_path, rotation_reason,
};
pub use embedding::{EmbeddingProvider, OpenAIEmbedder};
pub use error::{LlmError, LlmErrorKind, StreamError};
pub use failover::ProviderChain;
pub use fallback::FallbackProvider;
pub use lane::{
    LANE_CONTEXT, Lane, LaneContext, LaneRoutingConfig, current_lane_context,
    default_lane_candidates, resolve_lane_for_topic, topic_prefix, with_lane_context,
};
pub use local_context_probe::LocalContextProbe;
pub mod middleware;
pub use middleware::MiddlewareStack;
pub use provider::{
    ApiStyle, DEFAULT_EMBEDDING_CONNECT_TIMEOUT_SECS, DEFAULT_EMBEDDING_TIMEOUT_SECS,
    DEFAULT_LLM_CONNECT_TIMEOUT_SECS, DEFAULT_LLM_TIMEOUT_SECS, LaneFailure, LlmProvider,
    OperationalStage, attribute_lane_failures, build_http_client, lane_failure_summary, lane_label,
    operational_error_message, transport_error_message,
};
pub use responsiveness::ResponsivenessObserver;
pub use retry::{RetryConfig, RetryProvider};
pub use router::{ProviderRouter, SubProviderMeta};
pub use stream_accumulator::StreamAccumulator;
pub use swappable::SwappableProvider;
pub use throttle::SemaphoreThrottledProvider;
pub use types::{
    CacheLane, ChatResponse, ChatStream, ProviderMetadata, SemanticCheckpointReport, StopReason,
    StreamEvent, ThinkTagStreamSplitter, TokenUsage, ToolSpec, strip_think_tags,
};
