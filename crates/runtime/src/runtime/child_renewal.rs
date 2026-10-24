//! A live direct parent may continue its finished child under the parent's
//! current authority. Saved history alone never renews execution authority.

use super::*;
use crate::RunRestoreFailure;
use crate::restore::{RestoredState, RunRestoreDescriptor};

impl AgentRuntime {
    /// Operator stops hold until the thread root is resumed; cancellation discards.
    pub(super) fn continuation_refusal(
        &self,
        run_id: RunId,
        entry: Option<&RunEntry>,
    ) -> Option<RunRestoreFailure> {
        if self.operator_stopped(run_id) {
            return Some(RunRestoreFailure::StoppedByOperator);
        }
        entry
            .is_some_and(|entry| {
                matches!(
                    *entry.cancel_tx.borrow(),
                    crate::state::RunInterrupt::Cancel
                ) || entry.terminal_reason.as_deref() == Some(event_bus::CANCELLED_REASON)
            })
            .then_some(RunRestoreFailure::Cancelled)
    }

    /// Restore a child's saved history for its live parent. Ownership-only
    /// records are renewed with the parent's current permit, and interrupted
    /// tool calls are reported as outcome-unknown instead of refusing the child.
    pub(super) fn renewed_child_state(
        &self,
        record: &storage::RunContextRecord,
        descriptor: &RunRestoreDescriptor,
        ownership: Option<&crate::ownership::OwnerPermit>,
    ) -> Result<RestoredState, RuntimeError> {
        let history = descriptor.conversation_descriptor();
        if !(record.restorable && history.restorable)
            && !(history.renewable_ownership_only() && ownership.is_some())
        {
            return Err(RuntimeError::RunRestoreFailed {
                run_id: record.run_id.clone(),
                reason: RunRestoreFailure::UnsupportedConfig(
                    history
                        .non_restorable_reason
                        .unwrap_or_else(|| "実行設定".into()),
                ),
            });
        }
        if let Some(permit) = ownership {
            permit
                .validate_generation()
                .map_err(|_| RuntimeError::StaleOwnership {
                    run_id: record.run_id.clone(),
                })?;
        }
        RestoredState::for_conversation(record)
    }

    /// Whether `parent` could continue its finished `child` with a message now,
    /// or why not. This reads saved state only and changes nothing.
    pub(crate) fn continuation_availability(
        &self,
        parent: RunId,
        child: RunId,
    ) -> Result<(), String> {
        let ownership = {
            let runs = lock_runs(&self.shared.runs);
            if let Some(reason) = self.continuation_refusal(child, runs.get(&child)) {
                return Err(reason.to_string());
            }
            runs.get(&parent)
                .and_then(|entry| entry.config.ownership.clone())
        };
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| RunRestoreFailure::StorageNotConfigured.to_string())?;
        let record = store
            .restore_record(child)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| RunRestoreFailure::MissingContext.to_string())?;
        let descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).map_err(|error| error.to_string())?;
        let history = descriptor.conversation_descriptor();
        if (record.restorable && history.restorable)
            || (history.renewable_ownership_only() && ownership.is_some())
        {
            Ok(())
        } else {
            Err(history
                .non_restorable_reason
                .unwrap_or_else(|| "実行設定".into()))
        }
    }
}
