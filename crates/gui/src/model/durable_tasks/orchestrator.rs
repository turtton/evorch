use event_bus::OrchestratorEvent;
use storage::entity::TaskContinuation;

use super::DurableTasksModel;

impl DurableTasksModel {
    pub(super) fn apply_orchestrator(&mut self, event: &OrchestratorEvent) {
        match event {
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
                    if row.title == row.id
                        && let Some(title) = progress.input.as_deref().and_then(|input| {
                            input.lines().map(str::trim).find(|line| !line.is_empty())
                        })
                    {
                        row.title = title.chars().take(160).collect();
                    }
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
            OrchestratorEvent::ThreadGoalUpdated { .. }
            | OrchestratorEvent::ThreadTodoUpdated { .. } => {}
        }
    }
}
