//! Shared logical same-successor fixture, never a production authority source.
use super::runtime;
use storage::recovery::restart::tools::ToolSnapshot;
use storage::recovery::restart::*;
use storage::{Database, StorageError, StorageHandle};
fn fixture_error(e: impl ToString) -> StorageError {
    StorageError::Io(e.to_string())
}
pub fn inherited_fixture_proof(
    db: &Database,
    handle: &StorageHandle,
) -> Result<Option<core_instance::InheritedHandoffProof>, StorageError> {
    inherited_fixture_proof_with_metadata(db, handle, fixture_restart_metadata, |_, _| Ok(None))
}
pub fn inherited_fixture_proof_with_metadata(
    db: &Database,
    handle: &StorageHandle,
    metadata: impl FnOnce(&str) -> RestartMetadata,
    mut snapshot: impl FnMut(
        &RestartExpectedMember,
        &RestartMetadata,
    ) -> Result<Option<ToolSnapshot>, StorageError>,
) -> Result<Option<core_instance::InheritedHandoffProof>, StorageError> {
    let mut transactions = db.restart_transactions().map_err(fixture_error)?;
    let tx = if let Some(tx) = transactions.pop() {
        if tx.phase.terminal() {
            return Ok(None);
        }
        tx
    } else {
        let manifests = db
            .recovery_manifests()
            .map_err(fixture_error)?
            .into_iter()
            .filter_map(|m| m.record.ok())
            .filter(|m| m.committed && !m.superseded)
            .collect::<Vec<_>>();
        if manifests.is_empty() {
            return Ok(None);
        }
        let id = format!("fixture/{}", manifests[0].manifest_id);
        let m = metadata(&id);
        let mut tx = handle
            .mutate_restart(id.clone(), None, RestartMutation::Create(Box::new(m)))
            .map_err(fixture_error)?;
        let mut expected = Vec::new();
        for manifest in &manifests {
            for member in &manifest.members {
                let r: runtime::quiesce::RunRecoveryRecord =
                    serde_json::from_str(&member.record_json).map_err(fixture_error)?;
                expected.push(RestartExpectedMember {
                    run_id: member.run_id.clone(),
                    incarnation: member.incarnation.clone(),
                    root_run_id: r.root.to_string(),
                    thread_id: r.thread,
                    intent_revision: member.intent_revision,
                    snapshot: member.snapshot.clone(),
                    owner_id: None,
                    owner_generation: None,
                    lane_id: r.recipe.lane.map(|_| r.root.to_string()),
                });
            }
        }
        tx = handle
            .mutate_restart(
                id.clone(),
                Some(tx.revision),
                RestartMutation::BeginCapture(expected),
            )
            .map_err(fixture_error)?;
        for m in manifests {
            tx = handle
                .mutate_restart(
                    id.clone(),
                    Some(tx.revision),
                    RestartMutation::BindComponent {
                        manifest_id: m.manifest_id,
                    },
                )
                .map_err(fixture_error)?;
        }
        for member in tx.expected.clone() {
            let Some(evidence) = snapshot(&member, &tx.metadata)? else {
                continue;
            };
            tx = handle
                .mutate_restart(
                    id.clone(),
                    Some(tx.revision),
                    RestartMutation::CaptureTools {
                        run_id: member.run_id,
                        incarnation: member.incarnation,
                        snapshot: evidence,
                    },
                )
                .map_err(fixture_error)?;
        }
        tx = handle
            .mutate_restart(id.clone(), Some(tx.revision), RestartMutation::Commit)
            .map_err(fixture_error)?;
        handle
            .mutate_restart(
                id,
                Some(tx.revision),
                RestartMutation::Arm {
                    nonce_hash: "fixture-nonce".into(),
                },
            )
            .map_err(fixture_error)?
    };

    Ok(Some(core_instance::test_support::inherited_handoff_proof(
        tx.metadata.restart_id,
        tx.metadata.config_identity,
        tx.metadata.successor_epoch,
        tx.ticket_nonce_hash.unwrap_or_default(),
        tx.metadata.successor_build,
    )))
}
pub fn fixture_restart_metadata(id: &str) -> RestartMetadata {
    RestartMetadata {
        restart_id: id.into(),
        capture_id: format!("fixture-capture/{id}"),
        config_identity: "fixture-config".into(),
        old_epoch: 1,
        successor_epoch: 2,
        old_build: "fixture-build".into(),
        successor_build: "fixture-build".into(),
        target_identity: "fixture-target".into(),
        handoff_version: 1,
        snapshot_version: 1,
        storage_version: 62,
        old_builtin_tool_names: Default::default(),
        old_compiled_catalog_fingerprint: None,
        new_compiled_catalog_fingerprint: None,
        old_effective_tools_fingerprint: None,
        new_effective_tools_fingerprint: None,
    }
}
