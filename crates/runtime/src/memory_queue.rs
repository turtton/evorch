use crate::memory::{InterviewError, InterviewInput, Interviewer, MemoryBoundary};
use crate::{AgentRunPhase, AgentRuntime, Role, RunConfig};
use storage::memory::Lesson;
use storage::{StorageConfig, StorageHandle};

#[derive(Clone)]
pub struct LearningSettings {
    pub writer: StorageHandle,
    pub storage: StorageConfig,
    pub project: String,
    pub quick: crate::ModelPreference,
}

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
                learning_internal: true,
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
        self.complete_task(task, &worker_report).await
    }

    pub async fn complete_task(
        &self,
        task: &QueuedTask<'_>,
        worker_report: &str,
    ) -> Result<Vec<Lesson>, InterviewError> {
        let reviewer = self.runtime.delegate_background(
            Role::Reviewer,
            format!(
                "Review the completed task and verify its evidence. Return a fenced json block with verdict (approve or request-update), findings, and criteria (id: exact verified evidence reference, status: met/unmet/unknown, note). Do not mark evidence met without checking it.\nTask: {}\nWorker report:\n{}",
                task.prompt, worker_report
            ),
            RunConfig {
                learning_internal: true,
                interactive: false,
                keep_alive: false,
                memory: None,
                ..task.config.clone()
            },
        );
        if tokio::time::timeout(
            std::time::Duration::from_secs(120),
            self.runtime.wait(reviewer),
        )
        .await
        .map_err(|_| {
            let _ = self.runtime.cancel(reviewer);
            InterviewError::Timeout
        })?? != AgentRunPhase::Done
        {
            return Err(InterviewError::Incomplete);
        }
        let reviewer_report = self
            .runtime
            .run_result(reviewer)?
            .ok_or(InterviewError::Incomplete)?;
        let lessons = self
            .interviewer
            .interview(&InterviewInput {
                project: task.project,
                task_id: task.id,
                worker_report,
                reviewer_report: &reviewer_report,
            })
            .await?;
        if let Ok(review) = crate::orchestration::review::parse_review_result(&reviewer_report)
            && review.verdict == event_bus::ReviewVerdict::Approve
            && !review.criteria.is_empty()
            && review
                .criteria
                .iter()
                .all(|criterion| criterion.status == event_bus::CriterionStatus::Met)
        {
            for lesson in &lessons {
                if review.criteria.iter().any(|criterion| {
                    criterion.id == lesson.evidence
                        && criterion.status == event_bus::CriterionStatus::Met
                }) {
                    let history =
                        storage::Database::open(&self.config)?.memory_history(&lesson.id)?;
                    match history.last().map(|entry| entry.status) {
                        Some(storage::memory::MemoryStatus::Candidate) => {
                            self.writer.validate_lesson(&lesson.id, &lesson.evidence)?;
                            self.writer.promote_lesson(&lesson.id)?;
                        }
                        Some(storage::memory::MemoryStatus::Validated) => {
                            self.writer.promote_lesson(&lesson.id)?
                        }
                        Some(
                            storage::memory::MemoryStatus::Promoted
                            | storage::memory::MemoryStatus::Rejected,
                        )
                        | None => {}
                    }
                }
            }
        }
        Ok(lessons)
    }
}
