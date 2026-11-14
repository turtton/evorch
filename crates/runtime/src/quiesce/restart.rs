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
    pub fn begin_restart_quiesce(&self) -> Result<RestartAttempt, RuntimeError> {
        self.shared.dispatch_tails.result()?;
        let generation = self.shared.quiesce.begin().map_err(runtime::error)?;
        Ok(RestartAttempt {
            owner: self.shared.dispatch_tails.clone(),
            generation,
        })
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
            if !old.active || old.checkpoint.is_none() || inner.unstarted.contains(&run) {
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
