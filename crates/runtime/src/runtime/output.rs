use serde::Serialize;

use super::{AgentRunPhase, AgentRuntime, RunId, lock_runs, unknown_run};
use crate::RuntimeError;

#[derive(Serialize)]
pub(crate) struct RunOutput {
    run_id: String,
    phase: AgentRunPhase,
    status: OutputStatus,
    output: Option<String>,
    reason: Option<String>,
    /// For a finished direct child: whether a message continues it with its
    /// saved context, which is preferable to delegating the same work again.
    #[serde(skip_serializing_if = "Option::is_none")]
    continuable: Option<bool>,
    /// Why a finished direct child cannot be continued.
    #[serde(skip_serializing_if = "Option::is_none")]
    continuation_blocked: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum OutputStatus {
    StillRunning,
    Waiting,
    Completed,
    Cancelled,
    Stopped,
    Failed,
}

impl AgentRuntime {
    /// Live phase and terminal reason of a registered run, with no relation check.
    /// Only for runs whose conversation the caller is already authorized to read
    /// (ADR 0041 thread tools); `run_output` keeps its parent/child rule.
    pub(crate) fn live_run_status(&self, run: RunId) -> Option<(AgentRunPhase, Option<String>)> {
        let runs = lock_runs(&self.shared.runs);
        let entry = runs.get(&run)?;
        Some((*entry.phase_rx.borrow(), entry.terminal_reason.clone()))
    }

    pub(crate) fn run_output(
        &self,
        caller: RunId,
        target: RunId,
    ) -> Result<RunOutput, RuntimeError> {
        let admission = self.admission_snapshot(target);
        let runs = lock_runs(&self.shared.runs);
        let caller_entry = runs.get(&caller).ok_or_else(|| unknown_run(caller))?;
        let Some(entry) = runs.get(&target) else {
            if caller != target
                && let Some(admission) = admission
                && admission.parent == Some(caller)
            {
                let (phase, status, reason) = match admission.result {
                    None => (AgentRunPhase::Pending, OutputStatus::StillRunning, None),
                    Some(Err(RuntimeError::RunTerminated { .. })) => (
                        AgentRunPhase::Error,
                        OutputStatus::Cancelled,
                        Some("cancelled".into()),
                    ),
                    Some(Err(error)) => (
                        AgentRunPhase::Error,
                        OutputStatus::Failed,
                        Some(error.to_string()),
                    ),
                    Some(Ok(())) => return Err(unknown_run(target)),
                };
                return Ok(RunOutput {
                    run_id: target.to_string(),
                    phase,
                    status,
                    output: None,
                    reason,
                    continuable: None,
                    continuation_blocked: None,
                });
            }
            return Err(unknown_run(target));
        };
        if caller == target || (caller_entry.parent != Some(target) && entry.parent != Some(caller))
        {
            return Err(RuntimeError::MessageDenied {
                sender: caller,
                recipient: target,
                detail: "run_output requires distinct directly related parent/child runs".into(),
            });
        }
        // Terminal publication holds this same lock, so phase, reason and output agree.
        let phase = *entry.phase_rx.borrow();
        let (status, output, reason) = match phase {
            AgentRunPhase::Done => (
                OutputStatus::Completed,
                entry.result_rx.borrow().clone(),
                None,
            ),
            AgentRunPhase::Stopped => (OutputStatus::Stopped, None, entry.terminal_reason.clone()),
            AgentRunPhase::Error => {
                let status = match entry.terminal_reason.as_deref() {
                    Some("cancelled") => OutputStatus::Cancelled,
                    _ => OutputStatus::Failed,
                };
                (status, None, entry.terminal_reason.clone())
            }
            AgentRunPhase::Waiting => (OutputStatus::Waiting, None, None),
            AgentRunPhase::Pending | AgentRunPhase::Running => {
                (OutputStatus::StillRunning, None, None)
            }
        };
        let finished_child = entry.parent == Some(caller)
            && matches!(
                phase,
                AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
            );
        // Saved state is read without holding the runs lock.
        drop(runs);
        let availability = finished_child.then(|| self.continuation_availability(caller, target));
        Ok(RunOutput {
            run_id: target.to_string(),
            phase,
            status,
            output,
            reason,
            continuable: availability.as_ref().map(Result::is_ok),
            continuation_blocked: availability.and_then(Result::err),
        })
    }
}
