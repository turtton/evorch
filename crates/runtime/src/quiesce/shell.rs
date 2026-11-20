//! Process-local shell controls. No wire entry point or restart policy change.
use super::*;
use crate::{AgentRuntime, RuntimeError};
use std::sync::{Arc, Weak};

pub(crate) struct RestartShellExecutorRegistration {
    runtime: Weak<crate::runtime::Shared>,
    run: RunId,
    executor: Weak<tools::ToolExecutor>,
}
impl Drop for RestartShellExecutorRegistration {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.upgrade() else {
            return;
        };
        let _gate = runtime
            .quiesce
            .checkpoint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut entries = runtime
            .restart_shell_executors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries
            .get(&self.run)
            .is_some_and(|current| current.ptr_eq(&self.executor))
        {
            entries.remove(&self.run);
        }
    }
}

/// Opaque, non-serializable job selection from one restart and incarnation.
#[derive(Clone)]
pub struct RestartShellJob {
    generation: u64,
    restart_id: String,
    instance_epoch: String,
    run_id: RunId,
    incarnation: String,
    snapshot: tools::ShellJobSnapshot,
}
impl RestartShellJob {
    pub fn snapshot(&self) -> &tools::ShellJobSnapshot {
        &self.snapshot
    }
    pub fn run_id(&self) -> RunId {
        self.run_id
    }
    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }
}

impl AgentRuntime {
    pub(crate) fn register_restart_shell_executor(
        &self,
        run: RunId,
        executor: &Arc<tools::ToolExecutor>,
    ) -> RestartShellExecutorRegistration {
        let _gate = self
            .shared
            .quiesce
            .checkpoint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let executor = Arc::downgrade(executor);
        self.shared
            .restart_shell_executors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run, executor.clone());
        RestartShellExecutorRegistration {
            runtime: Arc::downgrade(&self.shared),
            run,
            executor,
        }
    }
    // Caller holds checkpoint_gate through selection and observation/signal.
    // An expired registered run must NOT fall back to a process-global Shell.
    fn restart_shell_executor(&self, run: RunId) -> Option<Arc<tools::ToolExecutor>> {
        let entries = self
            .shared
            .restart_shell_executors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(executor) = entries.get(&run) {
            return executor.upgrade();
        }
        drop(entries);
        Some(Arc::clone(
            &self
                .shared
                .executor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ))
    }
    /// Bind verified host request identities to this attempt, once. Merely listing
    /// jobs uses Wait semantics: it sends no signal and acknowledges no result.
    /// 4b supplies the durable restart ID and current instance epoch.
    pub fn restart_shell_jobs(
        &self,
        attempt: &RestartAttempt,
        restart_id: &str,
        instance_epoch: &str,
    ) -> Result<Vec<RestartShellJob>, RuntimeError> {
        let _gate = self
            .shared
            .quiesce
            .checkpoint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.validate_restart_attempt(attempt)?;
        if restart_id.is_empty() || instance_epoch.is_empty() {
            return Err(runtime::error("missing restart shell identity"));
        }
        let intents = {
            let mut inner = self.shared.quiesce.lock();
            let scope = (
                attempt.generation(),
                restart_id.to_owned(),
                instance_epoch.to_owned(),
            );
            if inner
                .shell_scope
                .as_ref()
                .is_some_and(|old| old.0 == scope.0 && old != &scope)
            {
                return Err(runtime::error("restart shell scope mismatch"));
            }
            inner.shell_scope = Some(scope);
            inner
                .intents
                .values()
                .filter(|intent| intent.active)
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut jobs = Vec::new();
        for intent in intents {
            let Some(executor) = self.restart_shell_executor(intent.run_id) else {
                continue;
            };
            jobs.extend(
                executor
                    .shell_job_snapshots(&intent.run_id.to_string())
                    .into_iter()
                    .map(|snapshot| RestartShellJob {
                        generation: attempt.generation(),
                        restart_id: restart_id.into(),
                        instance_epoch: instance_epoch.into(),
                        run_id: intent.run_id,
                        incarnation: intent.incarnation.clone(),
                        snapshot,
                    }),
            );
        }
        Ok(jobs)
    }

    /// Explicit Interrupt only. Returns after terminal observation and confirmed
    /// reap; the run's safe point still owns history/snapshot acknowledgement.
    /// Cancellation of this waiter never cancels the restart or replays a command.
    pub async fn interrupt_restart_shell_job(
        &self,
        attempt: &RestartAttempt,
        restart_id: &str,
        instance_epoch: &str,
        job: &RestartShellJob,
    ) -> Result<tools::ShellJobSnapshot, RuntimeError> {
        let executor = {
            let _gate = self
                .shared
                .quiesce
                .checkpoint_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.validate_restart_attempt(attempt)?;
            let executor = self
                .restart_shell_executor(job.run_id)
                .ok_or_else(|| runtime::error("restart shell executor ended"))?;
            let inner = self.shared.quiesce.lock();
            if job.generation != attempt.generation()
                || job.restart_id != restart_id
                || job.instance_epoch != instance_epoch
                || inner.shell_scope.as_ref()
                    != Some(&(job.generation, restart_id.into(), instance_epoch.into()))
                || !inner
                    .intents
                    .get(&job.run_id)
                    .is_some_and(|intent| intent.active && intent.incarnation == job.incarnation)
                || !executor
                    .shell_job_snapshots(&job.run_id.to_string())
                    .iter()
                    .any(|current| current.identity == job.snapshot.identity)
            {
                return Err(runtime::error("stale restart shell job"));
            }
            executor
                .interrupt_shell_job(&job.snapshot.identity)
                .map_err(|error| runtime::error(error.to_string()))?;
            executor
        };
        executor
            .wait_shell_job_terminal(&job.snapshot.identity)
            .await
            .map_err(|error| runtime::error(error.to_string()))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use tools::Tool;

    #[tokio::test]
    async fn real_loop_unregisters_on_completion_without_retaining_executor() {
        let f = super::super::fixture::Fixture::new(0);
        let run = f.start();
        f.model.entered.notified().await;
        assert!(
            f.runtime
                .shared
                .restart_shell_executors
                .lock()
                .unwrap()
                .contains_key(&run)
        );
        f.cleanup(run).await;
        assert!(
            !f.runtime
                .shared
                .restart_shell_executors
                .lock()
                .unwrap()
                .contains_key(&run)
        );
    }

    #[tokio::test]
    async fn stale_registration_cleanup_cannot_remove_a_replacement_executor() {
        let f = super::super::fixture::Fixture::new(0);
        let run = f.start();
        f.model.entered.notified().await;
        let old = Arc::new(tools::ToolExecutor::new(f.runtime.shared.bus.clone()));
        let replacement = Arc::new(tools::ToolExecutor::new(f.runtime.shared.bus.clone()));
        let first = f.runtime.register_restart_shell_executor(run, &old);
        let second = f.runtime.register_restart_shell_executor(run, &replacement);
        drop(first);
        assert!(Arc::ptr_eq(
            &f.runtime.restart_shell_executor(run).unwrap(),
            &replacement
        ));
        drop(second);
        assert!(
            !f.runtime
                .shared
                .restart_shell_executors
                .lock()
                .unwrap()
                .contains_key(&run)
        );
        f.cleanup(run).await;
    }

    #[tokio::test]
    async fn ending_executor_and_blocker_query_do_not_retain_or_return_an_expired_weak() {
        let f = super::super::fixture::Fixture::new(0);
        let shell = Arc::new(tools::Shell::new(Arc::new(
            sandbox::DirectSandbox::new_unchecked(),
        )));
        let mut base = tools::ToolExecutor::new(f.runtime.shared.bus.clone());
        base.register(shell.clone()).unwrap();
        *f.runtime.shared.executor.lock().unwrap() = Arc::new(base);
        let attempt = f.runtime.begin_restart_quiesce().unwrap();
        let run = f.start();
        f.runtime.wait_quiesced().await.unwrap();
        shell
            .execute_with_context(
                &tools::ToolExecutionContext {
                    run_id: run.to_string(),
                    ..Default::default()
                },
                serde_json::json!({"command":"read value","yield_ms":0}),
            )
            .await
            .unwrap();
        let executor = Arc::new(tools::ToolExecutor::new(f.runtime.shared.bus.clone()));
        let weak = Arc::downgrade(&executor);
        let registration = f.runtime.register_restart_shell_executor(run, &executor);
        assert_eq!(Arc::strong_count(&executor), 1, "registration must be weak");
        let (ended, ending) = std::sync::mpsc::channel();
        let (release, cleanup) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            drop(executor);
            ended.send(()).unwrap();
            // Force observation into the resource-drop / registration-drop gap.
            cleanup.recv().unwrap();
            drop(registration);
        });
        ending.recv().unwrap();
        assert!(weak.upgrade().is_none());
        let jobs = f
            .runtime
            .restart_shell_jobs(&attempt, "restart", "epoch")
            .unwrap();
        release.send(()).unwrap();
        owner.join().unwrap();
        let unregistered = !f
            .runtime
            .shared
            .restart_shell_executors
            .lock()
            .unwrap()
            .contains_key(&run);
        let base_job = shell.shell_job_snapshots(&run.to_string()).pop().unwrap();
        shell.interrupt_shell_job(&base_job.identity).unwrap();
        shell
            .wait_shell_job_terminal(&base_job.identity)
            .await
            .unwrap();
        f.cleanup(run).await;
        assert!(
            jobs.is_empty(),
            "expired run must not fall back to a base Shell"
        );
        assert!(unregistered);
    }

    #[tokio::test]
    async fn restart_interrupt_rejects_changed_incarnation_and_job_without_signalling() {
        let f = super::super::fixture::Fixture::new(0);
        let shell = Arc::new(tools::Shell::new(Arc::new(
            sandbox::DirectSandbox::new_unchecked(),
        )));
        let mut executor = tools::ToolExecutor::new(f.runtime.shared.bus.clone());
        executor.register(shell.clone()).unwrap();
        *f.runtime.shared.executor.lock().unwrap() = Arc::new(executor);
        // The identity checks are independent of checkpoint collection. Start
        // the synthetic job only after parking, so no provider/tool dispatch
        // can race this deliberately changed incarnation.
        let attempt = f.runtime.begin_restart_quiesce().unwrap();
        let run = f.start();
        f.runtime.wait_quiesced().await.unwrap();
        let ctx = tools::ToolExecutionContext {
            run_id: run.to_string(),
            call_id: Some("original-call".into()),
            ..Default::default()
        };
        shell
            .execute_with_context(
                &ctx,
                serde_json::json!({"command":"read value", "yield_ms":0}),
            )
            .await
            .unwrap();
        let selected = f
            .runtime
            .restart_shell_jobs(&attempt, "restart", "epoch")
            .unwrap()
            .pop()
            .unwrap();
        let mut stale = selected.clone();
        stale.incarnation.push_str("-old");
        assert!(
            f.runtime
                .interrupt_restart_shell_job(&attempt, "restart", "epoch", &stale)
                .await
                .is_err()
        );
        stale = selected.clone();
        // Swap a real job identity belonging to a different run. An operation
        // may never fall back to a human-readable/reused shell handle.
        let other = tools::ToolExecutionContext {
            run_id: "other-run".into(),
            ..ctx.clone()
        };
        shell
            .execute_with_context(
                &other,
                serde_json::json!({"command":"read value", "yield_ms":0}),
            )
            .await
            .unwrap();
        stale.snapshot = shell.shell_job_snapshots(&other.run_id).pop().unwrap();
        assert!(
            f.runtime
                .interrupt_restart_shell_job(&attempt, "restart", "epoch", &stale)
                .await
                .is_err()
        );
        assert!(shell.has_running_shell_jobs(&ctx.run_id));
        assert!(shell.has_running_shell_jobs(&other.run_id));
        f.runtime
            .interrupt_restart_shell_job(&attempt, "restart", "epoch", &selected)
            .await
            .unwrap();
        shell.interrupt_shell_job(&stale.snapshot.identity).unwrap();
        shell
            .wait_shell_job_terminal(&stale.snapshot.identity)
            .await
            .unwrap();
        f.cleanup(run).await;
    }
}
