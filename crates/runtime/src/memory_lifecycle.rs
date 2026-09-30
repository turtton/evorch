use std::sync::Weak;

use tokio::sync::watch;

use crate::agent_loop::RunTask;
use crate::memory_queue::{LearningQueue, LearningSettings, QueuedTask};
use crate::runtime::Shared;
use crate::{AgentRuntime, RunConfig, RunId, RuntimeError};

pub(crate) struct PendingLearning {
    settings: LearningSettings,
    prompt: String,
    task_id: String,
    result: watch::Sender<Option<Result<(), String>>>,
}

impl AgentRuntime {
    pub fn with_learning(self, settings: LearningSettings) -> Self {
        let _ = self.shared.learning.set(settings);
        self
    }

    pub(crate) fn prepare_learning(&self, task: &RunTask) -> Option<PendingLearning> {
        if task.parent.is_some() || task.config.learning_internal || task.config.keep_alive {
            return None;
        }
        let settings = self.shared.learning.get()?.clone();
        let mut nonce = [0_u8; 16];
        if getrandom::fill(&mut nonce).is_err() {
            tracing::warn!(run = %task.run_id, "post-run learning identity unavailable");
            return None;
        }
        let (result, receiver) = watch::channel(None);
        self.shared
            .learning_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(task.run_id, receiver);
        Some(PendingLearning {
            settings,
            prompt: task.prompt.clone(),
            task_id: format!("learning-{:032x}", u128::from_le_bytes(nonce)),
            result,
        })
    }

    pub async fn wait_learning(&self, run: RunId) -> Result<Result<(), String>, RuntimeError> {
        self.inspect_agent(run)?;
        let receiver = self
            .shared
            .learning_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .cloned();
        let Some(mut receiver) = receiver else {
            return Ok(Ok(()));
        };
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return Ok(result);
            }
            if receiver.changed().await.is_err() {
                return Ok(Err("learning task ended before completion".into()));
            }
        }
    }
}

impl PendingLearning {
    pub(crate) fn complete(
        self,
        weak: Weak<Shared>,
        run: RunId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            let result = self.execute(weak.clone(), run).await;
            if result.is_err() {
                tracing::warn!(%run, "post-run learning failed; completed run result retained");
                if let Some(shared) = weak.upgrade() {
                    shared.bus.emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                        source: "learning".into(),
                        severity: event_bus::DiagnosticSeverity::Warning,
                        code: "LearningPipelineFailed".into(),
                        detail: "Lesson extraction or review failed; unapproved candidates remain unpromoted. Inspect the learning runs for details.".into(),
                        run_id: Some(run.to_string()),
                        thread_id: None,
                        call_id: None,
                    }));
                }
            }
            self.result.send_replace(Some(result));
        })
    }

    async fn execute(&self, weak: Weak<Shared>, run: RunId) -> Result<(), String> {
        let Some(runtime) = AgentRuntime::from_weak(&weak) else {
            return Ok(());
        };
        if runtime
            .inspect_agent(run)
            .map_err(|error| error.to_string())?
            .phase
            != crate::AgentRunPhase::Done
        {
            return Ok(());
        }
        // Escalated runs can be Done without publishing a completed task result.
        if runtime
            .run_result(run)
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Ok(());
        }
        let queue = LearningQueue::new(
            runtime,
            (self.settings.writer.clone(), self.settings.storage.clone()),
            self.settings.quick.clone(),
        );
        let lessons = queue
            .complete_task(
                &QueuedTask {
                    id: &self.task_id,
                    project: &self.settings.project,
                    prompt: &self.prompt,
                    config: RunConfig::default(),
                },
                run,
            )
            .await
            .map_err(|error| error.to_string())?;
        // Passive intake only after promotion succeeds. Its warn-only API cannot
        // replace the completed learning result with a storage/draft failure.
        if let Some(shared) = weak.upgrade()
            && let Some(settings) = shared.self_improvement.get()
            && settings.policy.collect_lessons
        {
            // complete_task returns the entire reviewed batch, including rejected
            // candidates. Confirm promotion in storage before passive intake;
            // a projection read failure must not fail successful learning either.
            let promoted = (|| -> Result<Vec<_>, storage::StorageError> {
                let database = storage::Database::open(&self.settings.storage)?;
                let mut promoted = Vec::new();
                for lesson in lessons {
                    if database
                        .memory_history(&lesson.id)?
                        .last()
                        .is_some_and(|entry| {
                            entry.status == storage::memory::MemoryStatus::Promoted
                        })
                    {
                        promoted.push(lesson);
                    }
                }
                Ok(promoted)
            })();
            match promoted {
                Ok(lessons) => {
                    crate::self_improvement::ImprovementCollector::new(settings.clone())
                        .ingest_lessons(&lessons);
                }
                Err(error) => {
                    tracing::warn!(%error, "improvement lesson intake skipped; learning result retained")
                }
            }
        }
        Ok(())
    }
}
