//! Explicit host composition, separate from the inert injected constructor.
use super::*;
use std::sync::Arc;
use tools::{
    NetworkGuard,
    mcp::{
        approval::ApprovalLedger,
        types::{ApprovalPermit, McpApprovalInspection, McpApprovalStore},
    },
};
/// Both entry points must share one project limiter and one revocation ledger.
pub struct McpHostServices {
    pub runs: Arc<dyn McpRunService>,
    pub gui: Arc<dyn McpGuiService>,
}
impl McpHostServices {
    /// The caller has already selected a keychain-only backend (or unavailable).
    /// No server is started, discovered, or contacted by this composition step.
    pub fn for_user(backend: McpCredentialBackend) -> Self {
        match NetworkGuard::new() {
            Ok(guard) => {
                let approvals: Arc<dyn McpApprovalStore> = ApprovalLedger::open_user()
                    .map(|ledger| Arc::new(ledger) as Arc<dyn McpApprovalStore>)
                    .unwrap_or_else(|_| Arc::new(UnavailableApprovals));
                let service = Arc::new(ProjectMcpService::new(backend, approvals, Arc::new(guard)));
                Self {
                    runs: service.clone(),
                    gui: service,
                }
            }
            Err(_) => {
                let service = Arc::new(UnavailableMcpService::new(backend));
                Self {
                    runs: service.clone(),
                    gui: service,
                }
            }
        }
    }
}
struct UnavailableApprovals;
impl McpApprovalStore for UnavailableApprovals {
    fn preview(&self, _: &McpApprovalInspection) -> Result<McpApprovalPreview, McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
    fn approve(
        &self,
        _: &ConfirmMcpApproval,
        _: &McpApprovalInspection,
    ) -> Result<(), McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
    fn revoke(&self, _: &ApprovalScope) -> Result<(), McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
    fn check(&self, _: &McpApprovalInspection) -> Result<ApprovalPermit, McpErrorKind> {
        Err(McpErrorKind::NeedsApproval)
    }
}
