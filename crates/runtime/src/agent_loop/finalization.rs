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
        let authority = if let Some(permit) = &self.task.config.ownership {
            match permit.validate_generation() {
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
            match crate::restore::persist_terminal_snapshot(self) {
                Ok(snapshot) => {
                    let saved = matches!(
                        snapshot,
                        crate::ownership::SnapshotReference::RunContext { .. }
                    );
                    if saved {
                        self.snapshot_saved_diagnostic();
                    }
                    (snapshot, saved)
                }
                Err(error) => {
                    self.snapshot_diagnostic(&error);
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
            && let Err(error) = permit
                .checkpoint_off_thread(
                    crate::ownership::CheckpointBoundary::Terminal,
                    snapshot,
                    self.ownership_retry(),
                    || false,
                )
                .await
        {
            // Teardown ignores interrupts, but retains the same finite retry budget.
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
            if let Err(error) = crate::restore::persist_terminal_snapshot(self) {
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
