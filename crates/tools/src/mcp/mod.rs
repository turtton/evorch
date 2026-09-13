//! Guarded MCP HTTP client, independent of the single-shot search providers.

mod client;
mod error;
mod types;
pub(crate) mod wire;

pub use client::McpClient;
pub use error::{McpError, McpErrorKind};
pub use types::{McpClientConfig, McpClientInfo, McpToolContent, McpToolDefinition, McpToolResult};
