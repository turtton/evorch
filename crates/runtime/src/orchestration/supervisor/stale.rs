use super::{SupervisorActor, tasks::TaskRequest};
use event_bus::{GoalState, OrchestratorEvent};
use std::time::{SystemTime, UNIX_EPOCH};
use storage::entity::{TaskContinuation, TaskStatus};

impl SupervisorActor {
    pub(super) fn sample_stale_tasks(&mut self) {
        let Ok(elapsed) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return;
        };
        let Ok(now) = u64::try_from(elapsed.as_nanos()) else {
            return;
        };
        let snapshots = {
            let ledgers = match self.ledgers.lock() {
                Ok(ledgers) => ledgers,
                Err(poisoned) => poisoned.into_inner(),
            };
            ledgers
                .values()
                .map(|ledger| ledger.snapshot().clone())
                .collect::<Vec<_>>()
        };
        for snapshot in snapshots {
            if snapshot.detached || snapshot.state != GoalState::Active {
                continue;
            }
            for (task_id, progress) in &snapshot.task_progress {
                let Ok(mut task) = serde_json::from_value::<TaskContinuation>(progress.clone())
                else {
                    continue;
                };
                match task.status {
                    TaskStatus::Running | TaskStatus::Retrying => {}
                    TaskStatus::Pending
                    | TaskStatus::Queued
                    | TaskStatus::Blocked
                    | TaskStatus::Cancelled
                    | TaskStatus::Completed
                    | TaskStatus::Failed => continue,
                }
                let Some(heartbeat) = task.heartbeat_at_ns else {
                    continue;
                };
                if now.saturating_sub(heartbeat)
                    <= self.settings.stale_ttl_secs.saturating_mul(1_000_000_000)
                {
                    continue;
                }
                let Some(run_id) = snapshot.task_runs.get(task_id) else {
                    continue;
                };
                if !snapshot
                    .attached_runs
                    .iter()
                    .any(|run| &run.run_id == run_id && run.role.eq_ignore_ascii_case("worker"))
                {
                    continue;
                }
                self.emit_for_goal(
                    &snapshot.goal_id,
                    OrchestratorEvent::TaskStaleMarked {
                        task_id: task_id.clone(),
                        run_id: run_id.clone(),
                        last_heartbeat_ns: heartbeat,
                    },
                );
                task.status = TaskStatus::Failed;
                task.failure_reason = Some("stale-worker".into());
                self.publish_task(&snapshot.goal_id, task_id, run_id, task);
                self.progress.remove(run_id);
                self.continue_task(TaskRequest {
                    goal_id: snapshot.goal_id.clone(),
                    task_id: task_id.clone(),
                    run_id: run_id.clone(),
                    attempt: snapshot
                        .task_attempts
                        .get(task_id)
                        .copied()
                        .map_or(0, |attempt| attempt),
                    stale: true,
                });
            }
        }
    }
}
