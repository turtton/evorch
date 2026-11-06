use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Value-less configuration location. Never includes names or expanded values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum McpValueField {
    #[error("command")]
    Command,
    #[error("args")]
    Args,
    #[error("cwd")]
    Cwd,
    #[error("env")]
    Env,
    #[error("url")]
    Url,
    #[error("headers")]
    Headers,
    #[error("token")]
    Token,
}

/// Classified failure only: no underlying error, response, URL or headers are retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
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
    #[error("config_conflict")]
    ConfigConflict,
    #[error("needs_approval")]
    NeedsApproval,
    #[error("approval_revoked")]
    ApprovalRevoked,
    #[error("stale_preview")]
    StalePreview,
    #[error("secret_unavailable")]
    SecretUnavailable,
    #[error("secret_locked")]
    SecretLocked,
    #[error("secret_denied")]
    SecretDenied,
    #[error("secret_missing")]
    SecretMissing,
    #[error("missing_environment_{0}")]
    MissingEnvironment(McpValueField),
    #[error("process_limit_reached")]
    ProcessLimitReached,
    #[error("unavailable")]
    Unavailable,
    #[error("cancelled")]
    Cancelled,
    #[error("outcome_unknown")]
    OutcomeUnknown,
    #[error("mrtr_unsupported")]
    InputRequired,
    #[error("limit_exceeded")]
    LimitExceeded,
    #[error("definition_changed")]
    DefinitionChanged,
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
            | McpErrorKind::Configuration
            | McpErrorKind::ConfigConflict
            | McpErrorKind::NeedsApproval
            | McpErrorKind::ApprovalRevoked
            | McpErrorKind::StalePreview
            | McpErrorKind::SecretUnavailable
            | McpErrorKind::SecretLocked
            | McpErrorKind::SecretDenied
            | McpErrorKind::SecretMissing
            | McpErrorKind::MissingEnvironment(_)
            | McpErrorKind::ProcessLimitReached
            | McpErrorKind::Unavailable
            | McpErrorKind::Cancelled
            | McpErrorKind::OutcomeUnknown
            | McpErrorKind::InputRequired
            | McpErrorKind::LimitExceeded
            | McpErrorKind::DefinitionChanged => None,
        };
        json!({"server":self.server, "method":self.method, "request_id":self.request_id,
            "class":self.kind.to_string(), "status":status})
    }
}
