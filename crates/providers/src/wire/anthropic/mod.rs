//! Anthropic Messages API の wire 形式を扱います。

/// canonical リクエストに `max_tokens` が無い場合に使う既定値。
pub const DEFAULT_MAX_TOKENS: u64 = 4096;

mod convert;
mod stream;
mod types;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub use convert::{from_wire_response, to_finish_reason, to_wire_request};
pub use stream::AnthropicStreamInterpreter;
pub use types::{
    WireContentBlock, WireMessage, WireMessagesRequest, WireMessagesResponse, WireRole, WireTool,
    WireToolResultContent, WireUsage,
};
