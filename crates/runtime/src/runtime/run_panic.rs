//! A panicking run task must not leave its run Running forever.
use super::*;
use crate::panic_capture::CaughtPanic;
use event_bus::event::{DIAGNOSTIC_SITE_PREFIX, diagnostic_codes};
use event_bus::{DiagnosticEvent, DiagnosticSeverity};

impl AgentRuntime {
    /// The loop unwound without `finalize`: stop the run's shell processes, keep its
    /// workspace (as a failed finalization does), report the panic, then publish Error.
    pub(super) async fn run_panicked(
        &self,
        run_id: RunId,
        panic: CaughtPanic,
    ) -> Option<crate::quiesce::dispatch::DispatchGuard> {
        let id = run_id.to_string();
        // Panic is never a safe-point checkpoint, even after successful cleanup.
        if !self.shared.quiesce.running() {
            self.shared
                .quiesce
                .fail(format!("run {run_id} panicked before a safe point"));
        }
        let executor = self
            .shared
            .benchmark_executors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&run_id)
            .unwrap_or_else(|| {
                Arc::clone(
                    &self
                        .shared
                        .executor
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                )
            });
        executor.cancel_shell_jobs(&id);
        executor.end_run(&id);
        self.close_artifact_page(&id);
        let shells = match executor.drain_shell_jobs(&id).await {
            Ok(()) => "stopped".to_string(),
            Err(error) => {
                self.shared
                    .quiesce
                    .fail(format!("shell cleanup failed: {error}"));
                format!("cleanup failed: {error}")
            }
        };
        // Keep the intent active until actual shell cleanup, never just until
        // a panic/timeout has ended the Rust future.
        if let Ok(execution) = self.execution_snapshot(run_id) {
            self.end_intent(
                run_id,
                &execution.publication().incarnation(),
                crate::quiesce::StopReason::Terminal,
            );
        }
        // Close host input before Error is public; the loop's state is gone,
        // so anything it accepted may or may not have reached the model.
        let inbox = lock_runs(&self.shared.runs)
            .get(&run_id)
            .map(|entry| Arc::clone(&entry.user_inbox));
        if let Some(inbox) = inbox {
            user_inbox::seal_off_thread(inbox).await;
        }
        if let Err(error) = self.mark_unreachable_host_inputs(&id, "run_panicked") {
            self.host_input_diagnostic(run_id, &error.to_string());
        }
        let mut detail = panic.message.clone();
        if let Some(location) = &panic.location {
            detail.push_str(&format!("\n{DIAGNOSTIC_SITE_PREFIX}{location}"));
        }
        detail.push_str(&format!("\nshell jobs: {shells}; workspace retained"));
        if !panic.backtrace.is_empty() {
            detail.push_str(&format!("\nbacktrace:\n{}", panic.backtrace));
        }
        self.shared.bus.emit(Event::new(DiagnosticEvent {
            source: "agent_runtime".into(),
            severity: DiagnosticSeverity::Error,
            code: diagnostic_codes::AGENT_RUN_PANICKED.into(),
            detail,
            run_id: Some(id.clone()),
            thread_id: None,
            call_id: None,
        }));
        let from = lock_runs(&self.shared.runs)
            .get(&run_id)
            .map(|entry| *entry.phase_tx.borrow());
        // A panic after the terminal transition is reported but does not reopen the run.
        if let Some(from) = from
            && matches!(
                from,
                AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
            )
        {
            // The run's loop state is gone: authority comes from its configuration,
            // read outside the runs lock (which the completion relay holds while
            // taking others) and validated against the registry before any write.
            let ownership = lock_runs(&self.shared.runs)
                .get(&run_id)
                .and_then(|entry| entry.config.ownership.clone());
            self.reconcile_todo_or_report(run_id, ownership.as_ref());
            let first_line = panic.message.lines().next().unwrap_or_default();
            if let Ok(mailbox) = self.run_mailbox(run_id) {
                self.close_question_mailbox(run_id, &mailbox);
            }
            return self.publish_terminal(
                run_id,
                LifecycleEvent::AgentRunStateChanged {
                    run_id: id,
                    from,
                    to: AgentRunPhase::Error,
                    reason: Some(format!("panicked: {first_line}")),
                },
            );
        }
        None
    }
}
