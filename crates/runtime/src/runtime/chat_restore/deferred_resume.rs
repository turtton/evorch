use super::*;
use crate::runtime::cancellation::PendingChatResume;

/// Both the execution and the accepted request are fenced. A later Stop can
/// invalidate a request even after its authority has been taken for dispatch.
pub(in crate::runtime) struct ResumeFence {
    execution: Arc<ExecutionControl>,
    valid: Arc<AtomicBool>,
}

impl ResumeFence {
    pub(super) fn execution(&self) -> Arc<ExecutionControl> {
        Arc::clone(&self.execution)
    }

    pub(in crate::runtime) fn is_current(
        &self,
        execution: Option<&Arc<ExecutionControl>>,
        cancelled: bool,
    ) -> bool {
        !cancelled
            && !self.execution.cancelled()
            && self.valid.load(Ordering::Acquire)
            && execution.is_some_and(|current| Arc::ptr_eq(current, &self.execution))
    }
}

impl AgentRuntime {
    pub(super) fn defer_chat_resume(
        &self,
        run: RunId,
        authority: &RunConfig,
    ) -> Result<bool, RuntimeError> {
        let stop_gate = self.stop_restore_gate(run);
        let _stop_guard = stop_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
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
        // Persist before accepting the scheduling instruction. Current authority
        // stays in memory: recovery must reauthorize this evidence in PR 06.
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
            valid: Arc::new(AtomicBool::new(true)),
        });
        // Duplicate Continue requests coalesce; the latest current authority wins.
        pending.authority = Some(authority.clone());
        Ok(true)
    }

    pub(in crate::runtime) fn resume_deferred_chat(&self, run: RunId) {
        let deferred = {
            let runs = lock_runs(&self.shared.runs);
            let Some(entry) = runs.get(&run) else { return };
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
                pending.authority.take().map(|authority| {
                    (
                        authority,
                        ResumeFence {
                            execution: Arc::clone(&entry.execution),
                            valid: Arc::clone(&pending.valid),
                        },
                    )
                })
            })
        };
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
