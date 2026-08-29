//! LLM provider クライアントを統一的に扱うための抽象を提供します。

pub mod auth;
pub mod client;
pub mod error;
pub mod http;
pub mod message;
pub mod provider;
pub mod sse;
pub mod stream;
pub mod wire;

pub use auth::ProviderAuth;
pub use client::ProviderClient;
pub use error::ProviderError;
pub use message::{
    ChatRequest, ChatResponse, ContentBlock, FinishReason, Message, ProviderCapabilities, Role,
    ToolResultContent, ToolSpec, Usage,
};
pub use stream::{DeltaStream, StreamAccumulator, StreamEvent};
