use std::sync::{Arc, Weak};

use tokio::sync::watch;

use crate::agent_loop::RunTask;
use crate::memory::Interviewer;
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
            let result = self.execute(weak, run).await;
            if result.is_err() {
                tracing::warn!(%run, "post-run learning failed; completed run result retained");
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
        let Some(report) = runtime.run_result(run).map_err(|error| error.to_string())? else {
            return Ok(());
        };
        let interviewer = Interviewer::new(
            Arc::clone(&runtime.shared.model),
            self.settings.quick.clone(),
            self.settings.writer.clone(),
        );
        let queue = LearningQueue::new(
            runtime,
            (self.settings.writer.clone(), self.settings.storage.clone()),
            interviewer,
        );
        queue
            .complete_task(
                &QueuedTask {
                    id: &self.task_id,
                    project: &self.settings.project,
                    prompt: &self.prompt,
                    config: RunConfig::default(),
                },
                &report,
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}
