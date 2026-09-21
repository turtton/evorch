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
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum OutputStatus {
    StillRunning,
    Completed,
    Cancelled,
    Failed,
}

impl AgentRuntime {
    pub(crate) fn run_output(
        &self,
        caller: RunId,
        target: RunId,
    ) -> Result<RunOutput, RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        let caller_entry = runs.get(&caller).ok_or_else(|| unknown_run(caller))?;
        let entry = runs.get(&target).ok_or_else(|| unknown_run(target))?;
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
            AgentRunPhase::Error => {
                let status = match entry.terminal_reason.as_deref() {
                    Some("cancelled") => OutputStatus::Cancelled,
                    _ => OutputStatus::Failed,
                };
                (status, None, entry.terminal_reason.clone())
            }
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => {
                (OutputStatus::StillRunning, None, None)
            }
        };
        Ok(RunOutput {
            run_id: target.to_string(),
            phase,
            status,
            output,
            reason,
        })
    }
}
