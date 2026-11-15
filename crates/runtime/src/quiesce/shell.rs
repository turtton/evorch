//! Process-local shell controls. No wire entry point or restart policy change.
use super::*;
use crate::{AgentRuntime, RuntimeError};

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
        let executor = self
            .shared
            .executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(intents
            .into_iter()
            .flat_map(|intent| {
                executor
                    .shell_job_snapshots(&intent.run_id.to_string())
                    .into_iter()
                    .map(move |snapshot| RestartShellJob {
                        generation: attempt.generation(),
                        restart_id: restart_id.into(),
                        instance_epoch: instance_epoch.into(),
                        run_id: intent.run_id,
                        incarnation: intent.incarnation.clone(),
                        snapshot,
                    })
            })
            .collect())
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
        let executor = self
            .shared
            .executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        {
            let _gate = self
                .shared
                .quiesce
                .checkpoint_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.validate_restart_attempt(attempt)?;
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
        }
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
