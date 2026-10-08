//! Durable memo-only context for a handoff awaiting provider admission.
use super::{RunRestoreDescriptor, SnapshotError};
use crate::{AgentRuntime, Role, RunConfig, RunId, WorkspaceMode};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Workspace identity can be revalidated; an owner permit is always supplied
/// by the current host and is never reconstructed from this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingEscalation {
    pub source_run_id: RunId,
    pub workspace_run_id: Option<RunId>,
    pub workspace_branch: Option<String>,
    pub requires_ownership: bool,
    /// Host-authored text, kept separate from the worker's escalation memo.
    pub trusted_request: Option<String>,
}

pub(crate) fn persist_escalation_seed(
    runtime: &AgentRuntime,
    run_id: RunId,
    source_run_id: RunId,
    prompt: &str,
    config: &RunConfig,
    worktree: Option<&crate::workspace::OwnedWorktree>,
    requires_ownership: bool,
) -> Result<(), SnapshotError> {
    let Some(store) = runtime.shared.run_store.get() else {
        return Ok(());
    };
    let descriptor = RunRestoreDescriptor {
        role: Role::Orchestrator.name().into(),
        name: Some(format!(
            "chat:{}:{}",
            Role::Orchestrator.name(),
            event_bus::escalation_thread_id(&run_id.to_string())
        )),
        parent_run_id: None,
        interactive: config.interactive,
        keep_alive: config.keep_alive,
        category: None,
        load_skills: vec![],
        workspace_mode: config.workspace_mode,
        model_preference: None,
        restorable: true,
        non_restorable_reason: None,
        renewable_team: None,
        interrupted_tool_calls: vec![],
        durable_task_id: None,
        selected_model: None,
        tool_names: vec![],
        project_root: config.project_root.clone(),
        pending_escalation: Some(PendingEscalation {
            source_run_id,
            workspace_run_id: worktree.map(|owned| {
                owned
                    .run_name
                    .parse()
                    .expect("runtime worktree run identity")
            }),
            workspace_branch: worktree.map(|owned| owned.branch.clone()),
            requires_ownership,
            trusted_request: runtime.trusted_thread_request(source_run_id),
        }),
        completed_turn_end: None,
    };
    let messages = [providers::Message {
        role: providers::Role::User,
        content: vec![providers::ContentBlock::Text {
            text: prompt.into(),
        }],
    }];
    store
        .handle
        .upsert_run_context(&storage::RunContextRecord {
            run_id: run_id.to_string(),
            role: descriptor.role.clone(),
            name: descriptor.name.clone().expect("handoff name"),
            parent_run_id: None,
            config_json: serde_json::to_string(&descriptor)?,
            messages_json: serde_json::to_string(&messages)?,
            checkpoints_json: "[]".into(),
            terminal_phase: "Error".into(),
            restorable: true,
            updated_at_ns: i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?,
        })?;
    Ok(())
}

impl PendingEscalation {
    pub(crate) fn restore_worktree(
        &self,
        descriptor: &RunRestoreDescriptor,
    ) -> Result<Option<crate::workspace::OwnedWorktree>, String> {
        match (
            descriptor.workspace_mode,
            self.workspace_run_id,
            self.workspace_branch.as_deref(),
        ) {
            (WorkspaceMode::Shared, None, None) => Ok(None),
            (WorkspaceMode::Isolated, Some(run), Some(branch)) => {
                let root = descriptor
                    .project_root
                    .clone()
                    .ok_or("missing handoff project")?;
                crate::workspace::Project::new(root)
                    .and_then(|project| {
                        crate::workspace::WorktreeManager::new(project)
                            .open_existing_on_branch(run, branch)
                    })
                    .map(Some)
                    .map_err(|error| error.to_string())
            }
            _ => Err("inconsistent handoff workspace identity".into()),
        }
    }
}
