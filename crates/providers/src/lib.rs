//! LLM provider クライアントを統一的に扱うための抽象を提供します。

pub mod auth;
pub mod client;
mod codex_catalog_version;
pub(crate) mod dedup;
pub mod error;
pub mod http;
pub mod message;
mod models;
pub(crate) mod observe;
pub mod provider;
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
pub use error::ProviderError;
pub use message::{
    ChatRequest, ChatResponse, ContentBlock, FinishReason, JsonSchema, Message, ObservationContext,
    ProviderCapabilities, Role, ServiceTier, ToolResultContent, ToolSpec, Usage,
};
pub use models::{CodexModelInfo, list_codex_models, list_models, verify_connectivity};
pub use retry::RetryPolicy;
pub use stream::{DeltaStream, StreamAccumulator, StreamEvent};
