//! No public terminal state until the old incarnation can no longer affect its successor.
use super::{LoopState, cleanup_worktree};
use crate::workspace::OwnedWorktree;
use event_bus::LifecycleEvent;

impl LoopState {
    pub(super) async fn finalize(&mut self, mut owned: Option<OwnedWorktree>) {
        let run_id = self.task.run_id.to_string();
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
        if drained
            && let Some(permit) = &self.task.config.ownership
            && let Err(error) = permit.checkpoint(&self.context.visible_messages())
        {
            self.finalization_error(error.to_string());
        }
        let uncertain = self.shared.executor.has_unobserved_shell_jobs(&run_id);
        let saved = match crate::restore::persist_terminal_snapshot(self) {
            Ok(()) => {
                self.snapshot_saved_diagnostic();
                true
            }
            Err(error) => {
                self.snapshot_diagnostic(&error);
                if uncertain {
                    self.finalization_error(format!("terminal context snapshot failed: {error}"));
                }
                false
            }
        };
        // Without unknown shell effects, snapshot failure remains a non-fatal
        // diagnostic and does not suppress ordinary completion or workspace cleanup.
        let released = if drained && (saved || !uncertain) {
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
            match self.take_pending_escalation() {
                Some(memo) => {
                    let shared = self.shared.runtime.clone();
                    crate::escalation::handoff::complete(&shared, self, memo, owned.take()).await;
                }
                None => cleanup_worktree(&self.shared, self.task.run_id, owned.take()).await,
            }
        }
        // Failed teardown/persistence retains both handles and workspace for inspection.
        // Dropping OwnedWorktree itself does not delete the worktree.
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
