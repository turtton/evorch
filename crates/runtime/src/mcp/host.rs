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
#[derive(Clone)]
pub struct McpHostServices {
    pub runs: Arc<dyn McpRunService>,
    pub gui: Arc<dyn McpGuiService>,
    lifecycle: Option<Arc<ProjectMcpService>>,
}
impl McpHostServices {
    /// The caller has already selected a keychain-only backend (or unavailable).
    /// No server is started, discovered, or contacted by this composition step.
    pub fn for_user(backend: McpCredentialBackend) -> Self {
        Self::compose(backend, ApprovalLedger::open_user())
    }
    /// Explicit core configuration directory; shares the legacy GUI's consent
    /// ledger when both hosts use the same user directory. Construction is inert.
    pub fn at_config_dir(backend: McpCredentialBackend, directory: std::path::PathBuf) -> Self {
        Self::compose(backend, ApprovalLedger::at_user_config_dir(directory))
    }
    fn compose(
        backend: McpCredentialBackend,
        approvals: Result<ApprovalLedger, McpErrorKind>,
    ) -> Self {
        match NetworkGuard::new() {
            Ok(guard) => {
                let approvals: Arc<dyn McpApprovalStore> = approvals
                    .map(|ledger| Arc::new(ledger) as Arc<dyn McpApprovalStore>)
                    .unwrap_or_else(|_| Arc::new(UnavailableApprovals));
                Self::from_service(ProjectMcpService::new(backend, approvals, Arc::new(guard)))
            }
            Err(_) => {
                let service = Arc::new(UnavailableMcpService::new(backend));
                Self {
                    runs: service.clone(),
                    gui: service,
                    lifecycle: None,
                }
            }
        }
    }
    /// Inert composition of an explicitly supplied store/transport implementation.
    pub fn from_service(service: ProjectMcpService) -> Self {
        let service = Arc::new(service);
        Self {
            runs: service.clone(),
            gui: service.clone(),
            lifecycle: Some(service),
        }
    }
    pub async fn shutdown(&self) -> Result<(), McpErrorKind> {
        match &self.lifecycle {
            Some(service) => service.shutdown().await,
            None => Ok(()),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gui_and_runs_share_the_same_privileged_service_instance() {
        let services = McpHostServices::for_user(McpCredentialBackend::Unavailable(
            SecretUnavailableReason::Unavailable,
        ));
        assert!(std::ptr::addr_eq(
            Arc::as_ptr(&services.runs),
            Arc::as_ptr(&services.gui)
        ));
    }
}
