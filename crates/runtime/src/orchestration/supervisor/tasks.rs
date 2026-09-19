use storage::entity::{TaskContinuation, TaskStatus};

use super::*;

pub(super) struct TaskRequest {
    pub(super) goal_id: String,
    pub(super) task_id: String,
    pub(super) run_id: String,
    pub(super) attempt: u32,
    pub(super) stale: bool,
}

impl SupervisorHandle {
    pub fn resume_task(&self, task_id: &str) -> Result<(), SupervisorError> {
        self.tx
            .send(SupervisorCommand::ResumeTask(self.task_request(task_id)?))
            .map_err(|_| SupervisorError::Closed)
    }

    pub fn retry_task(&self, task_id: &str) -> Result<(), SupervisorError> {
        self.tx
            .send(SupervisorCommand::RetryTask(self.task_request(task_id)?))
            .map_err(|_| SupervisorError::Closed)
    }

    pub fn cancel_task(&self, task_id: &str) -> Result<(), SupervisorError> {
        self.tx
            .send(SupervisorCommand::CancelTask(self.task_request(task_id)?))
            .map_err(|_| SupervisorError::Closed)
    }

    fn task_request(&self, task_id: &str) -> Result<TaskRequest, SupervisorError> {
        let ledgers = match self.ledgers.lock() {
            Ok(ledgers) => ledgers,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut owners = ledgers.values().filter_map(|ledger| {
            let snapshot = ledger.snapshot();
            let run_id = snapshot.task_runs.get(task_id)?.clone();
            Some(TaskRequest {
                goal_id: snapshot.goal_id.clone(),
                task_id: task_id.into(),
                run_id,
                stale: false,
                attempt: snapshot
                    .task_attempts
                    .get(task_id)
                    .map_or(0, |attempt| *attempt),
            })
        });
        match (owners.next(), owners.next()) {
            (Some(request), None) => Ok(request),
            _ => Err(SupervisorError::UnknownTask(task_id.into())),
        }
    }
}

impl SupervisorActor {
    pub(super) fn continue_task(&mut self, request: TaskRequest) {
        let Some((snapshot, mut task)) = self.task_state(&request) else {
            return;
        };
        match snapshot.state {
            GoalState::Cancelled | GoalState::Complete => return,
            GoalState::Active | GoalState::Paused | GoalState::Blocked => {}
        }
        // An adopted running generation has no process; it resumes from its durable cursor.
        if snapshot.detached
            && !self.progress.contains_key(&request.run_id)
            && matches!(task.status, TaskStatus::Running | TaskStatus::Retrying)
        {
            task.status = TaskStatus::Failed;
        }
        task.attempts = request.attempt.max(task.attempts);
        match continuation::decide_task(&task, self.settings.max_continuations) {
            ContinuationDecision::Suppress(reason) => {
                self.suppress(&request.goal_id, snapshot.epoch, reason);
                return;
            }
            ContinuationDecision::Dispatch => {}
        }
        let Some(previous) = snapshot
            .attached_runs
            .iter()
            .find(|run| run.run_id == request.run_id)
        else {
            return;
        };
        let role_name = previous.role.to_lowercase();
        let Some(role) = [
            Role::Orchestrator,
            Role::Worker,
            Role::Reviewer,
            Role::Explorer,
            Role::Librarian,
            Role::Planner,
            Role::Oracle,
            Role::MultimodalLooker,
        ]
        .into_iter()
        .find(|role| role.name().to_lowercase() == role_name) else {
            return;
        };
        let parent = previous
            .parent_run_id
            .as_deref()
            .and_then(|id| id.strip_prefix("run-")?.parse::<u64>().ok())
            .map(RunId::new);
        let mut run = self.runtime.reserve_run_id();
        while snapshot
            .attached_runs
            .iter()
            .any(|attached| attached.run_id == run.to_string())
        {
            run = self.runtime.reserve_run_id();
        }
        if (!snapshot.detached || self.progress.contains_key(&request.run_id))
            && let Some(old) = self.find_run(&request.run_id)
        {
            let _ = self.runtime.cancel(old);
        }
        task.attempts += 1;
        task.status = if request.stale {
            TaskStatus::Retrying
        } else {
            TaskStatus::Running
        };
        task.heartbeat_at_ns = None;
        let prompt = match serde_json::to_string(&task) {
            Ok(context) => format!(
                "Continue the durable task from this saved state, preserving completed work:\n{context}"
            ),
            Err(_) => return,
        };
        self.runtime.track_goal_run(run, &request.run_id);
        if self.snapshot(&request.goal_id).is_none_or(|current| {
            matches!(current.state, GoalState::Cancelled | GoalState::Complete)
        }) {
            return;
        }
        self.emit_for_goal(
            &request.goal_id,
            OrchestratorEvent::TaskRetryScheduled {
                goal_id: request.goal_id.clone(),
                task_id: request.task_id.clone(),
                attempt: task.attempts,
                reason: if request.stale {
                    "stale-worker"
                } else {
                    "operator continuation"
                }
                .into(),
                new_run_id: run.to_string(),
            },
        );
        self.publish_task(&request.goal_id, &request.task_id, &run.to_string(), task);
        self.runtime.spawn_reserved(
            run,
            parent,
            role,
            prompt,
            RunConfig {
                task_id: Some(request.task_id.clone()),
                name: Some(format!("{}/task{}", request.goal_id, request.attempt + 1)),
                workspace_branch: snapshot.deliverable_branch,
                ..RunConfig::default()
            },
        );
        self.progress
            .insert(run.to_string(), ProgressTrack::new(AgentRunPhase::Pending));
        self.watch_task_admission(
            TaskRequest {
                run_id: run.to_string(),
                attempt: request.attempt + 1,
                ..request
            },
            run,
        );
    }

    pub(super) fn cancel_task(&mut self, request: TaskRequest) {
        let Some((snapshot, mut task)) = self.task_state(&request) else {
            return;
        };
        match task.status {
            TaskStatus::Cancelled | TaskStatus::Completed => return,
            TaskStatus::Pending
            | TaskStatus::Queued
            | TaskStatus::Blocked
            | TaskStatus::Retrying
            | TaskStatus::Running
            | TaskStatus::Failed => task.status = TaskStatus::Cancelled,
        }
        task.failure_reason = Some("cancelled by operator".into());
        if (!snapshot.detached || self.progress.contains_key(&request.run_id))
            && let Some(run) = request
                .run_id
                .strip_prefix("run-")
                .and_then(|id| id.parse::<u64>().ok())
                .map(RunId::new)
        {
            let _ = self.runtime.cancel(run);
        }
        self.publish_task(&request.goal_id, &request.task_id, &request.run_id, task);
    }

    pub(super) fn task_phase(&self, run_id: &str, phase: AgentRunPhase, reason: Option<&str>) {
        let status = match phase {
            AgentRunPhase::Done => TaskStatus::Completed,
            AgentRunPhase::Error => match reason {
                Some("cancelled") => TaskStatus::Cancelled,
                Some(_) | None => TaskStatus::Failed,
            },
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => return,
        };
        for goal in self.goals_for_run(run_id) {
            let Some(snapshot) = self.snapshot(&goal) else {
                continue;
            };
            for (task_id, current_run) in &snapshot.task_runs {
                if current_run != run_id {
                    continue;
                }
                let Some(progress) = snapshot.task_progress.get(task_id) else {
                    continue;
                };
                let Ok(mut task) = serde_json::from_value::<TaskContinuation>(progress.clone())
                else {
                    continue;
                };
                if !matches!(task.status, TaskStatus::Running | TaskStatus::Retrying) {
                    continue;
                }
                task.status = status;
                if let Some(reason) = reason {
                    task.failure_reason = Some(reason.into());
                }
                self.publish_task(&goal, task_id, run_id, task);
            }
        }
    }

    pub(super) fn publish_task(&self, goal: &str, task: &str, run: &str, state: TaskContinuation) {
        if let Ok(progress) = serde_json::to_value(state) {
            self.emit_for_goal(
                goal,
                OrchestratorEvent::TaskProgressed {
                    task_id: task.into(),
                    run_id: run.into(),
                    progress,
                    reason: "task state transition".into(),
                },
            );
        }
    }
}
