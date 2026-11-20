//! Core-owned mailbox and leaf seam. No manual business logic lives here.
use super::*;
use tokio::sync::{mpsc, oneshot};
use workbench_host::bootstrap::core_startup::{
    RecoveryAdminError, RecoveryAdminFuture, RecoveryAdminResult, RecoveryResumeRequest,
};

pub const RECOVERY_ADMIN_QUEUE_CAPACITY: usize = 64;

/// Retains live inputs, never a coordinator borrowing its own fields.
pub struct RecoveryAdminContext {
    pub host: Arc<std::sync::Mutex<workbench_host::CommandHost>>,
    pub startup: StartupJobContext,
}
impl RecoveryAdminContext {
    pub(super) fn for_job(
        coordinator: &StartupCoordinator<'_>,
        host: &Arc<std::sync::Mutex<workbench_host::CommandHost>>,
    ) -> Result<Self, RecoveryAdminError> {
        Ok(Self {
            host: host.clone(),
            startup: coordinator
                .job_context()
                .map_err(|_| RecoveryAdminError::Unavailable)?,
        })
    }
    pub fn coordinator(&self) -> Result<StartupCoordinator<'_>, StartupBlocked> {
        self.startup.coordinator()
    }
}
/// Q2 leaf implements existing-Prepared-only Resume here. Keep the original
/// job/reservation supervised until effects settle: an ACK timeout is not
/// permission to drop an unknown token or start another effect. Operation
/// lookup/replay is the facade's responsibility; this is for new admissions.
pub trait RecoveryAdminHandler: Send + Sync {
    fn resume(
        &self,
        context: &RecoveryAdminContext,
        request: RecoveryResumeRequest,
    ) -> RecoveryAdminResult;
}
pub struct UnsupportedRecoveryAdmin;
impl RecoveryAdminHandler for UnsupportedRecoveryAdmin {
    fn resume(&self, _: &RecoveryAdminContext, _: RecoveryResumeRequest) -> RecoveryAdminResult {
        Err(RecoveryAdminError::Unsupported)
    }
}

pub(super) enum RecoveryRequest {
    Resume {
        request: Box<RecoveryResumeRequest>,
        reply: oneshot::Sender<RecoveryAdminResult>,
    },
    Retry {
        manifest_id: String,
        reply: oneshot::Sender<RecoveryAdminResult>,
    },
}
/// Host callers do mailbox admission only. An automatic attempt holding the
/// host mutex or doing SQLite work cannot block try_send.
pub struct RecoveryAdminActor {
    sender: mpsc::Sender<RecoveryRequest>,
}
impl RecoveryAdminActor {
    pub(super) fn channel() -> (Self, mpsc::Receiver<RecoveryRequest>) {
        let (sender, receiver) = mpsc::channel(RECOVERY_ADMIN_QUEUE_CAPACITY);
        (Self { sender }, receiver)
    }
    fn enqueue(&self, request: RecoveryRequest) -> Result<(), RecoveryAdminError> {
        self.sender.try_send(request).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => RecoveryAdminError::Busy,
            mpsc::error::TrySendError::Closed(_) => RecoveryAdminError::Stopped,
        })
    }
}
impl RecoveryAdminPort for RecoveryAdminActor {
    fn resume(
        &self,
        request: RecoveryResumeRequest,
    ) -> Result<RecoveryAdminFuture, RecoveryAdminError> {
        let (reply, result) = oneshot::channel();
        self.enqueue(RecoveryRequest::Resume {
            request: Box::new(request),
            reply,
        })?;
        Ok(Box::pin(async move {
            result.await.unwrap_or(Err(RecoveryAdminError::Stopped))
        }))
    }
    fn retry_automatic(
        &self,
        manifest_id: String,
    ) -> Result<RecoveryAdminFuture, RecoveryAdminError> {
        let (reply, result) = oneshot::channel();
        self.enqueue(RecoveryRequest::Retry { manifest_id, reply })?;
        Ok(Box::pin(async move {
            result.await.unwrap_or(Err(RecoveryAdminError::Stopped))
        }))
    }
}

pub(super) struct JobGuard {
    jobs: Arc<AtomicUsize>,
    changed: Arc<tokio::sync::Notify>,
}
impl JobGuard {
    pub(super) fn start(jobs: &Arc<AtomicUsize>, changed: &Arc<tokio::sync::Notify>) -> Self {
        jobs.fetch_add(1, Ordering::AcqRel);
        changed.notify_one();
        Self {
            jobs: jobs.clone(),
            changed: changed.clone(),
        }
    }
}
impl Drop for JobGuard {
    fn drop(&mut self) {
        self.jobs.fetch_sub(1, Ordering::AcqRel);
        self.changed.notify_one();
    }
}
pub(super) async fn run_job(
    context: Result<RecoveryAdminContext, RecoveryAdminError>,
    handler: Arc<dyn RecoveryAdminHandler>,
    request: Box<RecoveryResumeRequest>,
    jobs: &Arc<AtomicUsize>,
    changed: &Arc<tokio::sync::Notify>,
) -> RecoveryAdminResult {
    let context = context?;
    let producer = context
        .startup
        .runtime
        .producer_gate()
        .try_enter()
        .ok_or(RecoveryAdminError::Stopped)?;
    let guard = JobGuard::start(jobs, changed);
    // Await even when the client drops its reply future. Stop is processed only
    // after this job settles; no blocking worker is detached.
    tokio::task::spawn_blocking(move || {
        let _job = guard;
        let _producer = producer;
        handler.resume(&context, *request)
    })
    .await
    .unwrap_or(Err(RecoveryAdminError::Unavailable))
}

pub(super) async fn reconcile(runtime: &runtime::AgentRuntime) {
    let runtime = runtime.clone();
    match tokio::task::spawn_blocking(move || runtime.reconcile_automatic_activation_reservations())
        .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "automatic activation readback remains unresolved")
        }
        Err(error) => tracing::warn!(%error, "automatic activation readback worker failed"),
    }
}
