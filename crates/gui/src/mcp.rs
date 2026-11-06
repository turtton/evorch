//! Shared privileged MCP API. UI implementations live in the editor/controller;
//! no tool schema exports or runtime I/O are introduced by this module.
pub use runtime::mcp::McpErrorKind;
pub use runtime::mcp::{ConfirmMcpApproval, McpApprovalPreview, McpServerStatus, McpStatusEvent};
pub use runtime::mcp::{McpGuiService, McpSettingsView, SaveMcpSecret};
