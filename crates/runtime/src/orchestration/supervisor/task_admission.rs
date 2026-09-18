use storage::entity::{TaskContinuation, TaskStatus};

use super::{GoalSnapshot, SupervisorActor, SupervisorCommand, tasks::TaskRequest};
use crate::{RunId, RuntimeError};

impl SupervisorActor {
    pub(super) fn task_state(
        &self,
        request: &TaskRequest,
    ) -> Option<(GoalSnapshot, TaskContinuation)> {
        let snapshot = self.snapshot(&request.goal_id)?;
        if snapshot.task_runs.get(&request.task_id) != Some(&request.run_id)
            || snapshot
                .task_attempts
                .get(&request.task_id)
                .map_or(0, |attempt| *attempt)
                != request.attempt
        {
            return None;
        }
        let progress = snapshot.task_progress.get(&request.task_id)?.clone();
        let task = serde_json::from_value(progress).ok()?;
        Some((snapshot, task))
    }

    pub(super) fn watch_task_admission(&self, request: TaskRequest, run: RunId) {
        let runtime = self.runtime.clone();
        let commands = self.command_tx.clone();
        tokio::spawn(async move {
            let result = runtime.wait_admission(run).await;
            let _ = commands.send(SupervisorCommand::TaskAdmission { request, result });
        });
    }

    pub(super) fn task_admission(
        &mut self,
        request: TaskRequest,
        result: Result<(), RuntimeError>,
    ) {
        let Err(error) = result else {
            return;
        };
        let Some((_, mut task)) = self.task_state(&request) else {
            return;
        };
        match task.status {
            TaskStatus::Running | TaskStatus::Retrying => {}
            TaskStatus::Pending
            | TaskStatus::Queued
            | TaskStatus::Blocked
            | TaskStatus::Failed
            | TaskStatus::Cancelled
            | TaskStatus::Completed => return,
        }
        task.status = TaskStatus::Failed;
        task.failure_reason = Some(format!("ProviderUnavailable: {error}"));
        self.progress.remove(&request.run_id);
        self.publish_task(&request.goal_id, &request.task_id, &request.run_id, task);
    }
}
