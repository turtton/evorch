//! The context a run actually holds, read from its last persisted checkpoint.

use crate::restore::{RestoredState, RunRestoreDescriptor};
use crate::{AgentRuntime, RunId, RunRestoreFailure, RuntimeError};

/// Messages and request settings recorded at a run's last snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct RunContextView {
    pub run_id: RunId,
    pub role_name: String,
    pub category: Option<String>,
    /// `Checkpoint` for live runs, otherwise the terminal phase.
    pub checkpoint_phase: String,
    pub updated_at_ns: i64,
    /// Absent for snapshots written before the inspector existed.
    pub selected_model: Option<String>,
    pub tool_names: Vec<String>,
    /// Secret-redacted history, ending at the last complete tool round.
    pub messages: Vec<providers::Message>,
    pub compaction_checkpoint_count: usize,
}

impl AgentRuntime {
    /// Read the last persisted context of a run without restoring or executing anything.
    ///
    /// A missing store or snapshot returns `None`; malformed persisted context returns an error.
    ///
    /// # Errors
    /// Returns [`RuntimeError::RunRestoreFailed`] when the stored context cannot be parsed.
    pub fn run_context_view(&self, run_id: RunId) -> Result<Option<RunContextView>, RuntimeError> {
        let fail = |reason| RuntimeError::RunRestoreFailed {
            run_id: run_id.to_string(),
            reason: RunRestoreFailure::CorruptContext(reason),
        };
        let Some(store) = self.shared.run_store.get() else {
            return Ok(None);
        };
        let Some(mut record) = store
            .restore_record(run_id)
            .map_err(|error| fail(error.to_string()))?
        else {
            return Ok(None);
        };
        let mut descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).map_err(|error| fail(error.to_string()))?;
        let view_descriptor = RunRestoreDescriptor {
            interrupted_tool_calls: Vec::new(),
            ..descriptor.clone()
        };
        // Interrupted calls block execution, not reading the history.
        record.config_json =
            serde_json::to_string(&view_descriptor).map_err(|error| fail(error.to_string()))?;
        let history = RestoredState::from_record(&record)?;
        Ok(Some(RunContextView {
            run_id,
            role_name: std::mem::take(&mut descriptor.role),
            category: descriptor.category.take(),
            checkpoint_phase: record.terminal_phase,
            updated_at_ns: record.updated_at_ns,
            selected_model: descriptor.selected_model.take(),
            tool_names: std::mem::take(&mut descriptor.tool_names),
            messages: history.messages,
            compaction_checkpoint_count: history.checkpoints.len(),
        }))
    }
}
