use super::*;
use crate::restore::{RestoredState, RunRestoreDescriptor};
use crate::{RunConfig, RunRestoreFailure};
use event_bus::{AgentMessage, AgentMessageKind};

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
    ) -> Result<(), String> {
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
    ) -> Result<Option<FinalizingChild>, String> {
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
        let (role, ownership, budget, terminal) = {
            let runs = super::super::lock_runs(&self.shared.runs);
            let entry = &runs[&child];
            let parent = &runs[&caller];
            if parent.cancel_tx.borrow().is_interrupted() {
                return Err("answer saved, but the parent was interrupted; retry after explicitly resuming the parent".into());
            }
            let terminal = *entry.phase_rx.borrow();
            if matches!(
                terminal,
                AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
            ) {
                // An interruption of a live child is the operator's or parent's
                // decision; the answer must not undo it.
                match *entry.cancel_tx.borrow() {
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
                    caller_cancel: parent.cancel_tx.subscribe(),
                }));
            }
            if let Some(reason) = self.continuation_refusal(child, Some(entry)) {
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
                entry.role,
                parent.config.ownership.clone(),
                entry.config.budget.clone(),
                terminal,
            )
        };
        let _authority = ownership
            .as_ref()
            .map(|permit| permit.mutation_guard())
            .transpose()
            .map_err(|error| error.to_string())?;
        let mut record = store
            .restore_record(child)
            .map_err(|error| error.to_string())?
            .ok_or("answer saved, but the finished child has no saved context")?;
        let mut descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).map_err(|error| error.to_string())?;
        // A failed terminal snapshot leaves the last completed tool round.
        let snapshot_matches = match terminal {
            AgentRunPhase::Done => record.terminal_phase == "Done",
            _ => matches!(
                record.terminal_phase.as_str(),
                "Error" | "Stopped" | "Checkpoint"
            ),
        };
        if descriptor.parent_run_id != Some(caller)
            || record.parent_run_id.as_deref() != Some(caller.to_string().as_str())
            || crate::Role::from_name(&descriptor.role) != Ok(role)
            || record.role != descriptor.role
            || !snapshot_matches
        {
            return Err("answer saved, but the finished child context does not match its identity or terminal phase".into());
        }
        let history =
            RestoredState::for_conversation(&record).map_err(|error| error.to_string())?;
        if Self::answer_is_in_history(&history.messages, question) {
            return Ok(None);
        }
        let restored = self
            .renewed_child_state(&record, &descriptor, ownership.as_ref())
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
            keep_alive: descriptor.keep_alive,
            ownership,
            budget,
            ..RunConfig::default()
        };
        self.validate_history_restore(&record, &descriptor, &config)
            .map_err(|error| error.to_string())?;
        self.reserve_restored_shell_handles(child, &restored)
            .map_err(|error| error.to_string())?;
        let mut runs = super::super::lock_runs(&self.shared.runs);
        if !matches!(
            *runs[&caller].phase_rx.borrow(),
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
        ) || *runs[&child].phase_rx.borrow() != terminal
            || runs[&caller].cancel_tx.borrow().is_interrupted()
            || self.continuation_refusal(child, runs.get(&child)).is_some()
        {
            return Err(
                "answer saved, but the parent or child changed state before continuation".into(),
            );
        }
        descriptor.restorable = false;
        descriptor.non_restorable_reason = Some("snapshot_consumed".into());
        record.restorable = false;
        record.config_json =
            serde_json::to_string(&descriptor).map_err(|error| error.to_string())?;
        let write = restored
            .provenance
            .write(record.clone())
            .map_err(|error| error.to_string())?;
        store
            .handle
            .save_snapshot_bundle(&write)
            .map_err(|error| format!("{error:?}"))?;
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
            (child, Some(caller), role, config, caller),
            restored,
            &trigger,
            None,
        );
        Ok(None)
    }
}

fn refused(reason: &RunRestoreFailure) -> String {
    format!("answer saved, but {reason}")
}
