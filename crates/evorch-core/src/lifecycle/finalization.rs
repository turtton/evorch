//! Single-use prepared commands. Storage/probe/metadata preparation happens on
//! the continuation BEFORE delivery, never on the host-owning loop.
use super::product_exit::ProductExitContext;
use crate::restart::{FinalizationEnvironment, FinalizationTarget, RestartCoordinator, RestartId};
use core_launcher::handoff::{CurrentElf, targets::CaptureCheckedTarget};
use std::{
    io,
    sync::{Arc, Mutex},
};
use storage::recovery::restart::RestartTransaction;
use tokio::sync::{mpsc, oneshot};
use workbench_host::{admin::AdminExecutionOutcome, bootstrap::BootstrappedHost};

/// MEMORY-ONLY injection point for V1b-2b. No DB, hash/probe, waits, capture or
/// alternate start is allowed here. The loop calls it synchronously exactly once
/// per consumed command. An absent builder is finalization unavailable.
pub type EnvironmentBuilder = fn(&ProductExitContext) -> io::Result<FinalizationEnvironment>;

#[derive(Debug, PartialEq, Eq)]
pub enum FinalizationError {
    Conflict,
    Unavailable,
    OwnerStopping,
    Unknown,
    Rejected,
}

struct Delivery {
    last_generation: Option<u64>,
    environment: Option<EnvironmentBuilder>,
}
#[derive(Clone)]
pub struct FinalizationHandle {
    send: mpsc::Sender<PreparedFinalization>,
    delivery: Arc<Mutex<Delivery>>,
    core: RestartCoordinator,
}
/// Claimed BEFORE any Create/readback/probe work. Drop/failure never resets it.
/// There is no retry or Clone API, including after send/reply loss.
pub struct FinalizationPermit {
    id: RestartId,
    handle: FinalizationHandle,
}
pub(super) struct PreparedFinalization {
    id: RestartId,
    tx: RestartTransaction,
    current: Arc<CurrentElf>,
    checked: CaptureCheckedTarget,
    reply: oneshot::Sender<Result<(), FinalizationError>>,
}
/// Moved out of the driver into the loop's scope. Drop is memory-only and also
/// covers early errors/unwind, not merely the normal shutdown tail.
pub(super) struct FinalizationInbox {
    recv: mpsc::Receiver<PreparedFinalization>,
}
pub(super) fn channel(core: RestartCoordinator) -> (FinalizationHandle, FinalizationInbox) {
    let (send, recv) = mpsc::channel(1);
    (
        FinalizationHandle {
            send,
            core,
            delivery: Arc::new(Mutex::new(Delivery {
                last_generation: None,
                environment: None,
            })),
        },
        FinalizationInbox { recv },
    )
}
impl FinalizationHandle {
    /// Registers the leaf once. No replacement/fallback builder is accepted.
    pub fn install_environment_builder(
        &self,
        builder: EnvironmentBuilder,
    ) -> Result<(), FinalizationError> {
        let mut delivery = self.delivery.lock().unwrap();
        if delivery.environment.is_some() {
            return Err(FinalizationError::Conflict);
        }
        delivery.environment = Some(builder);
        Ok(())
    }
    /// The same coordinator as the lifecycle, never a newly constructed owner.
    pub fn coordinator(&self) -> &RestartCoordinator {
        &self.core
    }
    pub fn claim(&self, id: RestartId) -> Result<FinalizationPermit, FinalizationError> {
        let mut delivery = self.delivery.lock().unwrap();
        if self.send.is_closed() {
            return Err(FinalizationError::OwnerStopping);
        }
        if delivery.environment.is_none() {
            return Err(FinalizationError::Unavailable);
        }
        if self.core.snapshot().id != Some(id)
            || delivery
                .last_generation
                .is_some_and(|last| id.generation() <= last)
        {
            return Err(FinalizationError::Conflict);
        }
        delivery.last_generation = Some(id.generation());
        Ok(FinalizationPermit {
            id,
            handle: self.clone(),
        })
    }
}
impl FinalizationPermit {
    /// tx is the once-created, metadata-checked Preparing readback. Only the
    /// receipt is awaited: Accepted immediately releases the management task.
    pub async fn start(
        self,
        tx: RestartTransaction,
        current: Arc<CurrentElf>,
        checked: CaptureCheckedTarget,
    ) -> Result<AdminExecutionOutcome, FinalizationError> {
        let (reply, receipt) = oneshot::channel();
        self.handle
            .send
            .send(PreparedFinalization {
                id: self.id,
                tx,
                current,
                checked,
                reply,
            })
            .await
            .map_err(|_| FinalizationError::OwnerStopping)?;
        accepted(receipt).await
    }
}
async fn accepted(
    receipt: oneshot::Receiver<Result<(), FinalizationError>>,
) -> Result<AdminExecutionOutcome, FinalizationError> {
    receipt.await.map_err(|_| FinalizationError::Unknown)??;
    Ok(AdminExecutionOutcome::AwaitRestartRecovery)
}
impl FinalizationInbox {
    pub(super) async fn recv(&mut self) -> Option<PreparedFinalization> {
        self.recv.recv().await
    }
    pub(super) fn close(&mut self) {
        self.recv.close();
        while let Ok(command) = self.recv.try_recv() {
            let _ = command.reply.send(Err(FinalizationError::OwnerStopping));
        }
    }
}
impl Drop for FinalizationInbox {
    fn drop(&mut self) {
        self.close();
    }
}
#[cfg(test)]
#[path = "finalization_tests.rs"]
mod tests;

impl PreparedFinalization {
    pub(super) fn execute(
        self,
        host: &mut BootstrappedHost,
        context: &ProductExitContext,
        handle: &FinalizationHandle,
    ) {
        let result = (|| {
            let builder = handle
                .delivery
                .lock()
                .unwrap()
                .environment
                .ok_or(FinalizationError::Unavailable)?;
            let environment = builder(context).map_err(|_| FinalizationError::Unavailable)?;
            let target =
                FinalizationTarget::new(self.current, self.checked).with_environment(environment);
            handle
                .core
                .start_finalization(self.id, host, self.tx, target)
                .map_err(|_| FinalizationError::Rejected)
        })();
        let _ = self.reply.send(result);
    }
}
