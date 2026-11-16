use super::*;
use crate::agent_loop::LoopState;
use crate::checkpoint::{CheckpointBoundary, RunCheckpoint, SnapshotReference};

impl LoopState {
    /// No hook exists inside a parallel wave: only the complete batch and all of
    /// its results can reach this boundary. Stop/Cancel remains a separate path.
    pub(crate) async fn quiesce_safe_point(&mut self, boundary: CheckpointBoundary) -> bool {
        if !self.has_open_tool_batch() {
            self.shell_result_batch_open = false;
        }
        let Some(runtime) = self.runtime() else {
            return true;
        };
        if runtime.shared.quiesce.running() {
            return true;
        }
        let mut changes = runtime.shared.quiesce.subscribe();
        let mut shell_changes = self.shared.executor.subscribe_shell_job_changes();
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
                    let incarnation = self.task.publication.incarnation();
                    // An open batch must finish through its original ToolResults.
                    // Return to observation without releasing quiesce or dispatch.
                    if self.has_open_tool_batch()
                        && (self.has_unstaged_shell_results() || self.shell_result_batch_open)
                    {
                        let kind = if self
                            .shared
                            .executor
                            .has_running_shell_jobs(&self.task.run_id.to_string())
                        {
                            RestartBlockerKind::ShellRunning
                        } else {
                            RestartBlockerKind::UnfinishedToolBatch
                        };
                        runtime
                            .shared
                            .quiesce
                            .block(self.task.run_id, &incarnation, kind);
                        return true;
                    }
                    if let Err(error) = self.stage_restart_shell_results() {
                        runtime.shared.quiesce.fail(error);
                    } else if self.acknowledge_restart_shell_results().is_err() {
                        // Presentation is irreversible, but a failed tool ACK is
                        // not a storage-health failure. Keep the identity and
                        // retry on a shell/coordinator change without re-inserting
                        // the body. Identical blockers do not notify/spin.
                        runtime.shared.quiesce.block(
                            self.task.run_id,
                            &incarnation,
                            RestartBlockerKind::ShellResultUnobserved,
                        );
                    } else if let Some(kind) = self.restart_safety_blocker() {
                        runtime
                            .shared
                            .quiesce
                            .block(self.task.run_id, &incarnation, kind);
                    } else {
                        runtime
                            .shared
                            .quiesce
                            .clear_blocker(self.task.run_id, &incarnation);
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
            }
            tokio::select! {
                biased;
                changed = self.channels.cancel_rx.changed() => { if changed.is_err() { return false; } }
                changed = changes.changed() => { if changed.is_err() { return false; } }
                changed = async {
                    match shell_changes.as_mut() {
                        Some(changes) => changes.changed().await,
                        None => std::future::pending().await,
                    }
                } => { if changed.is_err() { shell_changes = None; } }
            }
        }
    }

    pub(crate) fn has_open_tool_batch(&self) -> bool {
        let mut pending = std::collections::HashSet::new();
        for block in self
            .context
            .messages
            .iter()
            .flat_map(|message| &message.content)
        {
            match block {
                providers::ContentBlock::ToolUse { id, .. } => {
                    pending.insert(id.as_str());
                }
                providers::ContentBlock::ToolResult { tool_call_id, .. } => {
                    pending.remove(tool_call_id.as_str());
                }
                _ => {}
            }
        }
        !pending.is_empty()
    }

    fn stage_restart_shell_results(&mut self) -> Result<(), String> {
        if self.has_open_tool_batch() {
            return Ok(());
        }
        for job in self
            .shared
            .executor
            .shell_job_snapshots(&self.task.run_id.to_string())
        {
            if job.state == tools::ShellJobState::Terminal && !job.teardown_confirmed {
                return Err("shell process teardown is unconfirmed".into());
            }
            if job.state != tools::ShellJobState::Terminal
                || job.observed
                || self
                    .shell_checkpoint_results
                    .contains_key(job.identity.id())
            {
                continue;
            }
            let outcome = self
                .shared
                .executor
                .shell_job_result(&job.identity)
                .map_err(|error| error.to_string())?;
            let call = job
                .call_id
                .as_deref()
                .ok_or("shell result has no original call identity")?;
            // This call already returned its continuing-job handle. A second
            // ToolResult for it would be invalid, and replacing it would break
            // the sent cache prefix. Append an identity-bound completion instead.
            self.context.push_user(&format!(
                "Shell job terminal result (original call: {call}, job: {}):\n{}",
                job.identity.id(),
                outcome.result.content
            ));
            self.shell_checkpoint_results
                .insert(job.identity.id().into(), job.identity);
            self.publish_message_count();
            // Attempt immediately after presentation. The safe-point caller
            // classifies a remaining ACK failure as retryable, not Failed.
            let _ = self.acknowledge_restart_shell_results();
        }
        Ok(())
    }

    pub(crate) fn acknowledge_restart_shell_results(&mut self) -> Result<(), String> {
        for identity in self.shell_checkpoint_results.values() {
            if self
                .shared
                .executor
                .shell_job_snapshots(&self.task.run_id.to_string())
                .iter()
                .any(|j| j.identity == *identity && j.observed)
            {
                continue;
            }
            self.shared
                .executor
                .acknowledge_shell_job_result(identity)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn has_unstaged_shell_results(&self) -> bool {
        let run = self.task.run_id.to_string();
        let jobs = self.shared.executor.shell_job_snapshots(&run);
        if jobs.is_empty() {
            return self.shared.executor.has_unobserved_shell_jobs(&run);
        }
        // A presented identity suppresses duplicate text, not the safety
        // proof. Until its ACK succeeds, ordinary persistence must also refuse
        // to save a context that disagrees with external-effect observation.
        jobs.iter()
            .any(|job| !job.teardown_confirmed || !job.observed)
    }

    fn restart_safety_blocker(&self) -> Option<RestartBlockerKind> {
        let run = self.task.run_id.to_string();
        if self.shared.executor.has_running_shell_jobs(&run) {
            Some(RestartBlockerKind::ShellRunning)
        } else if self.has_unstaged_shell_results() {
            Some(RestartBlockerKind::ShellResultUnobserved)
        } else if self.run_js_unconfirmed_detail().is_some() {
            Some(RestartBlockerKind::UnconfirmedRunJs)
        } else {
            None
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
            .has_running_shell_jobs(&self.task.run_id.to_string())
            || self.has_unstaged_shell_results()
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
        #[cfg(test)]
        if let Ok(execution) = runtime.execution_snapshot(self.task.run_id) {
            runtime.execution_hook(crate::runtime::HookPoint::TerminalRetiring, &execution);
        }
        // The active intent and its terminal tail must overlap. No sealer may
        // observe neither after finalization has closed the mailbox.
        runtime.invalidate_active_restart_capture("terminal retirement during capture");
        runtime.prepare_terminal_dispatch(self.task.run_id);
        runtime.end_intent(
            self.task.run_id,
            &self.task.publication.incarnation(),
            reason,
        );
        #[cfg(test)]
        if let Ok(execution) = runtime.execution_snapshot(self.task.run_id) {
            runtime.execution_hook(crate::runtime::HookPoint::TerminalIntentRetired, &execution);
        }
    }
}
