use super::*;
use crate::RunRestoreFailure;
use crate::restore::{RestoredState, RunRestoreDescriptor};

#[cfg(test)]
mod tests;

pub(super) enum RunContinuation {
    Fresh,
    Handoff(RunHandoff),
    Restored(RestoredState),
}

impl AgentRuntime {
    /// Continue a goal root in place, restoring terminal history without restoring stale ownership.
    pub fn continue_goal(
        &self,
        run_id: RunId,
        prompt: String,
        authority: RunConfig,
    ) -> Result<RunId, RuntimeError> {
        let fail = |reason| RuntimeError::RunRestoreFailed {
            run_id: run_id.to_string(),
            reason,
        };
        let store = self.shared.run_store.get();
        let _guard = store.map(|store| {
            store
                .restore_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let (role, phase) = {
            let entry = self.entry(run_id)?;
            (entry.role, *entry.phase_rx.borrow())
        };
        match phase {
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => {
                self.set_model_preference(run_id, authority.model_preference)?;
                self.send_message_with_images(run_id, prompt, authority.images)?;
                return Ok(run_id);
            }
            AgentRunPhase::Done | AgentRunPhase::Error => {}
        }
        let store = store.ok_or_else(|| fail(RunRestoreFailure::StorageNotConfigured))?;
        if store.snapshot_failed(run_id) {
            return Err(fail(RunRestoreFailure::MissingContext));
        }
        let mut record = store
            .restore_record(run_id)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?
            .ok_or_else(|| fail(RunRestoreFailure::MissingContext))?;
        let mut descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
        if !record.restorable || !descriptor.restorable {
            return Err(fail(RunRestoreFailure::UnsupportedConfig(
                descriptor
                    .non_restorable_reason
                    .unwrap_or_else(|| "実行設定".into()),
            )));
        }
        if descriptor.role != role.name()
            || record.role != descriptor.role
            || descriptor.parent_run_id.is_some()
            || record.parent_run_id.is_some()
        {
            return Err(fail(RunRestoreFailure::CorruptContext(
                "goal identity".into(),
            )));
        }
        let restored = RestoredState::from_record(&record)?;
        descriptor.restorable = false;
        descriptor.non_restorable_reason = Some("snapshot_consumed".into());
        record.restorable = false;
        record.config_json = serde_json::to_string(&descriptor)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
        store
            .handle
            .upsert_run_context(&record)
            .map_err(|error| fail(RunRestoreFailure::SnapshotConsumeFailed(error.to_string())))?;
        let config = RunConfig {
            name: descriptor.name,
            interactive: true,
            keep_alive: true,
            ..authority
        };
        Ok(self.spawn_run_with_handoff(
            run_id,
            None,
            role,
            prompt,
            config,
            RunContinuation::Restored(restored),
        ))
    }

    /// Start a chat run with the thread's latest terminal context, if one exists.
    /// Current configuration supplies fresh execution authority; only history is reused.
    ///
    /// # Errors
    /// Rejects unreadable or invalid snapshots rather than silently dropping history.
    pub fn delegate_chat(
        &self,
        thread_id: &str,
        role: Role,
        prompt: String,
        mut config: RunConfig,
    ) -> Result<RunId, RuntimeError> {
        let name = format!("chat:{}:{thread_id}", role.name());
        let restored = match self.shared.run_store.get() {
            None => None,
            Some(store) => {
                let _guard = store
                    .restore_gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let fail = |reason| RuntimeError::RunRestoreFailed {
                    run_id: name.clone(),
                    reason,
                };
                match store
                    .latest_terminal_named(&name)
                    .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?
                {
                    None => None,
                    Some(record) => {
                        let descriptor: RunRestoreDescriptor =
                            serde_json::from_str(&record.config_json).map_err(|error| {
                                fail(RunRestoreFailure::CorruptContext(error.to_string()))
                            })?;
                        // Ownership is deliberately renewed by the GUI, never restored from disk.
                        let supported = descriptor.restorable
                            || descriptor.non_restorable_reason.as_deref()
                                == Some("復元対象外の実行状態: ownership");
                        if !supported
                            || record.role != role.name()
                            || descriptor.role != role.name()
                            || record.parent_run_id.is_some()
                        {
                            return Err(fail(RunRestoreFailure::UnsupportedConfig(
                                descriptor
                                    .non_restorable_reason
                                    .unwrap_or_else(|| "chat identity".into()),
                            )));
                        }
                        Some(RestoredState::from_record(&record)?)
                    }
                }
            }
        };
        config.name = Some(name);
        let run_id = RunId::new(self.shared.next_run_id.fetch_add(1, Ordering::Relaxed));
        let continuation = restored.map_or(RunContinuation::Fresh, RunContinuation::Restored);
        Ok(self.spawn_run_with_handoff(run_id, None, role, prompt, config, continuation))
    }
}
