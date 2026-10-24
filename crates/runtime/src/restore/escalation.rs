//! Durable memo-only context for a handoff awaiting provider admission.
use super::{RunRestoreDescriptor, SnapshotError};
use crate::{AgentRuntime, Role, RunConfig, RunId};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// An owner permit is always supplied by the current host and is never
/// reconstructed from this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingEscalation {
    pub source_run_id: RunId,
    pub requires_ownership: bool,
    /// Host-authored text, kept separate from the worker's escalation memo.
    pub trusted_request: Option<String>,
    /// Only writers retaining complete human input set this. Legacy snapshots
    /// may contain a truncated ID prefix and must never grant thread access.
    #[serde(default)]
    pub trusted_request_complete: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn persist_escalation_seed(
    runtime: &AgentRuntime,
    run_id: RunId,
    source_run_id: RunId,
    prompt: &str,
    config: &RunConfig,
    requires_ownership: bool,
    continuity: &crate::continuity::Continuity,
    pending_reference_id: Option<String>,
) -> Result<(), SnapshotError> {
    let Some(store) = runtime.shared.run_store.get() else {
        return Ok(());
    };
    let descriptor = RunRestoreDescriptor {
        admission_reservation_id: None,
        thread_id: Some(event_bus::escalation_thread_id(&run_id.to_string())),
        turn: None,
        boundary_pending: true,
        continuity: continuity.clone(),
        pending_reference_id,
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
        model_preference: None,
        restorable: true,
        non_restorable_reason: None,
        renewable_team: None,
        interrupted_tool_calls: vec![],
        outcome_notices_persisted: false,
        durable_task_id: None,
        selected_model: None,
        tool_names: vec![],
        project_root: config.project_root.clone(),
        pending_escalation: Some(PendingEscalation {
            source_run_id,
            requires_ownership,
            trusted_request: runtime.trusted_thread_request(source_run_id),
            trusted_request_complete: true,
        }),
        completed_turn_end: None,
        route_pins: vec![],
        allowed_directories: Default::default(),
        allowed_instructions: Default::default(),
    };
    let mut messages = vec![providers::Message {
        role: providers::Role::User,
        content: vec![providers::ContentBlock::Text {
            text: prompt.into(),
        }],
    }];
    let blocks = continuity.blocks(continuity.inherited_note.as_ref())?;
    if !blocks.is_empty() {
        messages.push(providers::Message {
            role: providers::Role::User,
            content: blocks,
        });
    }
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
