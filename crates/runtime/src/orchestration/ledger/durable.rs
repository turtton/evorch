use event_bus::OrchestratorEvent;

use super::{GoalLedger, LedgerError, event_goal_id};

impl GoalLedger {
    /// goal ID のない task イベントを既存の run/task linkage へ解決する。
    pub(crate) fn owns_event(&self, event: &OrchestratorEvent) -> bool {
        match event {
            OrchestratorEvent::TaskProgressed { run_id, .. }
            | OrchestratorEvent::TaskCheckpoint { run_id, .. }
            | OrchestratorEvent::TaskStaleMarked { run_id, .. } => self
                .snapshot
                .attached_runs
                .iter()
                .any(|run| run.run_id == *run_id),
            OrchestratorEvent::TaskRetryScheduled {
                task_id,
                new_run_id,
                ..
            } => {
                self.snapshot.task_runs.contains_key(task_id)
                    || self
                        .snapshot
                        .attached_runs
                        .iter()
                        .any(|run| run.run_id == *new_run_id)
            }
            OrchestratorEvent::GoalCreated { .. }
            | OrchestratorEvent::GoalStateChanged { .. }
            | OrchestratorEvent::GoalStageChanged { .. }
            | OrchestratorEvent::RunAttached { .. }
            | OrchestratorEvent::DeliverableBranchBound { .. }
            | OrchestratorEvent::EvidenceRecorded { .. }
            | OrchestratorEvent::FinishRejected { .. }
            | OrchestratorEvent::FinishAccepted { .. }
            | OrchestratorEvent::ContinuationDispatched { .. }
            | OrchestratorEvent::ContinuationSuppressed { .. }
            | OrchestratorEvent::ReviewRoundStarted { .. }
            | OrchestratorEvent::RepairDispatched { .. }
            | OrchestratorEvent::StallDetected { .. }
            | OrchestratorEvent::NudgeSent { .. }
            | OrchestratorEvent::MergeApprovalRequested { .. }
            | OrchestratorEvent::MergeApprovalResolved { .. }
            | OrchestratorEvent::MergeApprovalInvalidated { .. }
            | OrchestratorEvent::MergeExecuted { .. }
            | OrchestratorEvent::CloseoutStepRecorded { .. }
            | OrchestratorEvent::ShellCommandDenied { .. } => {
                event_goal_id(event) == Some(self.snapshot.goal_id.as_str())
            }
        }
    }

    pub(super) fn apply_task(&mut self, event: &OrchestratorEvent) -> Result<(), LedgerError> {
        if !self.owns_event(event) {
            return Err(LedgerError::UnresolvedEvent(format!("{event:?}")));
        }
        match event {
            OrchestratorEvent::TaskProgressed {
                task_id,
                run_id,
                progress,
                ..
            } => {
                self.snapshot
                    .task_runs
                    .insert(task_id.clone(), run_id.clone());
                self.snapshot
                    .task_progress
                    .insert(task_id.clone(), progress.clone());
            }
            OrchestratorEvent::TaskCheckpoint {
                task_id,
                run_id,
                tool_call_count,
                cumulative_input_tokens,
                cumulative_output_tokens,
                elapsed_ms,
            } => {
                self.snapshot
                    .task_runs
                    .insert(task_id.clone(), run_id.clone());
                self.snapshot
                    .task_checkpoints
                    .entry(task_id.clone())
                    .or_default()
                    .push((
                        *tool_call_count,
                        *cumulative_input_tokens,
                        *cumulative_output_tokens,
                        *elapsed_ms,
                    ));
            }
            OrchestratorEvent::TaskRetryScheduled {
                task_id,
                attempt,
                reason,
                new_run_id,
            } => {
                if !self
                    .snapshot
                    .attached_runs
                    .iter()
                    .any(|run| run.run_id == *new_run_id)
                {
                    let old_run_id = self.snapshot.task_runs.get(task_id);
                    let Some(previous) = self
                        .snapshot
                        .attached_runs
                        .iter()
                        .find(|run| Some(&run.run_id) == old_run_id)
                    else {
                        return Err(LedgerError::UnresolvedEvent(format!("{event:?}")));
                    };
                    let mut retry = previous.clone();
                    retry.run_id = new_run_id.clone();
                    self.snapshot.attached_runs.push(retry);
                }
                self.snapshot
                    .task_runs
                    .insert(task_id.clone(), new_run_id.clone());
                self.snapshot
                    .task_attempts
                    .insert(task_id.clone(), *attempt);
                self.snapshot.task_retries.push((
                    task_id.clone(),
                    *attempt,
                    reason.clone(),
                    new_run_id.clone(),
                ));
            }
            OrchestratorEvent::TaskStaleMarked {
                task_id,
                run_id,
                last_heartbeat_ns,
            } => {
                self.snapshot
                    .task_runs
                    .entry(task_id.clone())
                    .or_insert_with(|| run_id.clone());
                self.snapshot.stale_marks.push((
                    task_id.clone(),
                    run_id.clone(),
                    *last_heartbeat_ns,
                ));
            }
            OrchestratorEvent::GoalCreated { .. }
            | OrchestratorEvent::GoalStateChanged { .. }
            | OrchestratorEvent::GoalStageChanged { .. }
            | OrchestratorEvent::RunAttached { .. }
            | OrchestratorEvent::DeliverableBranchBound { .. }
            | OrchestratorEvent::EvidenceRecorded { .. }
            | OrchestratorEvent::FinishRejected { .. }
            | OrchestratorEvent::FinishAccepted { .. }
            | OrchestratorEvent::ContinuationDispatched { .. }
            | OrchestratorEvent::ContinuationSuppressed { .. }
            | OrchestratorEvent::ReviewRoundStarted { .. }
            | OrchestratorEvent::RepairDispatched { .. }
            | OrchestratorEvent::StallDetected { .. }
            | OrchestratorEvent::NudgeSent { .. }
            | OrchestratorEvent::MergeApprovalRequested { .. }
            | OrchestratorEvent::MergeApprovalResolved { .. }
            | OrchestratorEvent::MergeApprovalInvalidated { .. }
            | OrchestratorEvent::MergeExecuted { .. }
            | OrchestratorEvent::CloseoutStepRecorded { .. }
            | OrchestratorEvent::ShellCommandDenied { .. } => {
                return Err(LedgerError::UnresolvedEvent(format!("{event:?}")));
            }
        }
        Ok(())
    }
}
