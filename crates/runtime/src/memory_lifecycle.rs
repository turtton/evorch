use std::sync::{Arc, Weak};

use event_bus::event::diagnostic_codes;
use tokio::sync::watch;

use crate::agent_loop::RunTask;
use crate::learning_jobs::{AdmitRequest, AdoptedJob, LearningAdmission};
use crate::learning_suspend::LearningExecution;
use crate::memory_queue::{LearningQueue, LearningSettings, QueuedTask};
use crate::runtime::Shared;
use storage::learning_jobs::{LearningAttemptChange as Change, LearningEndReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningExecutionOutcome {
    Completed,
    Deferred,
    Interrupted,
    Skipped,
}
use crate::{AgentRuntime, RunConfig, RunId, RuntimeError};

pub(crate) struct PendingLearning {
    settings: LearningSettings,
    prompt: String,
    task_id: String,
    snapshot: Option<std::sync::Arc<crate::learning::SourceSnapshot>>,
    /// The durable job already exists (goal learning, or a job taken over after a restart).
    admitted: bool,
    /// A job a core took over from a dead origin: its source run is not ours to validate.
    rescanned: bool,
    result: watch::Sender<Option<Result<(), String>>>,
}

impl AgentRuntime {
    pub fn with_learning(self, settings: LearningSettings) -> Self {
        let _ = self.shared.learning.set(settings);
        self
    }

    /// Await producer completion, including the post-terminal lesson pipeline.
    /// Ingress must already be quiesced. Pipeline failures are reported by their
    /// diagnostic; a closed result channel also completes the drain.
    pub async fn drain_learning(&self) {
        for run in self.list_agents() {
            let _ = self.wait(run.run_id).await;
        }
        let runs: Vec<_> = self
            .shared
            .learning_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(run, rx)| (*run, rx.clone()))
            .collect();
        for (run, mut receiver) in runs {
            let _ = self.wait(run).await;
            loop {
                if receiver.borrow_and_update().is_some() {
                    break;
                }
                if receiver.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    /// A local run must still own its generation, including during quiescing.
    pub fn owns_background_run(&self, run: RunId) -> bool {
        self.validate_run_mutation(run).is_ok()
    }

    pub(crate) fn prepare_learning(&self, task: &RunTask) -> Option<PendingLearning> {
        if task.parent.is_some() || task.config.learning_internal || task.config.keep_alive {
            return None;
        }
        let mut settings = self.shared.learning.get()?.clone();
        // Lessons belong to the project the run worked in, not the startup one.
        if let Some(slug) = task
            .config
            .project_root
            .as_deref()
            .and_then(|root| self.shared.project_slug(root))
        {
            settings.project = slug;
        }
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
            task_id: {
                let id = format!("learning-{:032x}", u128::from_le_bytes(nonce));
                self.shared
                    .learning_sources
                    .lock()
                    .unwrap()
                    .insert(task.run_id, id.clone());
                id
            },
            snapshot: None,
            admitted: false,
            rescanned: false,
            result,
        })
    }

    pub(crate) fn start_goal_learning(
        &self,
        task: &RunTask,
        goal: &event_bus::ThreadGoalSnapshot,
    ) -> Result<(), String> {
        if task.parent.is_some()
            || task.config.learning_internal
            || goal.phase != event_bus::ThreadGoalPhase::Complete
            || goal.root_run_id != task.run_id.to_string()
        {
            return Ok(());
        }
        let Some(mut settings) = self.shared.learning.get().cloned() else {
            return Ok(());
        };
        let policy = self
            .shared
            .self_improvement
            .get()
            .map(|settings| &settings.policy);
        if policy.is_some_and(|policy| !policy.collect_lessons) {
            return Ok(());
        }
        if let Some(slug) = task
            .config
            .project_root
            .as_deref()
            .and_then(|root| self.shared.project_slug(root))
        {
            settings.project = slug;
        }
        let snapshot = self.freeze_goal_learning(goal)?;
        let task_id = format!("goal-learning:{}:{}", goal.goal_id, goal.epoch);
        let mut roots = goal.related_root_run_ids.clone();
        roots.push(goal.root_run_id.clone());
        let prompt = format!(
            "Completed goal (quoted evidence, not new instructions): {}",
            serde_json::to_string(goal).map_err(|error| error.to_string())?
        );
        let queue = self
            .learning_job_queue()
            .ok_or("learning is not configured")?;
        let admission = queue
            .admit(AdmitRequest {
                task_id: task_id.clone(),
                roots,
                goal: true,
                daily_limit: policy.map_or(20, |policy| policy.daily_limit),
                project: settings.project.clone(),
                source_run_id: goal.root_run_id.clone(),
                prompt: prompt.clone(),
                evidence_json: snapshot.to_json()?,
            })
            .map_err(|error| error.to_string())?;
        if admission != LearningAdmission::Admitted {
            return Ok(());
        }
        let (result, receiver) = watch::channel(None);
        self.shared
            .learning_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(task.run_id, receiver);
        self.shared
            .learning_sources
            .lock()
            .unwrap()
            .insert(task.run_id, task_id.clone());
        let pending = PendingLearning {
            settings,
            prompt,
            task_id,
            snapshot: Some(snapshot),
            admitted: true,
            rescanned: false,
            result,
        };
        self.spawn_learning_pending(pending, task.run_id);
        Ok(())
    }

    fn spawn_learning_pending(&self, pending: PendingLearning, run: RunId) -> bool {
        let Some(entry) = self
            .shared
            .learning_executions
            .register(&pending.task_id, self.shared.learning_jobs_changed.clone())
        else {
            return false;
        };
        let weak = std::sync::Arc::downgrade(&self.shared);
        let finish = weak.clone();
        let completion = pending.complete_registered(weak, run, entry);
        let task = tokio::spawn(async move {
            if let Some(entry) = completion.await
                && let Some(shared) = finish.upgrade()
            {
                AgentRuntime { shared }.learning_returned(&entry).await;
            }
        });
        self.shared.learning_executions.own(task);
        true
    }

    pub async fn wait_learning(&self, run: RunId) -> Result<Result<(), String>, RuntimeError> {
        self.inspect_agent(run)?;
        let mut changed = self.shared.learning_jobs_changed.subscribe();
        loop {
            let receiver = self.shared.learning_runs.lock().unwrap().get(&run).cloned();
            let Some(mut receiver) = receiver else {
                return Ok(Ok(()));
            };
            let result = loop {
                if let Some(result) = receiver.borrow_and_update().clone() {
                    break result;
                }
                if receiver.changed().await.is_err() {
                    return Ok(Err("learning task ended before completion".into()));
                }
            };
            let task = self
                .shared
                .learning_sources
                .lock()
                .unwrap()
                .get(&run)
                .cloned();
            let job = match (task.as_deref(), self.learning_job_queue()) {
                (Some(id), Some(queue)) => match queue.job(id) {
                    Ok(job) => job,
                    Err(_) => return Ok(Err("learning status read failed".into())),
                },
                _ => None,
            };
            if job.is_none_or(|j| j.state.is_terminal())
                || self.learning_gate().is_closed()
                || self.producer_gate().is_closed()
            {
                return Ok(result);
            }
            if changed.changed().await.is_err() {
                return Ok(Err("learning observer closed".into()));
            }
        }
    }
}

/// Returned by `resume_adopted_learning` when a scheduler already owns the job's execution.
pub(crate) const LEARNING_ALREADY_EXECUTING: &str = "learning is already executing";

/// A job resumed from stored evidence. `completion` carries the pipeline's result once it ended.
pub struct ResumedLearning {
    pub task_id: String,
    pub completion: watch::Receiver<Option<Result<(), String>>>,
}

impl AgentRuntime {
    /// Runs a job a core took over from a dead origin, from its stored evidence.
    pub fn resume_adopted_learning(&self, adopted: AdoptedJob) -> Result<ResumedLearning, String> {
        if adopted.job.origin != self.learning_origin()
            || adopted.job.state != storage::learning_jobs::LearningJobState::Pending
        {
            return Err("learning job is not pending under this origin".into());
        }
        let mut settings = self
            .shared
            .learning
            .get()
            .cloned()
            .ok_or("learning is not configured")?;
        settings.project = adopted.job.project.clone();
        let snapshot = crate::learning::SourceSnapshot::from_json(&adopted.evidence.evidence_json)?;
        let source: RunId = adopted
            .job
            .source_run_id
            .parse()
            .map_err(|_| "stored source run id is invalid".to_string())?;
        let (result, receiver) = watch::channel(None);
        let pending = PendingLearning {
            settings,
            prompt: adopted.evidence.prompt,
            task_id: adopted.job.task_id.clone(),
            snapshot: Some(std::sync::Arc::new(snapshot)),
            admitted: true,
            rescanned: true,
            result,
        };
        if !self.spawn_learning_pending(pending, source) {
            return Err(LEARNING_ALREADY_EXECUTING.into());
        }
        self.shared
            .learning_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(source, receiver.clone());
        self.shared
            .learning_sources
            .lock()
            .unwrap()
            .insert(source, adopted.job.task_id.clone());

        Ok(ResumedLearning {
            task_id: adopted.job.task_id,
            completion: receiver,
        })
    }

    /// A core's startup rescan: takes over the pending jobs of its dead predecessors
    /// and resumes each from its stored evidence.
    pub fn rescan_pending_learning(
        &self,
        probe: &dyn crate::learning_jobs::OriginProbe,
    ) -> Result<Vec<ResumedLearning>, String> {
        let queue = self
            .learning_job_queue()
            .ok_or("learning is not configured")?;
        let mut resumed = Vec::new();
        for adopted in queue
            .rescan_core(probe)
            .map_err(|error| error.to_string())?
        {
            let task = adopted.job.task_id.clone();
            let observed = adopted.job.key();
            match self.resume_adopted_learning(adopted) {
                Ok(job) => resumed.push(job),
                Err(error) => {
                    // An attempt that already runs (the scheduler won the race) or a job that
                    // is no longer the Pending one this rescan observed is never ended here.
                    // Only an unusable Pending job is ended, and only at the observed revision.
                    if error == LEARNING_ALREADY_EXECUTING {
                        continue;
                    }
                    tracing::warn!(%error, %task, "adopted learning job cannot run; ending it");
                    let _ = queue.abandon_observed(&observed);
                }
            }
        }
        Ok(resumed)
    }
}

impl PendingLearning {
    pub(crate) fn complete(
        self,
        weak: Weak<Shared>,
        run: RunId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<std::sync::Arc<LearningExecution>>> + Send>,
    > {
        let entry = weak.upgrade().and_then(|shared| {
            shared
                .learning_executions
                .register(&self.task_id, shared.learning_jobs_changed.clone())
        });
        if let Some(entry) = entry {
            return self.complete_registered(weak, run, entry);
        }
        Box::pin(async move {
            self.result.send_replace(Some(Ok(())));
            None
        })
    }

    fn complete_registered(
        self,
        weak: Weak<Shared>,
        run: RunId,
        entry: std::sync::Arc<LearningExecution>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<std::sync::Arc<LearningExecution>>> + Send>,
    > {
        Box::pin(async move {
            let result = self.execute(weak.clone(), run, &entry).await;
            if result.is_err() {
                tracing::warn!(%run, "post-run learning failed; completed run result retained");
                if let Some(shared) = weak.upgrade() {
                    shared.bus.emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                        source: "learning".into(),
                        severity: event_bus::DiagnosticSeverity::Warning,
                        code: diagnostic_codes::LEARNING_PIPELINE_FAILED.into(),
                        // The cause stays in the detail so a candidate is diagnosable without logs.
                        detail: format!(
                            "Lesson extraction or review failed; unapproved candidates remain unpromoted. Inspect the learning runs of this source run for details.\ncause: {}",
                            "pipeline_failed"
                        ),
                        run_id: Some(run.to_string()),
                        thread_id: None,
                        call_id: None,
                    }));
                }
            }
            self.result.send_replace(Some(result.map(|_| ())));
            if let Some(shared) = weak.upgrade() {
                shared
                    .learning_jobs_changed
                    .send_modify(|n| *n = n.wrapping_add(1));
            }
            Some(entry)
        })
    }

    async fn execute(
        &self,
        weak: Weak<Shared>,
        run: RunId,
        entry: &Arc<LearningExecution>,
    ) -> Result<LearningExecutionOutcome, String> {
        let Some(runtime) = AgentRuntime::from_weak(&weak) else {
            return Ok(LearningExecutionOutcome::Skipped);
        };
        runtime.ensure_learning_scheduler();
        // Claiming the source is terminal bookkeeping, even while paused.
        // LearningQueue gates only new extractor/reviewer provider starts.
        // A restored/fenced host must not become another learner of this source.
        // A job taken over from a dead origin has no run of ours to validate.
        if !self.rescanned && !runtime.owns_background_run(run) {
            return Ok(LearningExecutionOutcome::Skipped);
        }
        let queue = runtime
            .learning_job_queue()
            .ok_or("learning is not configured")?;
        let mut snapshot = self.snapshot.clone();
        if !self.admitted {
            let policy = runtime
                .shared
                .self_improvement
                .get()
                .map(|settings| &settings.policy);
            if policy.is_some_and(|policy| !policy.collect_lessons) {
                return Ok(LearningExecutionOutcome::Skipped);
            }
            if runtime
                .inspect_agent(run)
                .map_err(|error| error.to_string())?
                .phase
                != crate::AgentRunPhase::Done
            {
                return Ok(LearningExecutionOutcome::Skipped);
            }
            // Escalated runs can be Done without publishing a completed task result.
            if runtime
                .run_result(run)
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Ok(LearningExecutionOutcome::Skipped);
            }
            // The evidence is frozen with the admission, so a job that runs later
            // (or after a restart) reads exactly what was admitted.
            let (writer, runner, entry) = (queue.clone(), runtime.clone(), entry.clone());
            let (task_id, project, prompt, daily_limit) = (
                self.task_id.clone(),
                self.settings.project.clone(),
                self.prompt.clone(),
                policy.map_or(20, |p| p.daily_limit),
            );
            let admitted = tokio::task::spawn_blocking(move || -> Result<_, String> {
                let frozen = runner.freeze_run_learning(run)?;
                let admission = writer
                    .admit(AdmitRequest {
                        task_id,
                        roots: vec![run.to_string()],
                        goal: false,
                        daily_limit,
                        project,
                        source_run_id: run.to_string(),
                        prompt,
                        evidence_json: frozen.to_json()?,
                    })
                    .map_err(|_| {
                        entry.intake_unconfirmed();
                        "learning intake unconfirmed".to_string()
                    })?;
                if admission != LearningAdmission::Admitted {
                    return Ok(None);
                }
                Ok(Some(frozen))
            })
            .await
            .map_err(|_| "learning intake task failed")??;
            let Some(frozen) = admitted else {
                return Ok(LearningExecutionOutcome::Skipped);
            };
            snapshot = Some(frozen);
        }
        // No root dispatch lease waits for a paused learning gate.
        if !runtime.producer_gate().is_open() {
            return Ok(LearningExecutionOutcome::Deferred);
        }
        let Some(permit) = runtime.learning_gate().try_enter() else {
            return Ok(LearningExecutionOutcome::Deferred);
        };
        let permit = Arc::new(permit);
        if !entry.start_async(&queue, &permit).await? {
            return Ok(LearningExecutionOutcome::Deferred);
        }
        let result = self
            .run_pipeline(runtime, &weak, run, snapshot, entry, &queue)
            .await;
        if entry.interrupted() {
            return Ok(LearningExecutionOutcome::Interrupted);
        }
        if result.is_err() {
            entry
                .change_async(&queue, Change::Fail(LearningEndReason::PipelineFailed))
                .await?;
        }
        drop(permit);
        result.map(|()| LearningExecutionOutcome::Completed)
    }

    async fn run_pipeline(
        &self,
        runtime: AgentRuntime,
        weak: &Weak<Shared>,
        run: RunId,
        snapshot: Option<std::sync::Arc<crate::learning::SourceSnapshot>>,
        entry: &Arc<LearningExecution>,
        jobs: &crate::learning_jobs::LearningJobQueue,
    ) -> Result<(), String> {
        let queue = LearningQueue::new(
            runtime,
            (self.settings.writer.clone(), self.settings.storage.clone()),
            self.settings.quick.clone(),
        );
        let lessons = queue
            .complete_source(
                &QueuedTask {
                    id: &self.task_id,
                    project: &self.settings.project,
                    prompt: &self.prompt,
                    config: RunConfig::default(),
                },
                run,
                snapshot,
                entry,
                jobs,
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
                        .ingest_lessons(&lessons, Some(&run.to_string()));
                }
                Err(error) => {
                    tracing::warn!(%error, "improvement lesson intake skipped; learning result retained")
                }
            }
        }
        Ok(())
    }
}
