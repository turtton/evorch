//! LLM provider クライアントを統一的に扱うための抽象を提供します。

pub mod auth;
pub mod client;
mod codex_catalog_version;
mod compaction;
pub(crate) mod dedup;
pub mod error;
mod hosted_web_search;
pub mod http;
mod image_generation;
pub mod message;
mod models;
pub(crate) mod observe;
pub mod provider;
pub mod responses_replay;
pub mod retry;
pub mod sse;
pub mod stream;
pub mod wire;

pub use auth::ProviderAuth;
pub use client::ProviderClient;
pub use codex_catalog_version::{
    CODEX_MODELS_FALLBACK_VERSION, CodexCatalogVersion, CodexCatalogVersionResolver,
    CodexClientVersion,
};
pub use compaction::{CompactionResult, Compactor};
pub use error::ProviderError;
pub use hosted_web_search::{
    HostedWebSearch, HostedWebSearchCitation, HostedWebSearchError, HostedWebSearchRequest,
    HostedWebSearchResponse,
};
pub use image_generation::{
    ImageBackground, ImageGeneration, ImageGenerationError, ImageGenerationRequest,
    ImageGenerationResponse, InlineImage,
};
pub use message::{
    ChatRequest, ChatResponse, ContentBlock, FinishReason, JsonSchema, Message, ObservationContext,
    ProviderCapabilities, RequestTurnContext, Role, ServiceTier, ToolResultContent, ToolSpec,
    Usage,
};
pub use models::{CodexModelInfo, list_codex_models, list_models, verify_connectivity};
pub use responses_replay::{
    ResolvedResponsesScope, ResponsesAccount, ResponsesDialect, ResponsesOutputPosition,
    ResponsesReplayContext, ResponsesReplayOrigin, ResponsesSpeed,
};
pub use retry::RetryPolicy;
pub use stream::{DeltaStream, MalformedToolCall, StreamAccumulator, StreamEvent};
