//! Irreversible capture and M2 stages 1–11. Raw exec remains disconnected.
#[cfg(target_os = "linux")]
mod later;
#[cfg(not(target_os = "linux"))]
#[path = "finalization/unsupported.rs"]
mod later;
use super::*;
#[cfg(target_os = "linux")]
pub use later::FinalizationEnvironment;
pub use later::FinalizationTarget;
use storage::recovery::restart::RestartTransaction;
use workbench_host::bootstrap::planned_exit::ExitStage;

pub(super) struct BegunCapture(Result<runtime::RestartCaptureSession, RuntimeError>);

impl RestartCoordinator {
    /// Internal wiring point, not a transport operation. The prepared transaction
    /// is created/read back through the existing writer BEFORE calling this.
    /// Returns only acceptance; use wait_finalization for completion (currently
    /// always FailedClosed because raw exec (stage 12) is not connected).
    pub fn start_finalization(
        &self,
        id: RestartId,
        host: &mut BootstrappedHost,
        tx: RestartTransaction,
        target: FinalizationTarget,
    ) -> Result<(), RuntimeError> {
        self.start_finalization_inner(id, host, tx, Some(target))
    }
    #[cfg(test)]
    pub(super) fn start_finalization_without_target(
        &self,
        id: RestartId,
        host: &mut BootstrappedHost,
        tx: RestartTransaction,
    ) -> Result<(), RuntimeError> {
        self.start_finalization_inner(id, host, tx, None)
    }
    fn start_finalization_inner(
        &self,
        id: RestartId,
        host: &mut BootstrappedHost,
        tx: RestartTransaction,
        target: Option<FinalizationTarget>,
    ) -> Result<(), RuntimeError> {
        // Host Stop enters with the host mutex held, then takes the coordinator
        // mutex. Never acquire that host mutex inside the atomic boundary.
        host.validate_planned_exit(&self.0.gate)
            .map_err(|reason| RuntimeError::Model { reason })?;
        let begun = self.capture_boundary(id, &tx)?;
        self.own_finalizer(id, host, begun, tx, target);
        Ok(())
    }
    pub(super) fn capture_boundary(
        &self,
        id: RestartId,
        tx: &RestartTransaction,
    ) -> Result<BegunCapture, RuntimeError> {
        let mut state = self.0.state.lock().unwrap();
        if state.id != Some(id)
            || state.phase != RestartPhase::Sealed
            || state.capture_started
            || state.recipes.is_none()
            || tx.phase != storage::recovery::restart::RestartPhase::Preparing
            || tokio::time::Instant::now() >= state.preparation_deadline.unwrap()
        {
            return Err(RuntimeError::RestartBusy);
        }
        state
            .sealed
            .as_ref()
            .ok_or(RuntimeError::RestartBusy)?
            .validate(&self.0.runtime)?;
        if let Err(reason) = self.0.runtime.seal_learning_for_capture() {
            if let Some(attempt) = &state.attempt {
                let _ = self.0.runtime.fail_restart_attempt(attempt, &reason);
            }
            self.0
                .gate
                .transition(id.generation, HostRestartPhase::FailedClosed);
            self.publish(&mut state, RestartPhase::FailedClosed);
            return Err(RuntimeError::Model { reason });
        }
        let sealed = state.sealed.take().unwrap();
        let recipes = state.recipes.take().unwrap();
        // No I/O or await: the very same mutex used by direct/host Stop covers
        // begin_capture AND publication. A waiter never owns the Sealed value.
        let session = sealed.begin_capture(&self.0.runtime, tx, recipes);
        state.capture_started = true;
        #[cfg(target_os = "linux")]
        if let Some(observer) = &state.shutdown_watchdog {
            observer.capture_started();
        }
        state.finalization_id = Some(id);
        if let Some(worker) = state.worker.take() {
            worker.abort();
        }
        self.0
            .gate
            .transition(id.generation, HostRestartPhase::Capturing);
        self.publish(&mut state, RestartPhase::Capturing);
        Ok(BegunCapture(session))
    }
    pub(super) fn own_finalizer(
        &self,
        id: RestartId,
        host: &mut BootstrappedHost,
        begun: BegunCapture,
        tx: RestartTransaction,
        target: Option<FinalizationTarget>,
    ) {
        // This synchronous transfer cannot be cancelled. Shutdown after capture
        // still transfers resources and starts the owned invalidation/cleanup.
        let resources = host
            .take_planned_exit(&self.0.gate)
            .expect("exclusive host ownership checked before capture");
        let mut state = self.0.state.lock().unwrap();
        state.finalization = Some(resources.clone());
        let core = self.clone();
        let owner_host = host.owner.clone();
        state.worker = Some(
            tokio::spawn(async move {
                core.drive_finalization(id, resources, begun.0, tx, target, owner_host)
                    .await;
            })
            .abort_handle(),
        );
    }
    fn capture_checkpoint(&self, id: RestartId) -> Result<(), RuntimeError> {
        let state = self.0.state.lock().unwrap();
        if state.id != Some(id) || state.phase != RestartPhase::Capturing {
            return Err(RuntimeError::RestartBusy);
        }
        Ok(())
    }
    fn finalization_checkpoint(
        &self,
        id: RestartId,
        completed: Option<ExitStage>,
    ) -> Result<(), String> {
        let mut state = self.0.state.lock().unwrap();
        // Keep factual progress even if shutdown won the race, but never reopen
        // ShuttingDown or authorize another stage after it.
        state.completed_stage = completed;
        if state.id != Some(id)
            || !matches!(
                state.phase,
                RestartPhase::Capturing | RestartPhase::Finalizing
            )
        {
            return Err("planned exit prohibited by lifecycle latch".into());
        }
        if state.attempt.as_ref().is_none_or(|a| {
            self.0.runtime.restart_attempt_state(a) != runtime::quiesce::RestartAttemptState::Active
        }) {
            return Err("planned exit runtime attempt is no longer active".into());
        }
        self.publish(&mut state, RestartPhase::Finalizing);
        Ok(())
    }
    async fn drive_finalization(
        &self,
        id: RestartId,
        resources: Arc<tokio::sync::Mutex<workbench_host::bootstrap::planned_exit::PlannedExit>>,
        session: Result<runtime::RestartCaptureSession, RuntimeError>,
        tx: RestartTransaction,
        target: Option<FinalizationTarget>,
        owner_host: Arc<runtime::ownership::OwnerHost>,
    ) {
        let runtime = self.0.runtime.clone();
        // The coordinator mutex is NOT held while SQLite or graph capture runs.
        // This task remains owned even when every waiter drops or shutdown wins.
        let owner = self.clone();
        let capture = tokio::task::spawn_blocking(move || {
            let mut session = match session {
                Ok(session) => session,
                Err(error) => return (None, Err(error)),
            };
            let result = (|| {
                owner.capture_checkpoint(id)?;
                runtime.persist_restart_capture_expected(&mut session)?;
                owner.capture_checkpoint(id)?;
                runtime.persist_restart_tool_snapshots(&mut session)?;
                owner.capture_checkpoint(id)?;
                runtime.read_restart_capture_records(&session)?;
                owner.capture_checkpoint(id)?;
                runtime.commit_restart_capture(&mut session)?;
                Ok(())
            })();
            // Preserve the session on every error, including a shutdown between
            // DB steps, so invalidation reads back THIS transaction's revision.
            (Some(session), result)
        })
        .await;
        let mut resources = resources.lock().await;
        let (mut session, confirmed) = match capture {
            Ok((session, result)) => (session, result.is_ok()),
            Err(_) => (None, false),
        };
        let result = if confirmed {
            self.finalization_checkpoint(id, None)
        } else {
            Err("restart capture/Commit unconfirmed".into())
        };
        let result = match result {
            Ok(()) => {
                resources
                    .first_five(|stage| self.finalization_checkpoint(id, stage))
                    .await
            }
            Err(error) => Err(error),
        };
        let result = match result {
            Ok(()) => {
                let holds = resources.restart_holds();
                let owner = self.clone();
                let metadata = tx.metadata.clone();
                tokio::task::spawn_blocking(move || owner.six_to_eight(id, target, holds, metadata))
                    .await
                    .map_err(|_| "later finalization worker panicked".to_owned())
                    .and_then(|r| r)
            }
            Err(error) => Err(error),
        };
        let result = match result {
            Ok(armed) => {
                self.nine_to_eleven(id, armed, owner_host, resources.writer())
                    .await
            }
            Err(error) => Err(error),
        };
        // A successful raw exec never returns. Every returned path stays closed;
        // diagnostic ownership has been restored before invalidating this TX.
        let reason = result
            .err()
            .unwrap_or_else(|| "raw exec returned without replacement".into());
        {
            let mut state = self.0.state.lock().unwrap();
            if let Some(attempt) = &state.attempt {
                let _ = self.0.runtime.fail_restart_attempt(attempt, &reason);
            }
            if state.phase != RestartPhase::ShuttingDown {
                self.0
                    .gate
                    .transition(id.generation, HostRestartPhase::FailedClosed);
                self.publish(&mut state, RestartPhase::FailedClosed);
            }
        }
        let runtime = self.0.runtime.clone();
        let writer = resources.writer();
        let invalidated = tokio::task::spawn_blocking(move || {
            if let Some(session) = session.take() {
                runtime.invalidate_restart_capture(&session, &reason)
            } else {
                // A failed capture may already have invalidated its session.
                // Invalidate the SAME tx/revision, never create another capture.
                writer
                    .invalidate_restart(tx.metadata.restart_id, tx.revision, reason)
                    .map(|_| ())
                    .map_err(|e| RuntimeError::Model {
                        reason: e.to_string(),
                    })
            }
        })
        .await;
        if !matches!(invalidated, Ok(Ok(()))) {
            tracing::error!("planned exit invalidation unconfirmed; exec remains prohibited");
        }
        let holds = resources.restart_holds();
        if !matches!(
            tokio::task::spawn_blocking(move || holds.cleanup()).await,
            Ok(Ok(()))
        ) {
            tracing::error!(
                "planned hold invalidation/removal unconfirmed; shared cleanup retains ownership"
            );
        }
        {
            let mut state = self.0.state.lock().unwrap();
            state.finalization_done = true;
            let phase = state.phase;
            self.publish(&mut state, phase);
        }
        // M2 has ended, but reclamation remains owned independently. In particular
        // capture/Commit errors and shutdown before stage 1 must still stop/join.
        if let Err(error) = resources.cleanup().await {
            tracing::error!(%error, "planned exit cleanup failed; resources remain owned");
        }
        self.0.state.lock().unwrap().worker = None;
    }
    /// M2 completion observer only; cleanup may still be joining its owned tails.
    /// Operator shutdown awaits that same cleanup owner. Dropping this observer
    /// has no effect on cleanup. Success is forbidden until D-2c-2c/D-3.
    pub async fn wait_finalization(&self, id: RestartId) -> Result<(), RuntimeError> {
        let mut changed = self.subscribe();
        loop {
            {
                let state = self.0.state.lock().unwrap();
                if state.finalization_id != Some(id) {
                    return Err(RuntimeError::RestartBusy);
                }
                if state.finalization_done {
                    return Err(RuntimeError::RestartBusy);
                }
            }
            changed
                .changed()
                .await
                .map_err(|_| RuntimeError::RestartBusy)?;
        }
    }
}
