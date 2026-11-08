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
    /// Preserve source resource caps through committed admission recovery.
    #[serde(default)]
    pub budget: crate::budget_tracker::BudgetSettings,
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
) -> Result<u64, SnapshotError> {
    runtime.deny_deleted_run(&source_run_id.to_string())?;
    let Some(store) = runtime.shared.run_store.get() else {
        return Ok(0);
    };
    let descriptor = RunRestoreDescriptor {
        harness_notice_pending: false,
        lane_scope: config.lane_scope,
        spawn_credits: config.spawn_credits.clone(),
        responses_replay: None,
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
            budget: config.budget.clone(),
            trusted_request: runtime.trusted_thread_request(source_run_id),
            trusted_request_complete: true,
        }),
        completed_turn_end: None,
        route_pins: vec![],
        allowed_directories: Default::default(),
        allowed_instructions: Default::default(),
        loaded_skills: runtime.review_skill_refs_of(source_run_id),
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
    let mut provenance = super::provenance::ConversationProvenance::default();
    if messages.len() == 2 {
        provenance.continuity(&messages, continuity);
    }
    let record = storage::RunContextRecord {
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
        host_inputs_json: "[]".into(),
    };
    // An uncommitted preparation retry refreshes this seed under its current
    // snapshot CAS. Once responsibility commits the coordinator never rewrites it.
    let expected = store
        .restore_bundle(run_id)?
        .map_or(0, |bundle| bundle.snapshot_revision);
    if expected != 0
        && !store
            .handle
            .handoff_for_target(run_id.to_string())?
            .is_some_and(|receipt| !receipt.state.is_committed())
    {
        return Err(SnapshotError::Storage(storage::StorageError::StaleMutation));
    }
    let write = provenance.write(record, expected)?;
    let saved = store
        .handle
        .save_snapshot_bundle(&write)
        .map_err(super::provenance::storage_error)?;
    // Materialize authentic inputs, not old review decisions or a shared Arc.
    // Keep answer/reference context and ordering, but make every row belong to
    // the target so source retention cannot remove its authority evidence.
    let source = source_run_id.to_string();
    let target = run_id.to_string();
    let mut lineage = store.review_lineage_ancestors(&source)?;
    lineage.push(source);
    let mut evidence = store.review_evidence_for_lineage(&lineage)?;
    for row in &mut evidence {
        row.id = format!("handoff-evidence-{target}-{}", row.id);
        row.root_run_id.clone_from(&target);
        row.target_run_id.clone_from(&target);
    }
    // Legacy host seeds may predate the durable evidence ledger entirely.
    if evidence.is_empty()
        && let Some(request) = runtime.trusted_thread_request(source_run_id)
    {
        evidence.push(crate::run_store::evidence_row(
            &target,
            &target,
            "host_chat",
            &request,
        ));
    }
    store.handle.record_review_evidence(&evidence)?;
    Ok(saved.snapshot_revision)
}
