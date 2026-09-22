use super::*;
use crate::RunRestoreFailure;
use crate::restore::{RestoredState, RunRestoreDescriptor};

mod renewal;
#[cfg(test)]
mod tests;

pub(super) enum RunContinuation {
    Fresh,
    Awaited,
    Handoff(RunHandoff),
    Restored(RestoredState),
}

impl AgentRuntime {
    /// Continue a goal root in place, including after process restart.
    /// Persisted root identity supplies the role; the caller supplies current authority.
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
        let previous = {
            let runs = lock_runs(&self.shared.runs);
            runs.get(&run_id)
                .map(|entry| (entry.role, *entry.phase_rx.borrow()))
        };
        if matches!(
            previous,
            Some((
                _,
                AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
            ))
        ) {
            self.set_model_preference(run_id, authority.model_preference)?;
            self.send_message_with_images(run_id, prompt, authority.images)?;
            return Ok(run_id);
        }
        let store = store.ok_or_else(|| fail(RunRestoreFailure::StorageNotConfigured))?;
        let mut record = store
            .restore_record(run_id)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?
            .ok_or_else(|| fail(RunRestoreFailure::MissingContext))?;
        let mut descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json)
            .map_err(|error| fail(RunRestoreFailure::CorruptContext(error.to_string())))?;
        self.validate_history_restore(&record, &descriptor, &authority)?;
        let role = Role::from_name(&descriptor.role)
            .map_err(|error| fail(RunRestoreFailure::UnsupportedConfig(error.to_string())))?;
        if previous.is_some_and(|(previous_role, _)| previous_role != role)
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
        let mut restored_source = None;
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
                        self.validate_history_restore(&record, &descriptor, &config)?;
                        if record.role != role.name()
                            || descriptor.role != role.name()
                            || record.parent_run_id.is_some()
                            || descriptor.parent_run_id.is_some()
                        {
                            return Err(fail(RunRestoreFailure::UnsupportedConfig(
                                descriptor
                                    .non_restorable_reason
                                    .unwrap_or_else(|| "chat identity".into()),
                            )));
                        }
                        restored_source =
                            Some(crate::meta::parse_run_id(&record.run_id).map_err(|reason| {
                                fail(RunRestoreFailure::CorruptContext(reason))
                            })?);
                        Some(RestoredState::from_record(&record)?)
                    }
                }
            }
        };
        config.name = Some(name);
        let run_id = RunId::new(self.shared.next_run_id.fetch_add(1, Ordering::Relaxed));
        if let (Some(source), Some(restored)) = (restored_source, restored.as_ref()) {
            self.inherit_user_questions(source, run_id, &restored.messages)
                .map_err(|reason| RuntimeError::RunRestoreFailed {
                    run_id: source.to_string(),
                    reason: RunRestoreFailure::CorruptContext(format!(
                        "question inheritance failed: {reason}"
                    )),
                })?;
        }
        let continuation = restored.map_or(RunContinuation::Fresh, RunContinuation::Restored);
        Ok(self.spawn_run_with_handoff(run_id, None, role, prompt, config, continuation))
    }
}
