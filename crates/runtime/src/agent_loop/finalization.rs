//! No public terminal state until the old incarnation can no longer affect its successor.
use super::LoopState;
use event_bus::LifecycleEvent;

impl LoopState {
    pub(super) async fn finalize(&mut self) {
        let run_id = self.task.run_id.to_string();
        // Keep frozen metadata available to the terminal snapshot after session cleanup.
        if let Some(lease) = &self.mcp_lease {
            lease
                .shutdown(tools::mcp::transport::ShutdownReason::Completed)
                .await;
        }
        self.mcp_cancellation.take();
        // Cancelled tool futures may leave a supervisor reaping its child.
        // Every terminal path waits before shell or workspace teardown.
        self.shared.executor.drain_post_edit_hooks().await;
        let drained = match self.shared.executor.drain_shell_jobs(&run_id).await {
            Ok(()) => true,
            Err(error) => {
                self.finalization_error(format!(
                    "shell job cleanup failed; workspace retained: {error}"
                ));
                self.snapshot_diagnostic(&error);
                false
            }
        };
        let drained = if let Some(detail) = self.run_js_unconfirmed_detail() {
            self.finalization_error(format!(
                "run_js could not confirm that what it started has stopped ({detail}); workspace retained"
            ));
            false
        } else {
            drained
        };
        // No host input enters this incarnation from here: wait out a send in
        // progress, then take what never reached the context.
        crate::runtime::user_inbox::seal_off_thread(std::sync::Arc::clone(
            &self.channels.user_inbox,
        ))
        .await;
        let dropped = self.drain_undelivered_inbox();
        let uncertain = self.shared.executor.has_unobserved_shell_jobs(&run_id);
        // ADR 0027 §5: drain -> persist uncertainty/terminal history -> release
        // handles -> publish terminal.
        let (_snapshot, saved) = {
            match crate::restore::persist_terminal_snapshot(self, &dropped) {
                Ok(snapshot) => {
                    let saved = matches!(
                        snapshot,
                        crate::checkpoint::SnapshotReference::RunContext { .. }
                    );
                    if saved {
                        self.snapshot_saved_diagnostic();
                    } else {
                        self.settle_unsaved_host_inputs(&dropped, "no_terminal_snapshot");
                    }
                    (snapshot, saved)
                }
                Err(error) => {
                    self.snapshot_diagnostic(&error);
                    // Nothing of the failed transaction was notified; settle
                    // separately, or leave the receipts for reconciliation.
                    self.settle_unsaved_host_inputs(&dropped, "terminal_snapshot_failed");
                    if uncertain
                        || matches!(error, crate::restore::SnapshotError::AssistantReference(_))
                    {
                        self.finalization_error(format!(
                            "terminal context snapshot failed: {error}"
                        ));
                    }
                    (
                        crate::checkpoint::SnapshotReference::none(
                            crate::checkpoint::SnapshotNoneReason::WriteFailed,
                        ),
                        false,
                    )
                }
            }
        };
        if uncertain && !saved {
            self.finalization_error(
                "unobserved shell effects require a durable terminal snapshot; resources retained"
                    .into(),
            );
        }
        // Never clear active authority when shell effects cannot be recorded.
        if self.run_state.phase() == event_bus::AgentRunPhase::Error
            && let Some(runtime) = self.runtime()
            && let Err(error) = runtime.record_lane_harness_failure(
                self.task.run_id,
                &self.lane_failure_episode,
                "runtime_failed",
            )
        {
            runtime.lane_delivery_diagnostic(
                Some(self.task.run_id),
                "lane_failure_report_failed",
                &error.to_string(),
            );
        }
        // Without unknown shell effects, snapshot failure remains a non-fatal
        // diagnostic and does not suppress ordinary completion or workspace cleanup.
        let released = if drained && (saved || !uncertain) {
            // Releasing handles erases their snapshots. Consume confirmed
            // teardown first, even if the asynchronous monitor has not run yet.
            self.sync_scratch_shell_users();
            match self.shared.executor.release_shell_jobs(&run_id) {
                Ok(()) => true,
                Err(error) => {
                    self.finalization_error(error.to_string());
                    self.snapshot_diagnostic(&error);
                    false
                }
            }
        } else {
            false
        };
        if released {
            self.release_scratch_usage();
        }
        if !self.benchmark_replay
            && let Some(checkpoint) = &self.benchmark
            && let Some(runtime) = self.shared.runtime.upgrade()
            && let Some(recording) = runtime.benchmark_recording.get()
        {
            let result = self.channels.result_tx.borrow().clone();
            if let Err(error) = recording
                .recorder
                .completed(checkpoint, self.phase(), result.as_deref())
                .await
            {
                self.finalization_error(format!("benchmark completion recording failed: {error}"));
            }
        }
        if let Some(runtime) = self.runtime()
            && runtime.shared.quiesce.strict_restart_accounting()
            && (!drained || !saved || !released)
        {
            // Ordinary completion keeps ADR 0027's non-fatal snapshot policy,
            // but a restart may not turn failed teardown/persistence into success.
            runtime
                .shared
                .quiesce
                .fail("terminal finalization did not commit a safe checkpoint");
        }
        if released && let Some(memo) = self.take_pending_escalation() {
            // Handoff publishes its own terminal event and returns early. Retire
            // this incarnation before that publication, just like ordinary exit.
            self.record_intent_terminal();
            let shared = self.shared.runtime.clone();
            crate::escalation::handoff::complete(&shared, self, memo).await;
            return;
        }
        // The work period is over, whatever ended it. After a completed
        // escalation the source root no longer owns a list, so this is a no-op.
        // It must precede the terminal event and never changes how the run ended.
        self.reconcile_root_plan();
        self.record_intent_terminal();
        self.publish_terminal();
    }

    /// Settle host inputs no terminal snapshot records: `dropped` were never
    /// read, and unrecorded appends may have been. A failure leaves them
    /// accepted, which the host shows as of unknown delivery.
    fn settle_unsaved_host_inputs(&self, dropped: &[String], reason: &str) {
        let transitions = self.terminal_host_inputs(dropped, None, reason);
        if let Some(runtime) = self.runtime() {
            runtime.settle_host_inputs(self.task.run_id, &transitions);
        }
    }

    fn finalization_error(&mut self, reason: String) {
        if let Some((previous, _)) = self.pending_terminal.take() {
            self.run_state = previous;
        }
        self.channels.result_tx.send_replace(None);
        self.pending_escalation = None;
        self.finish_error(reason);
    }

    pub(crate) fn publish_terminal(&mut self) {
        let Some((_, event)) = self.pending_terminal.take() else {
            return;
        };
        let LifecycleEvent::AgentRunStateChanged { to, reason, .. } = &event else {
            return;
        };
        let phase = *to;
        self.publish_durable_task(phase, reason.clone());
        if let Some(runtime) = self.runtime() {
            self.terminal_dispatch = runtime.publish_terminal(self.task.run_id, event);
        }
    }
}

#[cfg(test)]
mod scratch_tests {
    use crate::scratch::usage::tests::{Harness, has_shell_users};
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn shell_teardown_is_consumed_before_finalization_erases_job_handles() {
        let mut h = Harness::new().await;
        h.terminal(false);
        h.state.sync_scratch_shell_users();
        assert!(h.has_shell_users());
        h.terminal(true);
        *h.shell.release_probe.lock().unwrap() = Some(Box::new({
            let scratch = h.scratch.clone();
            let key = h.key.clone();
            move || {
                assert!(
                    !has_shell_users(&scratch, &key),
                    "teardown must be consumed before the job snapshot disappears"
                );
            }
        }));
        h.state.finalize().await;
        assert_eq!(h.shell.released.load(Ordering::SeqCst), 1);
        assert!(!h.has_users());
    }
}
