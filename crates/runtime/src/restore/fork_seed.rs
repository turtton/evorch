//! A fork copies another run's history up to a completed turn. It never
//! inherits the source run's execution, so only identity and authority gates apply.
use serde::{Deserialize, Serialize};
use storage::RunContextRecord;

use super::{RestoredState, RunRestoreDescriptor, conversation_end};

/// Completed-turn boundary of another chat run, used only while the forked
/// conversation has no saved context of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChatForkSeed {
    pub source_run_id: String,
    /// Non-system message count published by `TurnCompleted`.
    pub context_len: u64,
}

impl RestoredState {
    /// Truncate a saved root chat context at a published turn boundary.
    pub(crate) fn for_fork_seed(
        record: &RunContextRecord,
        context_len: u64,
    ) -> Result<Self, crate::RuntimeError> {
        let corrupt = |reason: String| crate::RuntimeError::RunRestoreFailed {
            run_id: record.run_id.clone(),
            reason: crate::RunRestoreFailure::CorruptContext(reason),
        };
        let unsupported = |reason: String| crate::RuntimeError::RunRestoreFailed {
            run_id: record.run_id.clone(),
            reason: crate::RunRestoreFailure::UnsupportedConfig(reason),
        };
        let mut descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json)
            .map_err(|error| corrupt(error.to_string()))?;
        if record.parent_run_id.is_some()
            || descriptor.parent_run_id.is_some()
            || descriptor.renewable_team.is_some()
        {
            return Err(unsupported(
                "fork_seed: only root chat history can be forked".into(),
            ));
        }
        // A consumed snapshot still holds valid history; the fork never resumes it.
        let history = descriptor.conversation_descriptor();
        let accepted = (record.restorable && history.restorable)
            || history.renewable_ownership_only()
            || history.renewable_root_context()
            || history.non_restorable_reason.as_deref() == Some("snapshot_consumed");
        if !accepted {
            return Err(unsupported(
                history
                    .non_restorable_reason
                    .unwrap_or_else(|| "fork_seed".into()),
            ));
        }
        let messages: Vec<providers::Message> = serde_json::from_str(&record.messages_json)
            .map_err(|error| corrupt(error.to_string()))?;
        let end = usize::try_from(context_len)
            .ok()
            .filter(|len| *len > 0)
            .and_then(|len| conversation_end(&messages, len))
            .ok_or_else(|| corrupt("fork seed boundary is outside the saved history".into()))?;
        let ends_turn = messages[end - 1].role == providers::Role::Assistant
            && !messages[end - 1]
                .content
                .iter()
                .any(|block| matches!(block, providers::ContentBlock::ToolUse { .. }));
        if !ends_turn {
            return Err(corrupt("fork seed boundary does not end a turn".into()));
        }
        let checkpoints: Vec<crate::CompactionCheckpoint> =
            serde_json::from_str(&record.checkpoints_json)
                .map_err(|error| corrupt(error.to_string()))?;
        // Interrupted calls belong to a batch after the boundary.
        descriptor.interrupted_tool_calls.clear();
        let mut truncated = record.clone();
        truncated.messages_json =
            serde_json::to_string(&messages[..end]).map_err(|error| corrupt(error.to_string()))?;
        truncated.checkpoints_json = serde_json::to_string(
            &checkpoints
                .into_iter()
                .filter(|checkpoint| checkpoint.range.1 <= end)
                .collect::<Vec<_>>(),
        )
        .map_err(|error| corrupt(error.to_string()))?;
        truncated.config_json =
            serde_json::to_string(&descriptor).map_err(|error| corrupt(error.to_string()))?;
        Self::from_record(&truncated)
    }
}
