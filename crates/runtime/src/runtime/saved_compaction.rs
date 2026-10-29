//! A context-only transaction. Reservation spans model I/O without holding a mutex.
use super::*;
use crate::restore::{RestoredState, RunRestoreDescriptor};

struct Reservation<'a> {
    runtime: &'a AgentRuntime,
    run: RunId,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.runtime
            .shared
            .saved_compactions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.run);
    }
}

impl AgentRuntime {
    pub(super) fn ensure_no_saved_compaction(&self, run: RunId) -> Result<(), RuntimeError> {
        if self
            .shared
            .saved_compactions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&run)
        {
            return Err(RuntimeError::CompactionInFlight {
                run_id: run.to_string(),
            });
        }
        Ok(())
    }

    /// Compact a saved root without registering an execution or starting an agent turn.
    /// Concurrent continue, stop, and saved-compaction requests fail with
    /// `CompactionInFlight`; callers may retry after this future completes.
    /// Failure (including dropping the future) leaves the saved context unchanged.
    pub async fn compact_saved_chat(
        &self,
        run: RunId,
        mut authority: RunConfig,
    ) -> Result<(), RuntimeError> {
        let fail = |reason: String| RuntimeError::SavedCompactionFailed {
            run_id: run.to_string(),
            reason,
        };
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| RuntimeError::RunRestoreFailed {
                run_id: run.to_string(),
                reason: crate::RunRestoreFailure::StorageNotConfigured,
            })?;
        let gate = self.stop_restore_gate(run);
        let reservation = {
            let _restore = store
                .restore_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _stop = gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.ensure_no_saved_compaction(run)?;
            let admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let runs = lock_runs(&self.shared.runs);
            if admissions
                .get(&run)
                .is_some_and(|a| a.result.borrow().is_none())
                || runs.get(&run).is_some_and(|entry| {
                    matches!(
                        *entry.phase_rx.borrow(),
                        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
                    )
                })
            {
                return Err(fail(
                    "run is live; request compaction at its next turn boundary".into(),
                ));
            }
            self.shared
                .saved_compactions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(run);
            // Terminal publication follows all snapshot writes and effect teardown.
            Reservation { runtime: self, run }
        };
        let bundle = store
            .restore_bundle(run)
            .map_err(|error| fail(error.to_string()))?
            .ok_or_else(|| RuntimeError::RunRestoreFailed {
                run_id: run.to_string(),
                reason: crate::RunRestoreFailure::MissingContext,
            })?;
        let mut record = bundle.record.clone();
        let mut descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).map_err(|error| fail(error.to_string()))?;
        self.validate_history_restore(&record, &descriptor, &authority)?;
        let role = Role::from_name(&descriptor.role).map_err(|error| fail(error.to_string()))?;
        if descriptor.parent_run_id.is_some()
            || record.parent_run_id.is_some()
            || record.role != descriptor.role
            || descriptor.pending_escalation.is_some()
        {
            return Err(fail(
                "saved context is not an ordinary conversation root".into(),
            ));
        }
        if let Some(permit) = &authority.ownership
            && descriptor
                .thread_id
                .as_deref()
                .is_some_and(|thread| thread != permit.thread_id)
        {
            return Err(RuntimeError::StaleOwnership {
                run_id: run.to_string(),
            });
        }
        let mut restored = RestoredState::from_record(&record)?;
        restored.provenance =
            crate::restore::provenance::ConversationProvenance::from_bundle(&bundle);
        authority.images.clear();
        authority.project_root = descriptor.project_root.clone();
        authority.category = descriptor.category.clone();
        authority.model_preference = authority
            .model_preference
            .or(descriptor.model_preference.clone());
        self.shared
            .model
            .restore_route_pins(&run.to_string(), &descriptor.route_pins);
        let mut state = crate::agent_loop::LoopState::for_saved_compaction(
            self,
            run,
            role,
            authority.clone(),
            restored,
        )
        .await;
        let mut context = state.context.clone();
        let mut events = Vec::new();
        crate::compaction::compact_candidate(
            &mut state,
            &mut context,
            event_bus::CompactionReason::Manual,
            None,
            &mut events,
        )
        .await
        .map_err(|error| fail(error.to_string()))?;
        // These flags make the next request the sole replacement boundary. Raw history,
        // completion status, authority restrictions, receipts and run identity stay intact.
        descriptor.boundary_pending = true;
        descriptor.responses_replay = Some(context.responses_replay.clone());
        descriptor.allowed_directories.rewrite_pending = true;
        descriptor.allowed_instructions.forget_history();
        record.checkpoints_json = serde_json::to_string(context.checkpoints())
            .map_err(|error| fail(error.to_string()))?;
        record.config_json =
            serde_json::to_string(&descriptor).map_err(|error| fail(error.to_string()))?;
        let mut write = context
            .provenance
            .write(record)
            .map_err(|error| fail(error.to_string()))?;
        write.expected_snapshot_revision = Some(bundle.snapshot_revision);
        {
            let _restore = store
                .restore_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _stop = gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _owner = authority
                .ownership
                .as_ref()
                .map(|permit| permit.mutation_guard())
                .transpose()
                .map_err(|error| fail(error.to_string()))?;
            store
                .handle
                .save_snapshot_bundle(&write)
                .map_err(|error| fail(format!("{error:?}")))?;
        }
        for event in events {
            self.shared.bus.emit(event);
        }
        drop(reservation);
        Ok(())
    }
}
