//! Guarded MCP HTTP client, independent of the single-shot search providers.

pub mod approval;
mod client;
pub mod codec;
#[cfg(test)]
mod contracts_tests;
mod error;
pub mod headers;
pub mod names;
pub mod protocol;
mod registry;
pub mod secrets;
mod tool;
pub mod transport;
pub mod types;
pub(crate) mod wire;

pub use client::McpClient;
pub use error::{McpError, McpErrorKind, McpValueField};
pub use registry::{
    McpRegistryConnector, McpRegistrySession, McpServerTools, McpToolRegistry, McpToolTransport,
};
pub use tool::McpTool;
pub use types::{
    ApprovalFingerprint, DefinitionFingerprint, FrozenMcpSnapshot, McpCredentialBackend,
    McpServerStatus, RemoteToolKey,
};
pub use types::{McpClientConfig, McpClientInfo, McpToolContent, McpToolDefinition, McpToolResult};
