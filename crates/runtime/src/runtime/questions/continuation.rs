use super::*;
use crate::restore::{RestoredState, RunRestoreDescriptor};
use crate::{RunConfig, RunRestoreFailure};
use event_bus::{AgentMessage, AgentMessageKind};

/// Why an answered child could not be continued. Only the caller that can
/// repair the cause may retry; a deliberate refusal must never be retried.
#[derive(Debug)]
pub(crate) enum ChildResumeError {
    /// Stop, cancel, ownership, identity or corrupt-context decisions. The answer
    /// is saved and retrying the same continuation cannot change the outcome.
    Refused(String),
    /// A storage read or write failed before any child state changed. The same
    /// continuation can succeed once storage recovers.
    Retryable(String),
}

impl std::fmt::Display for ChildResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(message) | Self::Retryable(message) => f.write_str(message),
        }
    }
}

impl From<String> for ChildResumeError {
    fn from(message: String) -> Self {
        Self::Refused(message)
    }
}

impl From<&str> for ChildResumeError {
    fn from(message: &str) -> Self {
        Self::Refused(message.to_owned())
    }
}

fn storage_is_retryable(error: &storage::StorageError) -> bool {
    use storage::StorageError as E;
    matches!(
        error,
        E::Sqlite(_) | E::Io(_) | E::WriterClosed | E::StaleMutation
    )
}

fn read_failure(error: storage::StorageError) -> ChildResumeError {
    let message = error.to_string();
    if storage_is_retryable(&error) {
        ChildResumeError::Retryable(message)
    } else {
        ChildResumeError::Refused(message)
    }
}

fn save_failure(error: storage::thread_index::SnapshotApiError) -> ChildResumeError {
    use storage::thread_index::SnapshotApiError as E;
    let retryable = match &error {
        E::RevisionConflict => true,
        E::Storage(error) => storage_is_retryable(error),
        _ => false,
    };
    let message = crate::restore::provenance::save_failure_reason(error);
    if retryable {
        ChildResumeError::Retryable(message)
    } else {
        ChildResumeError::Refused(message)
    }
}

struct FinalizingChild {
    phase: tokio::sync::watch::Receiver<AgentRunPhase>,
    caller_cancel: tokio::sync::watch::Receiver<crate::RunInterrupt>,
}

impl AgentRuntime {
    /// A direct parent's answer renews execution authority; saved history never does.
    pub(super) async fn resume_answered_child(
        &self,
        caller: RunId,
        child: RunId,
        question: &UserQuestion,
    ) -> Result<(), ChildResumeError> {
        self.deny_deleted_run(&child.to_string())
            .map_err(|deleted| deleted.to_string())?;
        while let Some(mut waiting) = self.try_resume_answered_child(caller, child, question)? {
            tokio::select! {
                biased;
                _ = waiting.caller_cancel.wait_for(|interrupt| interrupt.is_interrupted()) => {
                    return Err("answer saved, but the parent was interrupted; retry after explicitly resuming the parent".into());
                }
                result = waiting.phase.wait_for(|phase| {
                    matches!(
                        phase,
                        AgentRunPhase::Done | AgentRunPhase::Stopped | AgentRunPhase::Error
                    )
                }) => {
                    result.map_err(|_| "answer saved, but the child disappeared during finalization")?;
                }
            }
        }
        Ok(())
    }

    fn try_resume_answered_child(
        &self,
        caller: RunId,
        child: RunId,
        question: &UserQuestion,
    ) -> Result<Option<FinalizingChild>, ChildResumeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or("question storage is not configured")?;
        let _restore = store
            .restore_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.validate_question_parent(caller, child)?;
        let (registered_role, ownership, budget, terminal) = {
            let runs = super::super::lock_runs(&self.shared.runs);
            let entry = runs.get(&child);
            let parent = &runs[&caller];
            if parent.execution.cancel_tx.borrow().is_interrupted() {
                return Err("answer saved, but the parent was interrupted; retry after explicitly resuming the parent".into());
            }
            let terminal = entry.map(|entry| *entry.phase_rx.borrow());
            if let Some(entry) = entry.filter(|_| {
                matches!(
                    terminal,
                    Some(AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting)
                )
            }) {
                // An interruption of a live child is the operator's or parent's
                // decision; the answer must not undo it.
                match *entry.execution.cancel_tx.borrow() {
                    crate::state::RunInterrupt::Stop => {
                        return Err(refused(&RunRestoreFailure::StoppedByOperator));
                    }
                    crate::state::RunInterrupt::Cancel => {
                        return Err(refused(&RunRestoreFailure::Cancelled));
                    }
                    crate::state::RunInterrupt::None => {}
                }
                // finish_success closes input before asynchronous cleanup;
                // its public phase remains live until the snapshot is safe.
                return Ok(entry.mailbox.is_closed().then(|| FinalizingChild {
                    phase: entry.phase_rx.clone(),
                    caller_cancel: parent.execution.cancel_tx.subscribe(),
                }));
            }
            if let Some(reason) = self.continuation_refusal(child, entry) {
                return Err(refused(&reason));
            }
            if !matches!(
                *parent.phase_rx.borrow(),
                AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
            ) {
                return Err(
                    "answer saved, but continuing a completed child requires a live parent".into(),
                );
            }
            (
                entry.map(|entry| entry.role),
                parent.config.ownership.clone(),
                entry.map_or_else(Default::default, |entry| entry.config.budget.clone()),
                terminal,
            )
        };
        let _authority = ownership
            .as_ref()
            .map(|permit| permit.mutation_guard())
            .transpose()
            .map_err(|error| error.to_string())?;
        let bundle = store
            .restore_bundle(child)
            .map_err(read_failure)?
            .ok_or("answer saved, but the finished child has no saved context")?;
        let mut record = bundle.record.clone();
        let mut descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).map_err(|error| error.to_string())?;
        // A failed terminal snapshot leaves the last completed tool round.
        let snapshot_matches = match terminal {
            Some(AgentRunPhase::Done) => record.terminal_phase == "Done",
            Some(_) => matches!(
                record.terminal_phase.as_str(),
                "Error" | "Stopped" | "Checkpoint"
            ),
            None => true,
        };
        let role = crate::Role::from_name(&descriptor.role).map_err(|error| error.to_string())?;
        if descriptor.parent_run_id != Some(caller)
            || record.parent_run_id.as_deref() != Some(caller.to_string().as_str())
            || registered_role.is_some_and(|registered| registered != role)
            || record.role != descriptor.role
            || !snapshot_matches
        {
            return Err("answer saved, but the finished child context does not match its identity or terminal phase".into());
        }
        self.reject_nested_orchestrator(&record, &descriptor)
            .map_err(|error| format!("answer saved, but {error}"))?;
        let history =
            RestoredState::for_conversation(&record).map_err(|error| error.to_string())?;
        if Self::answer_is_in_history(&history.messages, question) {
            return Ok(None);
        }
        let restored = self
            .renewed_child_state(&bundle, &descriptor, ownership.as_ref())
            .map_err(|error| {
                format!("answer saved, but child continuation is unavailable: {error}")
            })?;
        let config = RunConfig {
            name: descriptor.name.clone(),
            category: descriptor.category.clone(),
            load_skills: descriptor.load_skills.clone(),
            model_preference: descriptor.model_preference.clone(),
            project_root: descriptor.project_root.clone(),
            interactive: descriptor.interactive,
            lane_scope: descriptor.lane_scope,
            spawn_credits: descriptor.spawn_credits.clone(),
            keep_alive: descriptor.keep_alive,
            ownership,
            budget,
            ..RunConfig::default()
        };
        self.validate_history_restore(&record, &descriptor, &config)
            .map_err(|error| error.to_string())?;
        self.reserve_restored_shell_handles(child, &restored)
            .map_err(|error| error.to_string())?;
        #[cfg(test)]
        if let Ok(execution) = self.execution_snapshot(caller) {
            self.execution_hook(
                super::super::cancellation::HookPoint::QuestionChildPrepared,
                &execution,
            );
        }
        let mut intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut runs = super::super::lock_runs(&self.shared.runs);
        if runs.get(&caller).is_none_or(|parent| {
            !matches!(
                *parent.phase_rx.borrow(),
                AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
            ) || parent.execution.cancel_tx.borrow().is_interrupted()
        }) || runs.get(&child).map(|entry| *entry.phase_rx.borrow()) != terminal
            || self.continuation_refusal(child, runs.get(&child)).is_some()
        {
            return Err(
                "answer saved, but the parent or child changed state before continuation".into(),
            );
        }
        let execution = Self::prepare_restored_execution(child, Some(caller), &intents)
            .map_err(|e| e.to_string())?;
        descriptor.restorable = false;
        descriptor.non_restorable_reason = Some("snapshot_consumed".into());
        record.restorable = false;
        record.config_json =
            serde_json::to_string(&descriptor).map_err(|error| error.to_string())?;
        let expected = restored
            .snapshot_revision
            .get()
            .ok_or("answer saved, but the child snapshot revision is unknown")?;
        let write = restored
            .provenance
            .write(record.clone(), expected)
            .map_err(|error| error.to_string())?;
        #[cfg(test)]
        if let Some(entry) = runs.get(&child) {
            self.execution_hook(
                super::super::cancellation::HookPoint::ContinuationSnapshotPrepared,
                &std::sync::Arc::clone(&entry.execution),
            );
        }
        let saved = store
            .handle
            .save_snapshot_bundle(&write)
            .map_err(save_failure)?;
        restored.snapshot_revision.commit(saved.snapshot_revision);
        Self::install_restored_execution(
            child,
            Some(caller),
            &mut intents,
            &mut admissions,
            &execution,
        );
        self.shared.run_ids.observe(child);
        self.shared
            .model
            .restore_route_pins(&child.to_string(), &descriptor.route_pins);
        let trigger = AgentMessage {
            message_id: format!("question-answer:{}", question.id),
            sender_run_id: caller.to_string(),
            recipient_run_id: child.to_string(),
            kind: AgentMessageKind::Send,
            content: String::new(),
            reply_to: None,
        };
        self.register_restored(
            &mut runs,
            &intents,
            execution,
            (child, Some(caller), role, config, caller),
            restored,
            &trigger,
            None,
        )
        .map_err(|e| e.to_string())?;
        Ok(None)
    }
}

fn refused(reason: &RunRestoreFailure) -> ChildResumeError {
    ChildResumeError::Refused(format!("answer saved, but {reason}"))
}
