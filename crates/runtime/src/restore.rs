//! 終端コンテキストの保存形式と保存境界。

use std::time::{SystemTime, UNIX_EPOCH};

use agents::NetworkAccess;
use event_bus::AgentRunPhase;
use serde::{Deserialize, Serialize};
use storage::{RunContextRecord, StorageError};

use crate::agent_loop::LoopState;
use crate::{CoordinationTopology, ModelPreference, RunId, WorkspaceMode};

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

#[derive(Debug, thiserror::Error)]
pub(crate) enum SnapshotError {
    #[error("終端コンテキストを直列化できません: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("終端コンテキストを保存できません: {0}")]
    Storage(#[from] StorageError),
    #[error("保存時刻を取得できません: {0}")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("保存時刻が範囲外です: {0}")]
    Timestamp(#[from] std::num::TryFromIntError),
}

pub(crate) fn persist_terminal_snapshot(state: &LoopState) -> Result<(), SnapshotError> {
    let Some(runtime) = state.runtime() else {
        return Ok(());
    };
    let Some(store) = runtime.shared.run_store.get() else {
        return Ok(());
    };
    let terminal_phase = match state.run_state.phase() {
        AgentRunPhase::Done => "Done",
        AgentRunPhase::Error => "Error",
        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => return Ok(()),
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
        non_restorable_reason: (!unsupported.is_empty())
            .then(|| format!("復元対象外の実行状態: {}", unsupported.join(", "))),
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
        messages_json: serde_json::to_string(&state.context.messages)?,
        checkpoints_json: serde_json::to_string(state.context.checkpoints())?,
        terminal_phase: terminal_phase.to_string(),
        restorable: descriptor.restorable,
        updated_at_ns: i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?,
    };
    store.handle.upsert_run_context(&record)?;
    Ok(())
}
