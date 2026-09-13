//! Guarded MCP HTTP client, independent of the single-shot search providers.

mod client;
mod error;
mod registry;
mod tool;
mod types;
pub(crate) mod wire;

pub use client::McpClient;
pub use error::{McpError, McpErrorKind};
pub use registry::McpToolRegistry;
pub use tool::McpTool;
pub use types::{McpClientConfig, McpClientInfo, McpToolContent, McpToolDefinition, McpToolResult};
