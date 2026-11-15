//! In-process restart preparation. No restore, provider replay or wire entry point.
use super::*;
use crate::{AgentRuntime, RuntimeError};
use std::sync::Arc;

/// Process-local authority. Dropping a waiter or this token is not cancellation.
#[derive(Clone)]
pub struct RestartAttempt {
    owner: Arc<dispatch::DispatchTails>,
    generation: u64,
}

impl RestartAttempt {
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Read-only, attempt-bound lifecycle observation. No callback or authority is issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartAttemptState {
    Active,
    Cancelling,
    Cancelled,
    Failed,
    Superseded,
    ShuttingDown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeReceipt {
    generation: u64,
}

impl Coordinator {
    pub(crate) fn strict_restart_accounting(&self) -> bool {
        let inner = self.lock();
        inner.requested
            && !inner.cancelling
            && !inner.shutting_down
            && !matches!(inner.phase, QuiesceState::Failed { .. })
    }

    pub(crate) fn acknowledge(&self, run: RunId, incarnation: &str, generation: u64) {
        let mut inner = self.lock();
        if !inner.requested || inner.generation != generation || inner.cancelling {
            return;
        }
        let revision = *inner.revisions.get(&run).unwrap_or(&0);
        inner
            .acks
            .insert(run, (generation, incarnation.into(), revision));
        self.changed();
    }

    pub(crate) fn acknowledged(&self, run: RunId, incarnation: &str) -> bool {
        let inner = self.lock();
        inner
            .acks
            .get(&run)
            .is_some_and(|(generation, identity, revision)| {
                *generation == inner.generation
                    && identity == incarnation
                    && *revision == *inner.revisions.get(&run).unwrap_or(&0)
            })
    }

    pub(crate) fn resume_permitted(&self, generation: u64) -> bool {
        let inner = self.lock();
        !inner.requested && !inner.shutting_down && inner.resumed_through >= generation
    }
}

impl AgentRuntime {
    pub(crate) fn require_manual_resume_open(&self) -> Result<(), RuntimeError> {
        if !self.shared.quiesce.running() {
            return Err(RuntimeError::RestartBusy);
        }
        Ok(())
    }
    pub fn begin_restart_quiesce(&self) -> Result<RestartAttempt, RuntimeError> {
        self.shared.dispatch_tails.result()?;
        let generation = self.shared.quiesce.begin().map_err(runtime::error)?;
        Ok(RestartAttempt {
            owner: self.shared.dispatch_tails.clone(),
            generation,
        })
    }

    pub(crate) fn validate_restart_attempt(
        &self,
        attempt: &RestartAttempt,
    ) -> Result<(), RuntimeError> {
        let inner = self.shared.quiesce.lock();
        if !Arc::ptr_eq(&attempt.owner, &self.shared.dispatch_tails)
            || attempt.generation != inner.generation
            || !inner.requested
            || inner.cancelling
            || inner.shutting_down
        {
            return Err(runtime::error("stale restart attempt"));
        }
        if let QuiesceState::Failed { reason } = &inner.phase {
            return Err(runtime::error(reason));
        }
        Ok(())
    }
    /// Close the host input boundary before waiting for accepted host permits.
    /// This is the same marker the first dispatch-drain poll sets, not a drain.
    pub fn begin_restart_sealing(&self, attempt: &RestartAttempt) -> Result<(), RuntimeError> {
        let mut inner = self.shared.quiesce.lock();
        if !Arc::ptr_eq(&attempt.owner, &self.shared.dispatch_tails)
            || attempt.generation != inner.generation
            || !inner.requested
            || inner.cancelling
            || inner.shutting_down
        {
            return Err(runtime::error("stale restart sealer"));
        }
        if let QuiesceState::Failed { reason } = &inner.phase {
            return Err(runtime::error(reason));
        }
        if !inner.sealing {
            inner.sealing = true;
            self.shared.quiesce.changed();
        }
        Ok(())
    }
    /// Fail this attempt closed after a host-side sealing/drain error. Stale callers
    /// cannot affect a replacement attempt; an existing failure is never overwritten.
    pub fn fail_restart_attempt(
        &self,
        attempt: &RestartAttempt,
        reason: impl ToString,
    ) -> Result<(), RuntimeError> {
        let mut inner = self.shared.quiesce.lock();
        if !Arc::ptr_eq(&attempt.owner, &self.shared.dispatch_tails)
            || attempt.generation != inner.generation
            || !inner.requested
            || inner.cancelling
            || inner.shutting_down
        {
            return Err(runtime::error("stale restart failure"));
        }
        if !matches!(inner.phase, QuiesceState::Failed { .. }) {
            inner.phase = QuiesceState::Failed {
                reason: reason.to_string(),
            };
        }
        drop(inner);
        self.shared.quiesce.changed();
        Ok(())
    }
    /// Combine with subscribe_restart_state; stale watch notifications cannot reopen a newer attempt.
    /// Cancelled is published only after durable invalidation and accepted resume/wake replay.
    pub fn restart_attempt_state(&self, attempt: &RestartAttempt) -> RestartAttemptState {
        let inner = self.shared.quiesce.lock();
        if !Arc::ptr_eq(&attempt.owner, &self.shared.dispatch_tails) {
            return RestartAttemptState::Superseded;
        }
        if inner.generation == attempt.generation {
            return if matches!(inner.phase, QuiesceState::Failed { .. }) {
                RestartAttemptState::Failed
            } else if inner.requested {
                RestartAttemptState::Active
            } else {
                RestartAttemptState::Superseded
            };
        }
        if attempt.generation.checked_add(1) != Some(inner.generation) {
            return RestartAttemptState::Superseded;
        }
        if inner.shutting_down {
            return RestartAttemptState::ShuttingDown;
        }
        if inner.cancellation_finished_through == attempt.generation
            && !inner.requested
            && !inner.cancelling
        {
            return RestartAttemptState::Cancelled;
        }
        if matches!(inner.phase, QuiesceState::Failed { .. }) {
            return RestartAttemptState::Failed;
        }
        if inner.cancelling || inner.resumed_through == attempt.generation {
            return RestartAttemptState::Cancelling;
        }
        RestartAttemptState::Superseded
    }

    pub fn subscribe_restart_state(&self) -> watch::Receiver<u64> {
        self.shared.quiesce.subscribe()
    }

    /// Invalidates park checkpoints durably before releasing any live future.
    /// Snapshot receipts, queued input and the existing completion channels stay intact.
    pub fn cancel_restart_quiesce(
        &self,
        attempt: &RestartAttempt,
    ) -> Result<ResumeReceipt, RuntimeError> {
        let _checkpoint = self
            .shared
            .quiesce
            .checkpoint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut inner = self.shared.quiesce.lock();
        if !Arc::ptr_eq(&attempt.owner, &self.shared.dispatch_tails)
            || attempt.generation != inner.generation
            || !inner.requested
            || inner.shutting_down
        {
            return Err(runtime::error("stale restart attempt"));
        }
        if let QuiesceState::Failed { reason } = &inner.phase {
            return Err(runtime::error(reason));
        }
        self.shared.dispatch_tails.result()?;
        self.shared.dispatch_tails.reopen()?;
        inner.cancelling = true;
        inner.generation = inner
            .generation
            .checked_add(1)
            .ok_or_else(|| runtime::error("restart generation exhausted"))?;
        drop(inner);
        self.finish_restart_cancellation(attempt.generation)
    }

    fn finish_restart_cancellation(&self, generation: u64) -> Result<ResumeReceipt, RuntimeError> {
        // A new attempt must not close the gate between durable claim and
        // scheduling the claimed launch under I.
        let _manual = self
            .shared
            .quiesce
            .manual_launch_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let receipt =
            self.with_restart_launch_fence(|| self.invalidate_parked_intents(generation))?;
        self.replay_restart_wakes();
        self.replay_accepted_chat_resumes();
        let mut inner = self.shared.quiesce.lock();
        if inner.generation == generation.saturating_add(1)
            && !inner.requested
            && !inner.cancelling
            && inner.phase == QuiesceState::Running
        {
            inner.cancellation_finished_through = generation;
            self.shared.quiesce.changed();
        }
        Ok(receipt)
    }

    fn invalidate_parked_intents(&self, generation: u64) -> Result<ResumeReceipt, RuntimeError> {
        let coordinator = &self.shared.quiesce;
        // This operation is synchronous with checkpoint capture. No await may
        // occur while holding these locks.
        let mut inner = coordinator.lock();
        if !inner.cancelling
            || inner.shutting_down
            || inner.generation != generation.saturating_add(1)
        {
            return Err(runtime::error("restart cancellation was superseded"));
        }
        if let QuiesceState::Failed { reason } = &inner.phase {
            return Err(runtime::error(reason));
        }
        for (run, old) in inner.intents.clone() {
            let queued = old.wait == ResumeWait::AdmissionQueued;
            if !old.active
                || (!queued && old.checkpoint.is_none())
                || inner.unstarted.contains(&run)
            {
                continue;
            }
            let mut intent = old.clone();
            if intent.snapshot.is_none() {
                intent.snapshot = old
                    .checkpoint
                    .as_ref()
                    .map(|checkpoint| checkpoint.snapshot.clone());
            }
            intent.checkpoint = None;
            if queued {
                intent.wait = ResumeWait::Admission;
            }
            let expected = inner
                .persisted
                .contains(&run)
                .then(|| intent.incarnation.clone());
            if let Err(error) =
                coordinator.save(self.shared.run_store.get(), &intent, expected, None)
            {
                inner.phase = QuiesceState::Failed {
                    reason: error.to_string(),
                };
                coordinator.changed();
                return Err(runtime::error(error));
            }
            inner.intents.insert(run, intent);
        }
        inner.acks.clear();
        inner.blockers.clear();
        inner.resumed_through = generation;
        inner.cancelling = false;
        inner.sealing = false;
        inner.requested = false;
        inner.phase = QuiesceState::Running;
        coordinator.changed();
        Ok(ResumeReceipt { generation })
    }

    /// Runtime's delivery boundary, also used by direct Stop/Cancel callers.
    /// The guard prevents the next drain from passing before terminal registration.
    pub(crate) fn with_terminal_dispatch<T>(
        &self,
        call: impl FnOnce() -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        let _checkpoint = self
            .shared
            .quiesce
            .checkpoint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut inner = self.shared.quiesce.lock();
        let terminal_only = self.shared.dispatch_tails.capture_started()
            || (inner.requested
                && (inner.shutting_down || matches!(inner.phase, QuiesceState::Failed { .. })))
            || self.shared.dispatch_tails.result().is_err();
        let cancel_generation = if !terminal_only
            && inner.requested
            && inner.sealing
            && !inner.cancelling
            && !inner.shutting_down
            && !matches!(inner.phase, QuiesceState::Failed { .. })
        {
            let generation = inner.generation;
            self.shared.dispatch_tails.reopen()?;
            inner.generation = inner
                .generation
                .checked_add(1)
                .ok_or_else(|| runtime::error("restart generation exhausted"))?;
            inner.cancelling = true;
            self.shared.quiesce.changed();
            Some(generation)
        } else {
            if terminal_only {
                self.shared.dispatch_tails.terminal_handoff()?;
            } else {
                self.shared.dispatch_tails.reopen()?;
            }
            None
        };
        let guard = self.shared.dispatch_tails.register_terminal()?;
        drop(inner);
        // Do not hold the checkpoint gate under I/A/R: snapshots may need R.
        drop(_checkpoint);
        let result = guard.enter(call);
        self.discard_cancelled_restart_launches();
        if let Some(generation) = cancel_generation {
            let _checkpoint = self
                .shared
                .quiesce
                .checkpoint_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.finish_restart_cancellation(generation)?;
        }
        result
    }

    pub(crate) fn begin_dispatch_shutdown(&self) -> Result<(), RuntimeError> {
        let _checkpoint = self
            .shared
            .quiesce
            .checkpoint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut inner = self.shared.quiesce.lock();
        if inner.shutting_down {
            return Ok(());
        }
        self.shared.dispatch_tails.terminal_handoff()?;
        inner.generation = inner
            .generation
            .checked_add(1)
            .ok_or_else(|| runtime::error("restart generation exhausted"))?;
        inner.requested = true;
        inner.shutting_down = true;
        inner.cancelling = false;
        self.shared.quiesce.changed();
        Ok(())
    }
}
