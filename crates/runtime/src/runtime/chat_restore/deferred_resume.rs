use super::*;
use crate::runtime::cancellation::{PendingChatResume, SpawnIntent, current_execution};

/// A host observation of one execution and its immutable incarnation. The watch
/// cursor is sealed: a later Stop/Cancel (even the same value) invalidates it.
#[derive(Clone)]
pub struct ChatResumeObservation {
    run: RunId,
    // Pending observations live on the execution; do not create a self-cycle.
    execution: Option<Weak<ExecutionControl>>,
    interrupt: Option<watch::Receiver<RunInterrupt>>,
    graceful_stop: bool,
    cancelled: bool,
    cancellation_generation: Option<Arc<()>>,
}
impl ChatResumeObservation {
    /// Whether graceful Stop was recorded when this observation was captured.
    pub fn graceful_stop_requested(&self) -> bool {
        self.graceful_stop
    }
    fn is_current(
        &self,
        current: Option<&Arc<ExecutionControl>>,
        intent: Option<&SpawnIntent>,
    ) -> bool {
        let same = match (&self.execution, current) {
            (None, None) => true,
            (Some(observed), Some(current)) => observed
                .upgrade()
                .is_some_and(|observed| Arc::ptr_eq(&observed, current)),
            _ => false,
        };
        let same_generation = match (&self.cancellation_generation, intent) {
            (None, None) => true,
            (Some(observed), Some(current)) => {
                Arc::ptr_eq(observed, &current.cancellation_generation)
            }
            _ => false,
        };
        same && same_generation
            && self
                .interrupt
                .as_ref()
                .is_none_or(|rx| !rx.has_changed().unwrap_or(true))
            && (self.cancelled
                || (!intent.is_some_and(|i| i.cancelled) && current.is_none_or(|e| !e.cancelled())))
    }
}

pub(in crate::runtime) struct ResumeFence {
    observation: ChatResumeObservation,
    valid: Arc<AtomicBool>,
}
impl ResumeFence {
    pub(super) fn observed(observation: ChatResumeObservation) -> Self {
        Self {
            observation,
            valid: Arc::new(AtomicBool::new(true)),
        }
    }
    pub(in crate::runtime) fn is_current(
        &self,
        execution: Option<&Arc<ExecutionControl>>,
        intent: Option<&SpawnIntent>,
    ) -> bool {
        self.valid.load(Ordering::Acquire) && self.observation.is_current(execution, intent)
    }
}

impl AgentRuntime {
    /// Capture the host's decision boundary; not permission to restore. A fresh
    /// observation after terminal Cancel can authorize a new explicit action.
    pub fn observe_chat_resume(&self, run: RunId) -> ChatResumeObservation {
        let intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        let execution = current_execution(run, &intents, &admissions, &runs);
        let interrupt = execution.as_ref().map(|e| e.cancel_tx.subscribe());
        let graceful_stop = interrupt
            .as_ref()
            .is_some_and(|rx| matches!(*rx.borrow(), RunInterrupt::Stop));
        let cancelled = execution.as_ref().is_some_and(|e| e.cancelled());
        ChatResumeObservation {
            run,
            execution: execution.map(|execution| Arc::downgrade(&execution)),
            interrupt,
            graceful_stop,
            cancelled,
            cancellation_generation: intents
                .get(&run)
                .map(|i| Arc::clone(&i.cancellation_generation)),
        }
    }

    pub(super) fn defer_chat_resume(
        &self,
        run: RunId,
        authority: &RunConfig,
        observation: &ChatResumeObservation,
    ) -> Result<bool, RuntimeError> {
        let stop_gate = self.stop_restore_gate(run);
        let _stop_guard = stop_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Same I -> A -> R order as cancellation/registration. Acceptance and its
        // durable evidence refer to exactly the execution observed by the host.
        let intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        let current = current_execution(run, &intents, &admissions, &runs);
        if observation.run != run || !observation.is_current(current.as_ref(), intents.get(&run)) {
            return Err(RuntimeError::RunTerminated {
                run_id: run.to_string(),
            });
        }
        let Some(entry) = runs.get(&run) else {
            return Ok(false);
        };
        if !matches!(
            *entry.phase_rx.borrow(),
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
        ) {
            return Ok(false);
        }
        if entry.execution.cancelled()
            || matches!(*entry.execution.cancel_tx.borrow(), RunInterrupt::Cancel)
        {
            return Err(RuntimeError::RunTerminated {
                run_id: run.to_string(),
            });
        }
        if !matches!(*entry.execution.cancel_tx.borrow(), RunInterrupt::Stop) {
            return Ok(false);
        }
        self.shared
            .quiesce
            .update(
                run,
                &entry.execution.publication().incarnation(),
                self.shared.run_store.get(),
                |intent| intent.resume_requested = true,
            )
            .map_err(|reason| RuntimeError::Model {
                reason: format!("durable intent: {reason}"),
            })?;
        let mut pending = entry
            .execution
            .pending_resume
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = pending.get_or_insert_with(|| PendingChatResume {
            authority: None,
            observation: observation.clone(),
            valid: Arc::new(AtomicBool::new(true)),
        });
        pending.authority = Some(authority.clone());
        pending.observation = observation.clone();
        Ok(true)
    }

    /// Accepted before the restart gate closed; unlike a rejected manual command,
    /// this continuation must survive cancellation of the restart attempt.
    pub(crate) fn accepted_chat_resumes(&self) -> std::collections::BTreeMap<RunId, String> {
        lock_runs(&self.shared.runs)
            .iter()
            .filter(|(_, entry)| entry.execution.has_accepted_resume())
            .map(|(run, entry)| (*run, entry.execution.publication().incarnation()))
            .collect()
    }

    /// Called only after shutdown closed the manual launch gate.
    pub(crate) fn discard_accepted_chat_resumes(&self) -> Result<(), RuntimeError> {
        for run in self.accepted_chat_resumes().into_keys() {
            // Reuse the terminal lease, observation invalidation and durable
            // resume_requested reset of an explicit Stop, including settled runs.
            self.stop(run, crate::StopScope::SelfOnly)?;
        }
        Ok(())
    }

    /// The caller holds manual_launch_gate, after the durable resume grant.
    pub(crate) fn replay_accepted_chat_resumes(&self) {
        for run in self.accepted_chat_resumes().into_keys() {
            self.resume_deferred_chat_with_gate(run);
        }
    }

    pub(in crate::runtime) fn resume_deferred_chat(&self, run: RunId) {
        let _manual = self
            .shared
            .quiesce
            .manual_launch_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.resume_deferred_chat_with_gate(run);
    }

    fn resume_deferred_chat_with_gate(&self, run: RunId) {
        if self.require_manual_resume_open().is_err() {
            return;
        }
        let deferred = {
            let intents = self
                .shared
                .spawn_intents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let runs = lock_runs(&self.shared.runs);
            let Some(entry) = runs.get(&run) else {
                return;
            };
            if !matches!(
                *entry.phase_rx.borrow(),
                AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
            ) || self.shared.quiesce.active(run)
            {
                return;
            }
            let mut pending = entry
                .execution
                .pending_resume
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.as_mut().and_then(|pending| {
                if !pending.valid.load(Ordering::Acquire)
                    || !pending
                        .observation
                        .is_current(Some(&entry.execution), intents.get(&run))
                {
                    return None;
                }
                pending.authority.take().map(|authority| {
                    (
                        authority,
                        ResumeFence {
                            observation: pending.observation.clone(),
                            valid: pending.valid.clone(),
                        },
                    )
                })
            })
        };
        // Never wait for restore/StopGate while R is held. The sealed observation
        // bridges this handoff; all validation/admission/commit runs under StopGate.
        let Some((authority, fence)) = deferred else {
            return;
        };
        if let Err(error) = self.resume_chat_fenced(run, authority, Some(fence)) {
            self.deferred_resume_diagnostic(run, &error.to_string());
        }
    }

    pub(in crate::runtime) fn deferred_resume_diagnostic(&self, run: RunId, detail: &str) {
        self.shared
            .bus
            .emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                source: "runtime".into(),
                severity: event_bus::DiagnosticSeverity::Error,
                code: event_bus::event::diagnostic_codes::CHAT_RESUME_FAILED.into(),
                detail: detail.into(),
                run_id: Some(run.to_string()),
                thread_id: self.goal_thread(run),
                call_id: None,
            }));
    }
}
