//! 終端コンテキストの保存形式と保存境界。

use std::time::{SystemTime, UNIX_EPOCH};

use agents::NetworkAccess;
use event_bus::AgentRunPhase;
use serde::{Deserialize, Serialize};
use storage::{RunContextRecord, StorageError};

use crate::agent_loop::LoopState;
use crate::{CoordinationTopology, ModelPreference, RunId, WorkspaceMode};

pub(crate) struct RestoredState {
    pub(crate) messages: Vec<providers::Message>,
    pub(crate) checkpoints: Vec<crate::CompactionCheckpoint>,
    pub(crate) trigger: Option<event_bus::AgentMessage>,
}

impl RestoredState {
    pub(crate) fn attach_to(
        self,
        mut task: crate::agent_loop::RunTask,
    ) -> crate::agent_loop::RunTask {
        task.restored = Some(self);
        task
    }

    pub(crate) fn from_record(record: &RunContextRecord) -> Result<Self, crate::RuntimeError> {
        let fail = |reason| crate::RuntimeError::RunRestoreFailed {
            run_id: record.run_id.clone(),
            reason: crate::RunRestoreFailure::CorruptContext(reason),
        };
        let messages: Vec<providers::Message> =
            serde_json::from_str(&record.messages_json).map_err(|error| fail(error.to_string()))?;
        let checkpoints: Vec<crate::CompactionCheckpoint> =
            serde_json::from_str(&record.checkpoints_json)
                .map_err(|error| fail(error.to_string()))?;
        if messages.is_empty()
            || checkpoints.iter().any(|checkpoint| {
                checkpoint.range.0 >= checkpoint.range.1 || checkpoint.range.1 > messages.len()
            })
        {
            return Err(fail("context range".into()));
        }
        Ok(Self {
            messages,
            checkpoints,
            trigger: None,
        })
    }
}

/// ownership だけを理由に復元不可とした記録へ書き込むマーカー。
///
/// GUI の chat 系入口 (`delegate_chat` / `continue_goal`) はスナップショットから
/// ownership を復元せず、呼び出し側の現在の permit を常に再付与するため、
/// このマーカーを持つ記録は history 復元を許可する。復元判定の両ゲートと
/// `write_snapshot` は必ずこの定数を経由して文字列を一致させること。
pub(crate) const OWNERSHIP_ONLY_UNRESTORABLE_REASON: &str = "復元対象外の実行状態: ownership";

/// 非直列化の実行権限を含まない復元用設定。拒否理由も snapshot に残す。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRestoreDescriptor {
    pub role: String,
    pub name: Option<String>,
    pub parent_run_id: Option<RunId>,
    pub interactive: bool,
    pub keep_alive: bool,
    pub category: Option<String>,
    pub load_skills: Vec<String>,
    pub workspace_mode: WorkspaceMode,
    pub network_access: NetworkAccess,
    pub model_preference: Option<ModelPreference>,
    pub restorable: bool,
    pub non_restorable_reason: Option<String>,
}

impl RunRestoreDescriptor {
    /// ownership のみが復元不可の理由である記録かを返す。
    ///
    /// chat 系入口は ownership をスナップショットから復元しないため、
    /// この条件を満たす記録は `restorable` / `non_restorable_reason` に
    /// かかわらず history 復元を許可してよい。
    pub(crate) fn renewable_ownership_only(&self) -> bool {
        self.non_restorable_reason.as_deref() == Some(OWNERSHIP_ONLY_UNRESTORABLE_REASON)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SnapshotError {
    #[error("復元コンテキストを直列化できません: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("復元コンテキストを保存できません: {0}")]
    Storage(#[from] StorageError),
    #[error("保存時刻を取得できません: {0}")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("保存時刻が範囲外です: {0}")]
    Timestamp(#[from] std::num::TryFromIntError),
}

/// Persist only at a completed tool round; a failed write leaves the last snapshot intact.
pub(crate) fn persist_checkpoint(state: &LoopState) -> Result<(), SnapshotError> {
    let end = safe_context_end(&state.context.messages);
    if end == 0 || end != state.context.messages.len() {
        return Ok(());
    }
    write_snapshot(state, "Checkpoint", end)
}

pub(crate) fn persist_terminal_snapshot(state: &LoopState) -> Result<(), SnapshotError> {
    let phase = match state.run_state.phase() {
        AgentRunPhase::Done => "Done",
        AgentRunPhase::Error => "Error",
        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => return Ok(()),
    };
    let end = safe_context_end(&state.context.messages);
    if end == 0 {
        return Ok(());
    }
    write_snapshot(state, phase, end)
}

/// A pending batch is omitted as a whole, so restored history never asks a provider
/// to accept tool calls whose results were lost when the run was interrupted.
fn safe_context_end(messages: &[providers::Message]) -> usize {
    let mut pending = std::collections::HashSet::new();
    let mut end = 0;
    for (index, message) in messages.iter().enumerate() {
        for block in &message.content {
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
        if pending.is_empty() {
            end = index + 1;
        }
    }
    end
}

fn write_snapshot(
    state: &LoopState,
    terminal_phase: &str,
    end: usize,
) -> Result<(), SnapshotError> {
    let Some(runtime) = state.runtime() else {
        return Ok(());
    };
    let Some(store) = runtime.shared.run_store.get() else {
        return Ok(());
    };
    let config = state.run_config();
    let mut unsupported = Vec::new();
    match config.topology {
        CoordinationTopology::Single => {}
        CoordinationTopology::DynamicTeam { .. } => unsupported.push("topology"),
    }
    for (present, field) in [
        (config.team.is_some(), "team"),
        (config.team_task.is_some(), "team_task"),
        (config.team_store.is_some(), "team_store"),
        (config.finding_store.is_some(), "finding_store"),
        (config.delegation_value.is_some(), "delegation_value"),
        (config.memory.is_some(), "memory"),
        (config.learning_internal, "learning_internal"),
        (config.ownership.is_some(), "ownership"),
        (config.workspace_branch.is_some(), "workspace_branch"),
    ] {
        if present {
            unsupported.push(field);
        }
    }
    let non_restorable_reason = match unsupported.as_slice() {
        [] => None,
        ["ownership"] => Some(OWNERSHIP_ONLY_UNRESTORABLE_REASON.to_owned()),
        _ => Some(format!("復元対象外の実行状態: {}", unsupported.join(", "))),
    };
    let descriptor = RunRestoreDescriptor {
        role: state.run_role().name().to_string(),
        name: config.name.clone(),
        parent_run_id: state.task.parent,
        interactive: config.interactive,
        keep_alive: config.keep_alive,
        category: config.category.clone(),
        load_skills: config.load_skills.clone(),
        workspace_mode: config.workspace_mode,
        network_access: config.network_access,
        model_preference: state.channels.model_preference_rx.borrow().clone(),
        restorable: unsupported.is_empty(),
        non_restorable_reason,
    };
    let record = RunContextRecord {
        run_id: state.caller_run_id().to_string(),
        role: descriptor.role.clone(),
        name: descriptor
            .name
            .clone()
            .unwrap_or_else(|| descriptor.role.clone()),
        parent_run_id: descriptor.parent_run_id.map(|id| id.to_string()),
        config_json: serde_json::to_string(&descriptor)?,
        messages_json: serde_json::to_string(&state.context.messages[..end])?,
        checkpoints_json: serde_json::to_string(
            &state
                .context
                .checkpoints()
                .iter()
                .filter(|checkpoint| checkpoint.range.1 <= end)
                .collect::<Vec<_>>(),
        )?,
        terminal_phase: terminal_phase.to_string(),
        restorable: descriptor.restorable,
        updated_at_ns: i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?,
    };
    store.handle.upsert_run_context(&record)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::safe_context_end;
    use providers::{ContentBlock, Message, Role, ToolResultContent};

    #[test]
    fn incomplete_tool_batches_are_not_checkpoint_boundaries() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "work".into(),
            }],
        }];
        assert_eq!(safe_context_end(&messages), 1);
        messages.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                },
                ContentBlock::ToolUse {
                    id: "b".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                },
            ],
        });
        let result = |id: &str| Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: id.into(),
                content: vec![ToolResultContent::Text {
                    text: "done".into(),
                }],
                is_error: false,
            }],
        };
        assert_eq!(safe_context_end(&messages), 1);
        messages.push(result("a"));
        assert_eq!(safe_context_end(&messages), 1);
        messages.push(result("b"));
        assert_eq!(safe_context_end(&messages), 4);
    }
}
