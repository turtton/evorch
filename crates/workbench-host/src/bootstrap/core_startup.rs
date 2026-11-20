//! Core-only startup boundary, after shared leases/reload and before producers.
use super::{BootstrapError, BootstrapOptions, services::BackgroundServices};
use crate::projects::StaticProjects;
use runtime::{AgentRuntime, ownership::OwnerHost};
use std::{future::Future, pin::Pin, sync::Arc};

/// Chosen by the bootstrap entry, not inferred from runtime proof registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreBootMode {
    Cold,
    PlannedHandoff,
}

/// Owned current-process inputs. No recipe can manufacture this lease proof.
pub struct CoreStartupInputs {
    pub host: Arc<std::sync::Mutex<crate::CommandHost>>,
    pub mode: CoreBootMode,
    pub options: BootstrapOptions,
    pub runtime: AgentRuntime,
    pub owner: Arc<OwnerHost>,
    pub storage: storage::StorageConfig,
    pub writer: storage::StorageHandle,
    pub projects: StaticProjects,
    pub startup: storage::RecoveryStartupComplete,
    pub work_changed: Arc<tokio::sync::Notify>,
}
/// Payload-free diagnostics; callers must not include recipes or provider input.
pub trait CoreStartupDiagnostics: Send + Sync {
    fn snapshot(&self) -> serde_json::Value;
    fn has_pending_work(&self) -> bool;
    fn recovery_admin(&self) -> &dyn RecoveryAdminPort {
        &UnsupportedRecoveryAdminPort
    }
}
pub type CoreStartupResult = Result<Arc<dyn CoreStartupDiagnostics>, BootstrapError>;
pub type CoreStartupFuture<'a> = Pin<Box<dyn Future<Output = CoreStartupResult> + Send + 'a>>;
/// Implemented by the core, never invoked by legacy/embedded bootstrap.
pub trait CoreStartupHook: Sync {
    fn start<'a>(
        &'a self,
        inputs: CoreStartupInputs,
        services: &'a mut BackgroundServices,
    ) -> CoreStartupFuture<'a>;
}

/// A current-CAS request, carrying the non-cloneable, process-local reservation.
/// Neither the binding nor the operation key can manufacture execution authority.
pub struct RecoveryResumeRequest {
    pub manifest_id: String,
    pub binding: workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphBinding,
    pub effect: storage::recovery::graph_control::GraphEffectKey,
    pub reservation: runtime::ManualActivationReservation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAdminError {
    Unsupported,
    Busy,
    Stopped,
    Unavailable,
    Blocked {
        problem: workbench_protocol::namespaces::lifecycle::owned_graphs::GraphAuthorityProblem,
        detail: workbench_protocol::namespaces::lifecycle::owned_graphs::GraphProblemDetail,
    },
}
pub type RecoveryAdminResult = Result<Vec<runtime::RunId>, RecoveryAdminError>;
pub type RecoveryAdminFuture = Pin<Box<dyn Future<Output = RecoveryAdminResult> + Send>>;
/// Host-owned port. Implementations must enqueue synchronously without DB, owner,
/// or provider calls. Dropping the returned future must not cancel an admitted job.
/// Operation query/replay belongs to the facade: it must never call this port.
pub trait RecoveryAdminPort: Send + Sync {
    fn resume(
        &self,
        request: RecoveryResumeRequest,
    ) -> Result<RecoveryAdminFuture, RecoveryAdminError>;
    /// Only the original supervisor may re-enqueue a confirmed Open/Blocked graph.
    /// This does not acquire authority or resolve an unknown reservation.
    fn retry_automatic(
        &self,
        manifest_id: String,
    ) -> Result<RecoveryAdminFuture, RecoveryAdminError>;
}
/// Embedded hosts and builds without the Q2 leaf have no manual recovery effects.
pub struct UnsupportedRecoveryAdminPort;
impl RecoveryAdminPort for UnsupportedRecoveryAdminPort {
    fn resume(&self, _: RecoveryResumeRequest) -> Result<RecoveryAdminFuture, RecoveryAdminError> {
        Err(RecoveryAdminError::Unsupported)
    }
    fn retry_automatic(&self, _: String) -> Result<RecoveryAdminFuture, RecoveryAdminError> {
        Err(RecoveryAdminError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LegacyDiagnostics;
    impl CoreStartupDiagnostics for LegacyDiagnostics {
        fn snapshot(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn has_pending_work(&self) -> bool {
            false
        }
    }
    #[test]
    fn legacy_startup_exposes_only_the_unsupported_admin_port() {
        let diagnostics: &dyn CoreStartupDiagnostics = &LegacyDiagnostics;
        assert!(matches!(
            diagnostics.recovery_admin().retry_automatic("graph".into()),
            Err(RecoveryAdminError::Unsupported)
        ));
        assert!(!diagnostics.has_pending_work());
    }
}
