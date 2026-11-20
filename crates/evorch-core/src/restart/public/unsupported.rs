//! Unsupported platforms never enter #710 or mutate a gate/transaction.
use workbench_host::admin::*;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::namespaces::lifecycle::AdminOutcome;
pub struct PublicRestartCoordinator;
fn unsupported() -> AdminFailure {
    WorkbenchError::new(
        ErrorCode::UnsupportedCapability,
        "restart requires Linux",
        RetryAdvice::Never,
    )
}
impl AdminFacade for PublicRestartCoordinator {
    fn inspect<'a>(
        &'a self,
        _: &'a AdminIntent,
        _: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminInspection> {
        Box::pin(async { Err(unsupported()) })
    }
    fn query<'a>(&'a self, _: &'a AdminQuery, _: &'a VerifiedConnection) -> AdminIo<'a, AdminView> {
        Box::pin(async { Err(unsupported()) })
    }
    fn commit(
        &self,
        _: AdminIntent,
        _: AdminInspection,
        _: CommitAdmission,
        _: &VerifiedConnection,
    ) -> Result<AdminAccepted, AdminFailure> {
        Err(unsupported())
    }
    fn cancel(&self, _: &AdminIntent) -> Result<AdminOutcome, AdminFailure> {
        Err(unsupported())
    }
    fn status(&self, _: &VerifiedConnection) -> Result<AdminView, AdminFailure> {
        Err(unsupported())
    }
}
