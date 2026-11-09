//! Runtime/GUI MCP service boundary. Constructors are inert; only explicit new-run
//! preparation and approved connection tests may perform external MCP I/O.
mod api;
mod host;
mod limiter;
mod service;
pub use api::*;
pub use host::McpHostServices;
pub use limiter::ProjectMcpLimiter;
pub use service::ProjectMcpService;
pub use tools::mcp::transport::CancelContext;
pub use tools::mcp::{McpErrorKind, McpValueField};

pub use tools::mcp::types::{
    ApprovalFingerprint, ApprovalPreviewId, ApprovalScope, ApprovalWarning, ConfirmMcpApproval,
    McpApprovalPreview, McpApprovalSummary, McpCredentialBackend, McpDestinationSummary,
    McpServerStatus, McpStatusEvent, SecretUnavailableReason,
};
