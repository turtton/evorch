//! web_search ツールが利用する keyless 検索プロバイダ層。
//!
//! MCP JSON-RPC による単発 tools/call transport と、その envelope 解析を提供する。

pub(crate) mod envelope;
pub mod error;
pub mod mcp;

pub use error::SearchError;
pub use mcp::{McpToolSuccess, McpTransport, NetworkGuardMcpTransport};
