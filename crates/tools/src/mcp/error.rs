use serde_json::{Value, json};

/// Classified failure only: no underlying error, response, URL or headers are retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum McpErrorKind {
    #[error("timeout")]
    Timeout,
    #[error("transport")]
    Transport,
    #[error("http_status")]
    HttpStatus(u16),
    #[error("protocol")]
    Protocol,
    #[error("server_rejected")]
    ServerRejected,
    #[error("tool_rejected")]
    ToolRejected,
    #[error("configuration")]
    Configuration,
}

/// Safe correlation metadata for a later runtime diagnostic adapter.
#[derive(Debug, Clone, thiserror::Error)]
#[error("MCP {server} {method}: {kind}")]
pub struct McpError {
    pub server: String,
    pub method: &'static str,
    pub request_id: Option<i64>,
    pub kind: McpErrorKind,
}

impl McpError {
    pub fn detail(&self) -> Value {
        let status = match self.kind {
            McpErrorKind::HttpStatus(status) => Some(status),
            McpErrorKind::Timeout
            | McpErrorKind::Transport
            | McpErrorKind::Protocol
            | McpErrorKind::ServerRejected
            | McpErrorKind::ToolRejected
            | McpErrorKind::Configuration => None,
        };
        json!({"server":self.server, "method":self.method, "request_id":self.request_id,
            "class":self.kind.to_string(), "status":status})
    }
}
