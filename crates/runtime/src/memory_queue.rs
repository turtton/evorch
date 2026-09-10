use crate::memory::{InterviewError, InterviewInput, Interviewer, MemoryBoundary};
use crate::{AgentRunPhase, AgentRuntime, Role, RunConfig};
use storage::memory::Lesson;
use storage::{StorageConfig, StorageHandle};

pub struct QueuedTask<'a> {
    pub id: &'a str,
    pub project: &'a str,
    pub prompt: &'a str,
    pub config: RunConfig,
}

pub struct LearningQueue {
    runtime: AgentRuntime,
    writer: StorageHandle,
    config: StorageConfig,
    interviewer: Interviewer,
}

impl LearningQueue {
    pub fn new(
        runtime: AgentRuntime,
        storage: (StorageHandle, StorageConfig),
        interviewer: Interviewer,
    ) -> Self {
        Self {
            runtime,
            writer: storage.0,
            config: storage.1,
            interviewer,
        }
    }

    pub async fn execute(&self, task: QueuedTask<'_>) -> Result<Vec<Lesson>, InterviewError> {
        let boundary = MemoryBoundary::capture(&self.config, task.project)?;
        self.writer.start_task(task.id)?;
        let result = self.execute_started(&task, boundary).await;
        self.writer.finish_task(task.id, result.is_ok())?;
        result
    }

    async fn execute_started(
        &self,
        task: &QueuedTask<'_>,
        boundary: MemoryBoundary,
    ) -> Result<Vec<Lesson>, InterviewError> {
        let worker = self.runtime.delegate_background(
            Role::Worker,
            task.prompt.into(),
            RunConfig {
                memory: Some(boundary),
                ..task.config.clone()
            },
        );
        if self.runtime.wait(worker).await? != AgentRunPhase::Done {
            return Err(InterviewError::Incomplete);
        }
        let worker_report = self
            .runtime
            .run_result(worker)?
            .ok_or(InterviewError::Incomplete)?;
        let reviewer = self.runtime.delegate_background(
            Role::Reviewer,
            format!(
                "Review the completed task and its evidence.\nTask: {}\nWorker report:\n{}",
                task.prompt, worker_report
            ),
            RunConfig {
                memory: None,
                ..task.config.clone()
            },
        );
        if self.runtime.wait(reviewer).await? != AgentRunPhase::Done {
            return Err(InterviewError::Incomplete);
        }
        let reviewer_report = self
            .runtime
            .run_result(reviewer)?
            .ok_or(InterviewError::Incomplete)?;
        self.interviewer
            .interview(&InterviewInput {
                project: task.project,
                task_id: task.id,
                worker_report: &worker_report,
                reviewer_report: &reviewer_report,
            })
            .await
    }
}
