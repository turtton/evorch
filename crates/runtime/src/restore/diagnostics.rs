//! Read-only restoration status for CLI, GUI, and task inspection.

use super::{InterruptedToolCall, RestoredState, RunRestoreDescriptor, TeamRestoreIdentity};
use crate::{AgentRuntime, RunId, RunRestoreFailure, RuntimeError};
use serde::{Deserialize, Serialize};

/// A saved checkpoint describes available history, never proof of current authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRestoreDiagnostics {
    pub run_id: RunId,
    pub last_successful_checkpoint_at_ns: i64,
    pub checkpoint_phase: String,
    pub message_count: usize,
    pub compaction_checkpoint_count: usize,
    pub disk_restorable: bool,
    /// Conditional: current ownership/team authority and absence of live claims are checked at entry.
    pub history_available_with_current_authority: bool,
    pub refusal_reason: Option<String>,
    pub renewable_team: Option<TeamRestoreIdentity>,
    pub interrupted_tool_calls: Vec<InterruptedToolCall>,
    pub durable_task_id: Option<String>,
}

impl AgentRuntime {
    /// Inspect the last successfully persisted checkpoint without restoring or executing anything.
    /// A missing store or snapshot returns `None`; malformed persisted context returns an error.
    pub fn restore_diagnostics(
        &self,
        run_id: RunId,
    ) -> Result<Option<RunRestoreDiagnostics>, RuntimeError> {
        let fail = |reason| RuntimeError::RunRestoreFailed {
            run_id: run_id.to_string(),
            reason: RunRestoreFailure::CorruptContext(reason),
        };
        let Some(store) = self.shared.run_store.get() else {
            return Ok(None);
        };
        let Some(record) = store
            .restore_record(run_id)
            .map_err(|error| fail(error.to_string()))?
        else {
            return Ok(None);
        };
        let descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).map_err(|error| fail(error.to_string()))?;
        // Validate complete history even when incomplete tool effects block executing it.
        let mut history_record = record.clone();
        let mut history_descriptor = descriptor.clone();
        history_descriptor.interrupted_tool_calls.clear();
        history_record.config_json =
            serde_json::to_string(&history_descriptor).map_err(|error| fail(error.to_string()))?;
        let history = RestoredState::from_record(&history_record)?;
        let disk_restorable =
            record.restorable && descriptor.restorable && !descriptor.has_uncertain_effects();
        let renewable = descriptor.conversation_descriptor();
        let history_available_with_current_authority = (record.restorable && renewable.restorable)
            || renewable.renewable_ownership_only()
            || renewable.renewable_root_context()
            || renewable.renewable_team_root();
        Ok(Some(RunRestoreDiagnostics {
            run_id,
            last_successful_checkpoint_at_ns: record.updated_at_ns,
            checkpoint_phase: record.terminal_phase,
            message_count: history.messages.len(),
            compaction_checkpoint_count: history.checkpoints.len(),
            disk_restorable,
            history_available_with_current_authority,
            refusal_reason: renewable.non_restorable_reason,
            renewable_team: descriptor.renewable_team,
            interrupted_tool_calls: descriptor.interrupted_tool_calls,
            durable_task_id: descriptor.durable_task_id,
        }))
    }
}
