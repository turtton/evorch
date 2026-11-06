//! Runtime/GUI MCP service boundary. Constructors are inert; only explicit new-run
//! preparation and approved connection tests may perform external MCP I/O.
mod api;
mod limiter;
mod service;
pub use api::*;
pub use limiter::ProjectMcpLimiter;
pub use service::ProjectMcpService;
pub use tools::mcp::McpErrorKind;

pub use tools::mcp::types::{
    ConfirmMcpApproval, McpApprovalPreview, McpServerStatus, McpStatusEvent,
};
