use super::*;
use core_launcher::handoff::{CurrentElf, targets::CaptureCheckedTarget};
use sha2::{Digest, Sha256};
use storage::recovery::restart::RestartMetadata;
use workbench_host::bootstrap::planned_exit::RestartHolds;

/// Consumes only the result of the reversible pre-capture child reprobe.
/// No transaction, target or exec capability is returned to a waiter.
pub struct FinalizationTarget {
    current: Arc<CurrentElf>,
    environment: Option<FinalizationEnvironment>,
    checked: CaptureCheckedTarget,
    #[cfg(test)]
    pub(crate) observe: Option<Arc<dyn Fn(ExitStage) + Send + Sync>>,
    #[cfg(test)]
    pub(crate) after: Option<Arc<dyn Fn(ExitStage) + Send + Sync>>,
}
impl FinalizationTarget {
    pub fn new(current: Arc<CurrentElf>, checked: CaptureCheckedTarget) -> Self {
        Self {
            current,
            checked,
            environment: None,
            #[cfg(test)]
            observe: None,
            #[cfg(test)]
            after: None,
        }
    }
    pub fn with_environment(mut self, environment: FinalizationEnvironment) -> Self {
        self.environment = Some(environment);
        self
    }
    fn check(&self, metadata: &RestartMetadata) -> Result<(), String> {
        let target = self
            .checked
            .before_exec(&self.current)
            .map_err(|e| e.to_string())?;
        let request = self.checked.request();
        let description = target.description();
        if metadata.target_identity != request.id()
            || metadata.successor_build != description.build_identity
            || metadata.handoff_version != description.handoff_format
            || metadata.snapshot_version != description.snapshot_format
            || metadata.storage_version < description.schema.minimum
            || metadata.storage_version > description.schema.maximum
        {
            return Err("checked target differs from captured transaction".into());
        }
        Ok(())
    }
    fn observe(&self, stage: ExitStage) {
        #[cfg(test)]
        if let Some(observe) = &self.observe {
            observe(stage);
        }
        #[cfg(not(test))]
        let _ = stage;
    }
}
impl RestartCoordinator {
    pub(super) fn six_to_eight(
        &self,
        id: RestartId,
        target: Option<FinalizationTarget>,
        holds: Arc<RestartHolds>,
        metadata: RestartMetadata,
    ) -> Result<ArmedFinalization, String> {
        let target = target.ok_or("capture-checked target is not owned; exec prohibited")?;
        target.observe(ExitStage::TargetChecked);
        self.finalization_checkpoint(id, Some(ExitStage::StorageDrained))?;
        target.check(&metadata)?;
        #[cfg(test)]
        if let Some(after) = &target.after {
            after(ExitStage::TargetChecked);
        }
        self.finalization_checkpoint(id, Some(ExitStage::TargetChecked))?;

        target.observe(ExitStage::OwnersHeld);
        self.finalization_checkpoint(id, Some(ExitStage::TargetChecked))?;
        let committed = holds.read_transaction(&metadata)?;
        let held = holds.save_and_read_back(&committed)?;
        #[cfg(test)]
        if let Some(after) = &target.after {
            after(ExitStage::OwnersHeld);
        }
        self.finalization_checkpoint(id, Some(ExitStage::OwnersHeld))?;

        target.observe(ExitStage::Armed);
        self.finalization_checkpoint(id, Some(ExitStage::OwnersHeld))?;
        let nonce_hash = format!(
            "sha256:{:x}",
            Sha256::digest(uuid::Uuid::new_v4().as_bytes())
        );
        let armed = holds.arm_and_read_back(&held, nonce_hash)?;
        #[cfg(test)]
        if let Some(after) = &target.after {
            after(ExitStage::Armed);
        }
        self.finalization_checkpoint(id, Some(ExitStage::Armed))?;
        Ok(ArmedFinalization { target, armed })
    }
}

/// Internal wiring only. Product restart remains unregistered/unadvertised.
/// Instance, entry mask, sole signal reader and actual control-drain owner are
/// retained, never replaced by serializable booleans.
pub struct FinalizationEnvironment {
    instance: Arc<core_instance::Instance>,
    mask: Arc<core_instance::handoff::BlockedShutdownSignals>,
    signals: crate::lifecycle::OperatorSignals,
    control: crate::lifecycle::control::ControlDrainHandle,
    args: Vec<std::ffi::OsString>,
}
impl FinalizationEnvironment {
    pub fn new(
        instance: Arc<core_instance::Instance>,
        mask: Arc<core_instance::handoff::BlockedShutdownSignals>,
        signals: crate::lifecycle::OperatorSignals,
        control: crate::lifecycle::control::ControlDrainHandle,
        args: Vec<std::ffi::OsString>,
    ) -> Self {
        Self {
            instance,
            mask,
            signals,
            control,
            args,
        }
    }
}
pub(super) struct ArmedFinalization {
    target: FinalizationTarget,
    armed: RestartTransaction,
}

impl RestartCoordinator {
    pub(super) async fn nine_to_eleven(
        &self,
        id: RestartId,
        armed: ArmedFinalization,
        owner: Arc<runtime::ownership::OwnerHost>,
        writer: storage::StorageHandle,
    ) -> Result<(), String> {
        use core_instance::handoff::boot::{BOOT_TIMEOUT_SECS, BootDeadline};
        self.finalization_checkpoint(id, Some(ExitStage::Armed))?;
        // Restarting starts exactly once. Neither control waits nor heavy
        // preparation can mint a second deadline.
        let origin = tokio::time::Instant::now();
        let deadline = BootDeadline::start().map_err(text)?;
        let core = self.clone();
        let armed = tokio::task::spawn_blocking(move || {
            use core_instance::handoff::receipt::RestartBootReceipt;
            let environment = armed
                .target
                .environment
                .as_ref()
                .ok_or("exec environment not owned")?;
            armed.target.observe(ExitStage::ExecPrepared);
            core.finalization_checkpoint(id, Some(ExitStage::Armed))?;
            let tx = &armed.armed;
            if environment.instance.identity != tx.metadata.config_identity {
                return Err("instance differs from armed transaction".into());
            }
            let receipt = RestartBootReceipt::restarting(
                environment.instance.config_identity(),
                tx.metadata.restart_id.clone(),
                tx.metadata.old_epoch,
                tx.metadata.successor_epoch,
            )
            .map_err(text)?;
            environment
                .instance
                .config_identity()
                .write_restart_receipt(&receipt)
                .map_err(text)?;

            check_deadline(deadline)?;
            Ok::<_, String>(armed)
        })
        .await
        .map_err(|_| "Restarting receipt worker panicked")??;
        let environment = armed
            .target
            .environment
            .as_ref()
            .ok_or("exec environment missing")?;
        let control = tokio::time::timeout_at(
            origin + std::time::Duration::from_secs(BOOT_TIMEOUT_SECS),
            environment.control.drain(),
        )
        .await
        .map_err(|_| "control drain exceeded fixed boot deadline")?
        .map_err(text)?;
        control
            .validate(
                &armed.armed.metadata.config_identity,
                armed.armed.metadata.old_epoch,
            )
            .map_err(text)?;
        let core = self.clone();
        tokio::task::spawn_blocking(move || {
            core.prepare_and_stop_owner(id, armed, owner, control, deadline, writer)
        })
        .await
        .map_err(|_| "exec preparation worker panicked".to_owned())?
    }
    fn prepare_and_stop_owner(
        &self,
        id: RestartId,
        armed: ArmedFinalization,
        owner: Arc<runtime::ownership::OwnerHost>,
        _control: crate::lifecycle::control::ControlDrained,
        deadline: core_instance::handoff::boot::BootDeadline,
        writer: storage::StorageHandle,
    ) -> Result<(), String> {
        use core_instance::handoff::HandoffRecord;
        let target = &armed.target;
        let environment = target
            .environment
            .as_ref()
            .ok_or("exec environment missing")?;
        let tx = &armed.armed;
        self.finalization_checkpoint(id, Some(ExitStage::Armed))?;
        let checked = target.checked.before_exec(&target.current).map_err(text)?;
        // This object borrows Instance (the lifetime lock), while ArmedFinalization
        // owns the checked target and the exact read-back tx/revision/nonce.
        // CString backing, pointer arrays, ticket and FD rollback data are owned
        // by PreparedExec. No preparation is repeated after owner stop.
        let _prepared = environment
            .instance
            .prepare_exec_handoff(
                &environment.mask,
                checked.canonical_entrypoint(),
                &environment.args,
                HandoffRecord {
                    restart_id: tx.metadata.restart_id.clone(),
                    config_identity: tx.metadata.config_identity.clone(),
                    predecessor_epoch: tx.metadata.old_epoch,
                    successor_epoch: tx.metadata.successor_epoch,
                    nonce_hash: tx.ticket_nonce_hash.clone().ok_or("armed nonce missing")?,
                    successor_build: tx.metadata.successor_build.clone(),
                    boot_deadline: deadline,
                },
            )
            .map_err(text)?;
        writer.barrier().map_err(text)?;
        check_deadline(deadline)?;
        #[cfg(test)]
        if let Some(after) = &target.after {
            after(ExitStage::ExecPrepared);
        }
        self.finalization_checkpoint(id, Some(ExitStage::ExecPrepared))?;

        target.observe(ExitStage::SignalsStopped);
        let stopped = environment.signals.stop().map_err(text)?;
        stopped.check_pending().map_err(text)?;
        #[cfg(test)]
        if let Some(after) = &target.after {
            after(ExitStage::SignalsStopped);
        }
        self.finalization_checkpoint(id, Some(ExitStage::SignalsStopped))?;
        check_deadline(deadline)?;
        stopped.check_pending().map_err(text)?;
        target.observe(ExitStage::OwnerStopped);
        self.finalization_checkpoint(id, Some(ExitStage::SignalsStopped))?;
        stopped.check_pending().map_err(text)?;
        // Register recovery BEFORE stopping: even partial stop/unlink failure or
        // unwind must recover heartbeat/diagnostic ownership, never business work.
        let recovery = FailedClosedOwner {
            core: self,
            owner: &owner,
        };
        let result = owner.stop_worker_for_exec().map_err(text);
        // NO await / DB / allocation-heavy work on the successful side of this
        // boundary. D-3 will insert only pending/latch check + prepared raw call.
        // Today take the explicitly prohibited branch immediately, synchronously.
        self.fail_finalization_closed(if result.is_ok() {
            Some(ExitStage::OwnerStopped)
        } else {
            None
        });
        drop(recovery); // failed branch only: heartbeat/diagnostic restoration
        #[cfg(test)]
        if result.is_ok()
            && let Some(after) = &target.after
        {
            after(ExitStage::OwnerStopped);
        }
        drop(stopped); // same registered AsyncFd restored, including pending signals
        result?;
        Err("raw exec is prohibited until D-3".into())
    }
    fn fail_finalization_closed(&self, completed: Option<ExitStage>) {
        let mut state = self.0.state.lock().unwrap();
        if let Some(completed) = completed {
            state.completed_stage = Some(completed);
        }
        if let Some(attempt) = &state.attempt {
            let _ = self
                .0
                .runtime
                .fail_restart_attempt(attempt, "raw exec prohibited");
        }
        if state.phase != RestartPhase::ShuttingDown {
            self.0
                .gate
                .transition(state.generation, HostRestartPhase::FailedClosed);
            self.publish(&mut state, RestartPhase::FailedClosed);
        }
    }
}
struct FailedClosedOwner<'a> {
    core: &'a RestartCoordinator,
    owner: &'a runtime::ownership::OwnerHost,
}
impl Drop for FailedClosedOwner<'_> {
    fn drop(&mut self) {
        self.core.fail_finalization_closed(None);
        if let Err(error) = self.owner.restart_worker_failed_closed() {
            tracing::error!(%error, "owner heartbeat recovery unconfirmed; business remains closed");
        }
    }
}
fn check_deadline(deadline: core_instance::handoff::boot::BootDeadline) -> Result<(), String> {
    if deadline.elapsed().map_err(text)?
        >= std::time::Duration::from_secs(core_instance::handoff::boot::BOOT_TIMEOUT_SECS)
    {
        Err("fixed boot deadline expired".into())
    } else {
        Ok(())
    }
}
fn text(e: impl ToString) -> String {
    e.to_string()
}
