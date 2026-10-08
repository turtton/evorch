//! A panicking run task must not leave its run Running forever.
use super::*;
use crate::panic_capture::CaughtPanic;
use event_bus::event::{DIAGNOSTIC_SITE_PREFIX, diagnostic_codes};
use event_bus::{DiagnosticEvent, DiagnosticSeverity};

impl AgentRuntime {
    /// The loop unwound without `finalize`: stop the run's shell processes, keep its
    /// workspace (as a failed finalization does), report the panic, then publish Error.
    pub(super) async fn run_panicked(&self, run_id: RunId, panic: CaughtPanic) {
        let id = run_id.to_string();
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
        let shells = match executor.drain_shell_jobs(&id).await {
            Ok(()) => "stopped".to_string(),
            Err(error) => format!("cleanup failed: {error}"),
        };
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
            let first_line = panic.message.lines().next().unwrap_or_default();
            self.publish_terminal(
                run_id,
                LifecycleEvent::AgentRunStateChanged {
                    run_id: id,
                    from,
                    to: AgentRunPhase::Error,
                    reason: Some(format!("panicked: {first_line}")),
                },
            );
        }
    }
}
