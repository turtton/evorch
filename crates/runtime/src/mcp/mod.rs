//! Runtime/GUI MCP service boundary. Constructors are inert; only explicit new-run
//! preparation and approved connection tests may perform external MCP I/O.
mod api;
mod limiter;
pub use api::*;
pub use limiter::ProjectMcpLimiter;

pub use tools::mcp::types::{
    ConfirmMcpApproval, McpApprovalPreview, McpServerStatus, McpStatusEvent,
};
