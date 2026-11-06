//! Worker B: canonical approval projection + ledger + GUI preview/confirm.
//! Implement McpApprovalStore; never mint a permit on missing/corrupt storage.
pub use super::types::{
    ApprovalPermit, ApprovalScope, ConfirmMcpApproval, McpApprovalPreview, McpApprovalStore,
};
