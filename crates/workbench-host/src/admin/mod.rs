//! Shared management boundary. Leaves implement facades, never their own audit logger.
mod audit;
mod bindings;
mod denial;
mod io;
pub use io::AdminReader;
mod service;
pub use audit::{AdminAudit, AuditPort, AuditReceipt, RestartAuditBinding};
pub use bindings::*;
pub use service::*;
use std::{future::Future, pin::Pin};
pub use workbench_authority::VerifiedConnection;
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    namespaces::lifecycle::{
        AdminOperationState, AdminOutcome, AdminReceipt, LifecycleCommand, LifecycleQuery,
        LifecycleReply,
    },
};
pub type AdminIntent = LifecycleCommand;
pub type AdminQuery = LifecycleQuery;
pub type AdminView = LifecycleReply;
pub type AdminFailure = WorkbenchError;
pub type AdminIo<'a, T> = Pin<Box<dyn Future<Output = Result<T, AdminFailure>> + Send + 'a>>;
pub type AdminContinuation =
    Pin<Box<dyn Future<Output = Result<AdminExecutionOutcome, AdminFailure>> + Send + 'static>>;
pub enum AdminExecutionOutcome {
    Complete(AdminOutcome),
    AwaitRestartRecovery,
}
#[derive(Debug, Clone)]
pub struct AdminAccepted {
    receipt: AdminReceipt,
}
impl AdminAccepted {
    pub fn receipt(&self) -> &AdminReceipt {
        &self.receipt
    }
    pub fn into_receipt(self) -> AdminReceipt {
        self.receipt
    }
}
/// inspect/query may await I/O, without a host/coordinator mutex. commit and
/// the two fast-lane methods MUST only compare/change memory (no I/O or await).
pub trait AdminFacade: Send + Sync + 'static {
    fn inspect<'a>(
        &'a self,
        intent: &'a AdminIntent,
        connection: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminInspection>;
    fn query<'a>(
        &'a self,
        query: &'a AdminQuery,
        connection: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminView>;
    /// Refresh read-only evidence, if needed, after audit acknowledgement and
    /// before the final authorization and memory-only commit.
    fn recheck_before_commit<'a>(&'a self, _intent: &'a AdminIntent) -> AdminIo<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn commit(
        &self,
        intent: AdminIntent,
        expected: AdminInspection,
        admission: CommitAdmission,
        connection: &VerifiedConnection,
    ) -> Result<AdminAccepted, AdminFailure>;
    fn cancel(&self, _intent: &AdminIntent) -> Result<AdminOutcome, AdminFailure> {
        Err(unavailable())
    }
    fn status(&self, _connection: &VerifiedConnection) -> Result<AdminView, AdminFailure> {
        Err(unavailable())
    }
}
pub(crate) fn unavailable() -> AdminFailure {
    failure(
        ErrorCode::UnsupportedCapability,
        "management leaf is not installed",
    )
}
pub(crate) fn unpublished() -> AdminFailure {
    failure(
        ErrorCode::UnsupportedCapability,
        "management is not published",
    )
}
pub(crate) fn conflict() -> AdminFailure {
    failure(ErrorCode::Conflict, "management binding changed")
}
pub(crate) fn busy() -> AdminFailure {
    failure(ErrorCode::Busy, "management capacity is exhausted")
}
pub(crate) fn unknown() -> AdminFailure {
    failure(
        ErrorCode::Internal,
        "management result is unknown; do not retry the effect",
    )
}

pub(crate) fn failure(code: ErrorCode, message: &str) -> AdminFailure {
    WorkbenchError::new(
        code,
        message,
        workbench_protocol::errors::RetryAdvice::Never,
    )
}
