//! No public terminal state until the old incarnation can no longer affect its successor.
use super::LoopState;
use event_bus::LifecycleEvent;

impl LoopState {
    pub(super) async fn finalize(&mut self) {
        let run_id = self.task.run_id.to_string();
        // Cancelled tool futures may leave a supervisor reaping its child.
        // Every terminal path waits before shell, ownership or workspace teardown.
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
        // No host input enters this incarnation from here: wait out a send in
        // progress, then take what never reached the context.
        crate::runtime::user_inbox::seal_off_thread(std::sync::Arc::clone(
            &self.channels.user_inbox,
        ))
        .await;
        let dropped = self.drain_undelivered_inbox();
        let authority = if let Some(permit) = self.task.config.ownership.clone() {
            // Teardown must drain even after Stop/Cancel. Contention is waited
            // out (it never revokes authority); a real fence fails closed.
            match self
                .await_ownership(
                    permit.validate_generation_off_thread(self.ownership_retry(), || false),
                )
                .await
            {
                Ok(()) => true,
                Err(error) => {
                    self.finalization_error(error.to_string());
                    false
                }
            }
        } else {
            true
        };
        let uncertain = self.shared.executor.has_unobserved_shell_jobs(&run_id);
        // ADR 0027 §5: drain -> persist uncertainty/terminal history -> release
        // handles -> publish terminal. Ownership references commit after history.
        let (snapshot, saved) = if authority {
            match crate::restore::persist_terminal_snapshot(self, &dropped) {
                Ok(snapshot) => {
                    let saved = matches!(
                        snapshot,
                        crate::ownership::SnapshotReference::RunContext { .. }
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
                        crate::ownership::SnapshotReference::none(
                            crate::ownership::SnapshotNoneReason::WriteFailed,
                        ),
                        false,
                    )
                }
            }
        } else {
            self.settle_unsaved_host_inputs(&dropped, "ownership_not_held");
            (
                crate::ownership::SnapshotReference::none(
                    crate::ownership::SnapshotNoneReason::Skipped,
                ),
                false,
            )
        };
        if uncertain && !saved {
            self.finalization_error(
                "unobserved shell effects require a durable terminal snapshot; ownership retained"
                    .into(),
            );
        }
        // Never clear active authority when shell effects cannot be recorded.
        if drained
            && authority
            && (saved || !uncertain)
            && let Some(permit) = self.task.config.ownership.clone()
            && let Err(error) = self
                .await_ownership(permit.checkpoint_off_thread(
                    crate::ownership::CheckpointBoundary::Terminal,
                    snapshot,
                    self.ownership_retry(),
                    || false,
                ))
                .await
        {
            // Teardown ignores interrupts and waits out contention like the run.
            if let crate::ownership::RegistryError::Database {
                operation, phase, ..
            } = &error
            {
                tracing::warn!(
                    target: crate::ownership::DB_TRACE_TARGET,
                    run_id = %run_id,
                    stage = "finalization",
                    operation = operation.as_str(),
                    phase = phase.as_str(),
                    sqlite_code = ?error.sqlite_code(),
                    "ownership checkpoint failed during finalization"
                );
            }
            self.finalization_error(error.to_string());
            // The initial terminal phase was provisional. No reference committed;
            // best-effort correction preserves Error for recovery (Q3).
            if let Err(error) = crate::restore::persist_terminal_snapshot(self, &dropped) {
                self.snapshot_diagnostic(&error);
            }
        }
        // Without unknown shell effects, snapshot failure remains a non-fatal
        // diagnostic and does not suppress ordinary completion or workspace cleanup.
        let released = if drained && authority && (saved || !uncertain) {
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
        if released && let Some(memo) = self.take_pending_escalation() {
            let shared = self.shared.runtime.clone();
            crate::escalation::handoff::complete(&shared, self, memo).await;
        }
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
            runtime.publish_terminal(self.task.run_id, event);
        }
    }
}
