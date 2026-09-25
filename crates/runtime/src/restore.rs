//! 終端コンテキストの保存形式と保存境界。

use std::time::{SystemTime, UNIX_EPOCH};

use agents::NetworkAccess;
use event_bus::AgentRunPhase;
use serde::{Deserialize, Serialize};
use storage::{RunContextRecord, StorageError};

use crate::agent_loop::LoopState;
use crate::{CoordinationTopology, ModelPreference, RunId, WorkspaceMode};

mod diagnostics;
mod recovery;
pub use diagnostics::RunRestoreDiagnostics;

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
            || safe_context_end(&messages) != messages.len()
            || checkpoints.iter().any(|checkpoint| {
                checkpoint.range.0 >= checkpoint.range.1 || checkpoint.range.1 > messages.len()
            })
        {
            return Err(fail("context range".into()));
        }
        let descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).map_err(|error| fail(error.to_string()))?;
        if descriptor.has_uncertain_effects() {
            return Err(crate::RuntimeError::RunRestoreFailed {
                run_id: record.run_id.clone(),
                reason: crate::RunRestoreFailure::UnsupportedConfig(
                    recovery::UNRESOLVED_TOOL_CALLS_REASON.into(),
                ),
            });
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

/// A team root may reuse history only with an explicit current team authority.
pub(crate) const TEAM_RENEWAL_REQUIRED_REASON: &str = "current_team_authority_required";
pub(crate) const ROOT_CONTEXT_RENEWAL_REQUIRED_REASON: &str =
    "current_root_context_authority_required";

/// Identity only: no database path, writer, permit, or lease is restored from disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamRestoreIdentity {
    pub team_id: String,
    pub coordinator_run_id: RunId,
}

/// Calls with uncertain effects or in an incomplete batch. Inputs are excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptedToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub result_observed: bool,
    /// Based on the tool's registered permissions at dispatch; unknown/meta writes fail closed.
    pub may_have_side_effects: bool,
}

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
    #[serde(default)]
    pub renewable_team: Option<TeamRestoreIdentity>,
    #[serde(default)]
    pub interrupted_tool_calls: Vec<InterruptedToolCall>,
    #[serde(default)]
    pub durable_task_id: Option<String>,
}

impl RunRestoreDescriptor {
    pub(crate) fn has_uncertain_effects(&self) -> bool {
        self.interrupted_tool_calls
            .iter()
            .any(|call| call.may_have_side_effects)
    }

    /// ownership のみが復元不可の理由である記録かを返す。
    ///
    /// chat 系入口は ownership をスナップショットから復元しないため、
    /// この条件を満たす記録は `restorable` / `non_restorable_reason` に
    /// かかわらず history 復元を許可してよい。
    pub(crate) fn renewable_ownership_only(&self) -> bool {
        !self.has_uncertain_effects()
            && self.non_restorable_reason.as_deref() == Some(OWNERSHIP_ONLY_UNRESTORABLE_REASON)
    }

    pub(crate) fn renewable_root_context(&self) -> bool {
        !self.has_uncertain_effects()
            && self.parent_run_id.is_none()
            && self.renewable_team.is_none()
            && self.non_restorable_reason.as_deref() == Some(ROOT_CONTEXT_RENEWAL_REQUIRED_REASON)
    }

    pub(crate) fn renewable_team_root(&self) -> bool {
        !self.has_uncertain_effects()
            && self.parent_run_id.is_none()
            && self.role == agents::Role::Orchestrator.name()
            && self.renewable_team.is_some()
            && self.non_restorable_reason.as_deref() == Some(TEAM_RENEWAL_REQUIRED_REASON)
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

/// Record uncertain tool effects before dispatch. A failed write must prevent dispatch.
/// Recovery never repeats an incomplete batch. Current-authority chat entrances
/// retain history and report missing outcomes as errors on the next turn.
pub(crate) fn persist_tool_intent(state: &LoopState) -> Result<(), SnapshotError> {
    let end = safe_context_end(&state.context.messages);
    if end == 0 || end == state.context.messages.len() {
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
        (
            config.purpose != crate::RunPurpose::General,
            "learning_purpose",
        ),
        (config.ownership.is_some(), "ownership"),
        (config.workspace_branch.is_some(), "workspace_branch"),
    ] {
        if present {
            unsupported.push(field);
        }
    }
    let renewable_team = match (&config.team, &config.team_store) {
        (Some(team), Some(store))
            if state.task.parent.is_none()
                && state.run_role() == agents::Role::Orchestrator
                && config.topology.worker_limit().is_some()
                && team.coordinator == state.caller_run_id()
                && team.id == store.id
                && unsupported.iter().all(|field| {
                    matches!(
                        *field,
                        "topology"
                            | "team"
                            | "team_store"
                            | "delegation_value"
                            | "ownership"
                            | "memory"
                            | "finding_store"
                    )
                }) =>
        {
            Some(TeamRestoreIdentity {
                team_id: team.id.clone(),
                coordinator_run_id: team.coordinator,
            })
        }
        _ => None,
    };
    let mut interrupted_tool_calls = interrupted_tool_calls(&state.context.messages[end..]);
    for call in cancelled_tool_calls(&state.context.messages[..end]) {
        if !interrupted_tool_calls
            .iter()
            .any(|pending| pending.call_id == call.call_id)
        {
            interrupted_tool_calls.push(call);
        }
    }
    if state
        .shared
        .executor
        .has_unobserved_shell_jobs(&state.caller_run_id().to_string())
    {
        interrupted_tool_calls.push(InterruptedToolCall {
            call_id: "unobserved-shell-jobs".into(),
            tool_name: "shell".into(),
            result_observed: false,
            may_have_side_effects: true,
        });
    }
    for call in &mut interrupted_tool_calls {
        call.may_have_side_effects =
            tool_may_have_side_effects(&state.shared.executor, &call.tool_name);
    }
    // Tool uncertainty is diagnostic history, not an execution configuration.
    // Preserve the independent authority-renewal reason even for interrupted runs.
    let non_restorable_reason = if renewable_team.is_some() {
        Some(TEAM_RENEWAL_REQUIRED_REASON.into())
    } else if state.task.parent.is_none()
        && unsupported
            .iter()
            .any(|field| matches!(*field, "memory" | "finding_store"))
        && unsupported
            .iter()
            .all(|field| matches!(*field, "memory" | "finding_store" | "ownership"))
    {
        Some(ROOT_CONTEXT_RENEWAL_REQUIRED_REASON.into())
    } else {
        match unsupported.as_slice() {
            [] => None,
            ["ownership"] => Some(OWNERSHIP_ONLY_UNRESTORABLE_REASON.to_owned()),
            _ => Some(format!("復元対象外の実行状態: {}", unsupported.join(", "))),
        }
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
        restorable: non_restorable_reason.is_none(),
        non_restorable_reason,
        renewable_team,
        interrupted_tool_calls,
        durable_task_id: config.task_id.clone(),
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

fn interrupted_tool_calls(messages: &[providers::Message]) -> Vec<InterruptedToolCall> {
    let results: std::collections::HashSet<&str> = messages
        .iter()
        .flat_map(|message| {
            message.content.iter().filter_map(|block| match block {
                providers::ContentBlock::ToolResult { tool_call_id, .. } => {
                    Some(tool_call_id.as_str())
                }
                _ => None,
            })
        })
        .collect();
    messages
        .iter()
        .flat_map(|message| {
            message.content.iter().filter_map(|block| match block {
                providers::ContentBlock::ToolUse { id, name, .. } => Some(InterruptedToolCall {
                    call_id: id.clone(),
                    tool_name: name.clone(),
                    result_observed: results.contains(id.as_str()),
                    may_have_side_effects: true,
                }),
                _ => None,
            })
        })
        .collect()
}

fn tool_may_have_side_effects(executor: &tools::ToolExecutor, name: &str) -> bool {
    if let Some(permissions) = executor.tool_permissions(name) {
        return permissions.fs_write || permissions.process_spawn || permissions.network;
    }
    !matches!(
        name,
        "list_agents"
            | "inspect_agent"
            | "run_output"
            | "skill_load"
            | "wait"
            | "wait_reply"
            | "inbox"
            | "ledger_read"
            | "compact"
    )
}

// Runtime cancellation produces a protocol-complete error result, but cannot
// establish whether a process had already modified files before it was stopped.
fn cancelled_tool_calls(messages: &[providers::Message]) -> Vec<InterruptedToolCall> {
    let cancelled: std::collections::HashSet<&str> = messages
        .iter()
        .flat_map(|message| {
            message.content.iter().filter_map(|block| match block {
                providers::ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error: true,
                } if content.iter().any(|part| {
                    matches!(part,
                        providers::ToolResultContent::Text { text } if text == "cancelled"
                    )
                }) =>
                {
                    Some(tool_call_id.as_str())
                }
                _ => None,
            })
        })
        .collect();
    interrupted_tool_calls(messages)
        .into_iter()
        .filter(|call| cancelled.contains(call.call_id.as_str()))
        .collect()
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
