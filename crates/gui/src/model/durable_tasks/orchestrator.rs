use event_bus::{CriterionStatus, GateEvidence, GoalState, OrchestratorEvent};
use storage::entity::{TaskContinuation, TaskStatus};

use super::DurableTasksModel;

impl DurableTasksModel {
    pub(super) fn apply_orchestrator(&mut self, event: &OrchestratorEvent) {
        match event {
            OrchestratorEvent::GoalCreated {
                goal_id,
                goal,
                root_run_id,
                ..
            } => {
                self.run_goals.insert(root_run_id.clone(), goal_id.clone());
                let row = self.row(goal_id);
                row.title.clone_from(goal);
                row.goal_id = Some(goal_id.clone());
                row.run_id = Some(root_run_id.clone());
            }
            OrchestratorEvent::RunAttached {
                goal_id, run_id, ..
            } => {
                self.run_goals.insert(run_id.clone(), goal_id.clone());
                for row in self
                    .rows
                    .values_mut()
                    .filter(|row| row.run_id.as_ref() == Some(run_id))
                {
                    row.goal_id = Some(goal_id.clone());
                }
            }
            OrchestratorEvent::GoalStateChanged {
                goal_id,
                to,
                reason,
                ..
            } => {
                let row = self.row(goal_id);
                row.status = match to {
                    GoalState::Active => TaskStatus::Running,
                    GoalState::Paused | GoalState::Blocked => TaskStatus::Blocked,
                    GoalState::Complete => TaskStatus::Completed,
                    GoalState::Cancelled => TaskStatus::Cancelled,
                };
                row.detail.clone_from(reason);
            }
            OrchestratorEvent::TaskProgressed {
                task_id,
                run_id,
                progress,
                reason,
            } => {
                let parsed = serde_json::from_value::<TaskContinuation>(progress.clone()).ok();
                let Some(row) = self.task_for_run(task_id, run_id) else {
                    return;
                };
                row.detail.clone_from(reason);
                if let Some(progress) = parsed {
                    row.status = progress.status;
                    row.attempt = row.attempt.max(progress.attempts);
                    if let Some(artifact) = progress
                        .last_artifact
                        .filter(|value| !value.trim().is_empty())
                    {
                        row.last_artifact = Some(artifact);
                    }
                    if let Some(reason) = progress.failure_reason {
                        row.detail = reason;
                    }
                }
            }
            OrchestratorEvent::TaskCheckpoint {
                task_id,
                run_id,
                tool_call_count,
                cumulative_input_tokens,
                cumulative_output_tokens,
                elapsed_ms,
            } => {
                if let Some(row) = self.task_for_run(task_id, run_id) {
                    row.detail = format!(
                        "checkpoint: {tool_call_count} calls · {cumulative_input_tokens}/{cumulative_output_tokens} tokens · {elapsed_ms} ms"
                    );
                }
            }
            OrchestratorEvent::TaskRetryScheduled {
                task_id,
                attempt,
                reason,
                new_run_id,
            } => {
                let row = self.row(task_id);
                if *attempt <= row.attempt {
                    return;
                }
                row.attempt = *attempt;
                row.run_id = Some(new_run_id.clone());
                row.status = TaskStatus::Retrying;
                row.detail.clone_from(reason);
                if let Some(goal) = row.goal_id.clone() {
                    self.run_goals.insert(new_run_id.clone(), goal);
                }
            }
            OrchestratorEvent::TaskStaleMarked {
                task_id,
                run_id,
                last_heartbeat_ns,
            } => {
                if let Some(row) = self.task_for_run(task_id, run_id) {
                    row.status = TaskStatus::Blocked;
                    row.detail = format!("stale · last heartbeat {last_heartbeat_ns}");
                }
            }
            OrchestratorEvent::EvidenceRecorded { goal_id, evidence } => {
                let artifact = match evidence {
                    GateEvidence::PullRequest { url, .. } => Some(url.as_str()),
                    GateEvidence::Criteria {
                        head_sha,
                        checklist,
                        ..
                    } => checklist
                        .iter()
                        .rev()
                        .filter(|check| check.status == CriterionStatus::Met)
                        .filter_map(|check| check.evidence.as_ref())
                        .filter(|evidence| {
                            evidence.exit_status == 0 && &evidence.target_sha == head_sha
                        })
                        .find_map(|evidence| evidence.artifact_path.as_deref()),
                    GateEvidence::Ci { .. } | GateEvidence::Review { .. } => None,
                };
                if let Some(artifact) = artifact.filter(|value| !value.trim().is_empty()) {
                    for row in self
                        .rows
                        .values_mut()
                        .filter(|row| row.goal_id.as_ref() == Some(goal_id))
                    {
                        row.last_artifact = Some(artifact.into());
                    }
                }
            }
            OrchestratorEvent::CloseoutStepRecorded {
                goal_id,
                ok: true,
                artifact_ref: Some(artifact),
                ..
            } => {
                if !artifact.trim().is_empty() {
                    self.row(goal_id).last_artifact = Some(artifact.clone());
                }
            }
            OrchestratorEvent::GoalStageChanged { .. }
            | OrchestratorEvent::DeliverableBranchBound { .. }
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
            | OrchestratorEvent::ShellCommandDenied { .. } => {}
        }
    }
}
