use super::*;
use crate::agent_loop::LoopState;
use crate::checkpoint::{CheckpointBoundary, RunCheckpoint, SnapshotReference};

impl LoopState {
    /// No hook exists inside a parallel wave: only the complete batch and all of
    /// its results can reach this boundary. Stop/Cancel remains a separate path.
    pub(crate) async fn quiesce_safe_point(&mut self, boundary: CheckpointBoundary) -> bool {
        let Some(runtime) = self.runtime() else {
            return true;
        };
        if runtime.shared.quiesce.running() {
            return true;
        }
        let mut changes = runtime.shared.quiesce.subscribe();
        let parked_generation = runtime.shared.quiesce.lock().generation;
        loop {
            if let Some(kind) = self.interrupted() {
                self.finish_interrupted(kind);
                return false;
            }
            if runtime.shared.quiesce.resume_permitted(parked_generation) {
                return true;
            }
            {
                let _checkpoint = runtime
                    .shared
                    .quiesce
                    .checkpoint_gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let generation = {
                    let inner = runtime.shared.quiesce.lock();
                    (inner.requested
                        && !inner.cancelling
                        && !inner.shutting_down
                        && !matches!(inner.phase, QuiesceState::Failed { .. }))
                    .then_some(inner.generation)
                };
                if let Some(generation) = generation
                    && !runtime
                        .shared
                        .quiesce
                        .acknowledged(self.task.run_id, &self.task.publication.incarnation())
                {
                    match self.capture_safe_point(boundary) {
                        Ok(()) => runtime.shared.quiesce.acknowledge(
                            self.task.run_id,
                            &self.task.publication.incarnation(),
                            generation,
                        ),
                        Err(error) => runtime.shared.quiesce.fail(error),
                    }
                }
            }
            tokio::select! {
                biased;
                changed = self.channels.cancel_rx.changed() => { if changed.is_err() { return false; } }
                changed = changes.changed() => { if changed.is_err() { return false; } }
            }
        }
    }

    fn capture_safe_point(&self, boundary: CheckpointBoundary) -> Result<(), String> {
        let runtime = self.runtime().ok_or("runtime disappeared")?;
        if !runtime.shared.quiesce.active(self.task.run_id) {
            return Err("active run has no admitted durable intent".into());
        }
        if runtime.shared.run_store.get().is_none() {
            return Err("safe point requires a run store".into());
        }
        if self
            .shared
            .executor
            .has_unobserved_shell_jobs(&self.task.run_id.to_string())
            || self.run_js_unconfirmed_detail().is_some()
        {
            return Err("unconfirmed external tool/job effects; cannot quiesce".into());
        }
        if self.provider_request_started && self.last_provider_input.is_none() {
            return Err("model did not expose its exact provider input".into());
        }
        runtime.shared.quiesce.storage_step(StorageStep::Snapshot)?;
        let snapshot = if boundary == CheckpointBoundary::ToolsWaiting {
            crate::restore::persist_tools_waiting(self)
        } else {
            crate::restore::persist_checkpoint(self)
        }
        .map_err(|e| e.to_string())?;
        if !matches!(snapshot, SnapshotReference::RunContext { .. }) {
            return Err("safe point did not commit a checkpoint".into());
        }
        let checkpointed_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis()
            .try_into()
            .map_err(|_| "checkpoint timestamp overflow")?;
        runtime.shared.quiesce.update(
            self.task.run_id,
            &self.task.publication.incarnation(),
            runtime.shared.run_store.get(),
            |intent| {
                intent.initial_input = None;
                intent.wait = ResumeWait::Activity {
                    activity: self.current_activity(),
                };
                intent.checkpoint = Some(RunCheckpoint {
                    snapshot,
                    boundary,
                    checkpointed_at_ms,
                });
                intent.provider_input = self.last_provider_input.clone();
                intent.boundary_pending = self.boundary_pending;
                intent.responses_replay = self.context.responses_replay.clone();
                intent.compactions = self.context.checkpoints().to_vec();
                intent.route_pins = self.shared.model.route_pins(&self.task.run_id.to_string());
            },
        )
    }

    pub(crate) fn record_intent_terminal(&self) {
        let Some(runtime) = self.runtime() else {
            return;
        };
        let reason = match self.interrupted() {
            Some(crate::InterruptKind::Stop) => StopReason::OperatorStopped,
            Some(crate::InterruptKind::Cancel) => StopReason::Cancelled,
            None => StopReason::Terminal,
        };
        runtime.end_intent(
            self.task.run_id,
            &self.task.publication.incarnation(),
            reason,
        );
    }
}
