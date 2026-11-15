//! Durable capture tests use synchronous writer faults and explicit safe points.
use super::fixture::Fixture;
use super::*;
use crate::{Role, RunConfig, RuntimeError};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;
use storage::recovery::restart::*;
mod handoff;
fn metadata() -> RestartMetadata {
    RestartMetadata {
        restart_id: "restart".into(),
        capture_id: "capture".into(),
        config_identity: "config".into(),
        old_epoch: 1,
        successor_epoch: 2,
        old_build: "old".into(),
        successor_build: "new".into(),
        target_identity: "target".into(),
        handoff_version: 1,
        snapshot_version: 1,
        storage_version: 62,
        old_builtin_tool_names: BTreeSet::from(["read".into()]),
        old_compiled_catalog_fingerprint: None,
        new_compiled_catalog_fingerprint: None,
        old_effective_tools_fingerprint: None,
        new_effective_tools_fingerprint: None,
    }
}
fn recipe() -> RecoveryRecipe {
    RecoveryRecipe {
        settings_reference: CORE_RECOVERY_SETTINGS_REFERENCE.into(),
        policy: ResumePolicy::CoreOwned,
        goal_id: None,
        team: None,
        lane: None,
        descriptor: None,
    }
}
fn db(f: &Fixture) -> storage::Database {
    storage::Database::open(&f.config).unwrap()
}
fn tx(f: &Fixture) -> RestartTransaction {
    db(f).restart_transaction("restart").unwrap().unwrap()
}
fn create(f: &Fixture) -> RestartTransaction {
    f._storage
        .handle()
        .mutate_restart(
            "restart".into(),
            None,
            RestartMutation::Create(Box::new(metadata())),
        )
        .unwrap()
}
fn chat(f: &Fixture) -> RunId {
    f.runtime
        .delegate_chat(
            "thread",
            Role::Orchestrator,
            "park".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap()
}
async fn session(
    f: &Fixture,
    recipes: BTreeMap<RunId, RecoveryRecipe>,
) -> crate::RestartCaptureSession {
    f.runtime.begin_planned_restart_quiesce(&recipes).unwrap();
    // Complete an explicitly observed provider call so the real run reaches its
    // next safe point; no elapsed-time assumption or extra provider turn.
    f.model.release.add_permits(1);
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    let gate = HostRestartGate::default();
    assert!(gate.transition(1, HostRestartPhase::Sealing));
    let sealed = gate.sealed(&f.runtime, 1, fence).await.unwrap();
    let transaction = create(f);
    sealed
        .begin_capture(&f.runtime, &transaction, recipes)
        .unwrap()
}
#[tokio::test]
async fn restart_zero_member_capture_has_unique_id_and_readback_not_absent_manifest() {
    let f = Fixture::new(0);
    let mut s = session(&f, BTreeMap::new()).await;
    f.runtime.persist_restart_capture_expected(&mut s).unwrap();
    let records = f.runtime.read_restart_capture_records(&s).unwrap();
    assert!(records.is_empty());
    let saved = f.runtime.commit_restart_capture(&mut s).unwrap();
    assert_eq!(saved.phase, RestartPhase::Committed);
    assert!(saved.expected.is_empty());
    assert!(saved.manifest_digest.is_some());
    assert_eq!(tx(&f), saved);
    assert!(
        f.runtime
            .cancel_restart_quiesce(&f.runtime.begin_restart_quiesce().unwrap())
            .is_err()
    );
    f.runtime
        .fence_dispatch_shutdown(|| async { Ok::<_, String>(()) })
        .await
        .unwrap();
}
#[tokio::test]
async fn restart_missing_live_row_is_durable_expected_evidence_and_invalidates_strict_capture() {
    let f = Fixture::new(0);
    let run = chat(&f);
    f.model.entered.notified().await;
    let mut s = session(&f, BTreeMap::from([(run, recipe())])).await;
    let saved_intent = f.intent(run);
    rusqlite::Connection::open(&f.config.db_path)
        .unwrap()
        .execute(
            "DELETE FROM resume_intents WHERE run_id=?1",
            [run.to_string()],
        )
        .unwrap();
    f.runtime.persist_restart_capture_expected(&mut s).unwrap();
    assert_eq!(tx(&f).expected.len(), 1);
    assert_eq!(tx(&f).expected[0].run_id, run.to_string());
    assert!(f.runtime.read_restart_capture_records(&s).is_err());
    assert_eq!(tx(&f).phase, RestartPhase::Invalidated);
    assert!(!f.runtime.shared.dispatch_tails.capture_valid());
    assert!(matches!(
        f.runtime.quiesce_state(),
        QuiesceState::Failed { .. }
    ));
    assert_eq!(
        f.model.calls.load(Ordering::SeqCst),
        1,
        "capture adds no provider call"
    );
    // Restore only the fixture's row so explicit shutdown can retire it normally.
    f._storage
        .handle()
        .save_resume_intent(
            storage::resume_intent::ResumeIntentRecord {
                run_id: run.to_string(),
                incarnation: saved_intent.incarnation.clone(),
                payload: serde_json::to_string(&saved_intent).unwrap(),
            },
            None,
        )
        .unwrap();
    f.cleanup(run).await;
    f.runtime
        .fence_dispatch_shutdown(|| async { Ok::<_, String>(()) })
        .await
        .unwrap();
}
#[tokio::test]
async fn restart_capture_rejects_stop_without_receipt_and_invalidation_survives_terminal_save_failure()
 {
    let f = Fixture::new(0);
    let run = chat(&f);
    f.model.entered.notified().await;
    let mut s = session(&f, BTreeMap::from([(run, recipe())])).await;
    f.runtime.persist_restart_capture_expected(&mut s).unwrap();
    let before = f.intent(run);
    assert!(matches!(
        f.runtime.stop(run, crate::StopScope::SelfOnly),
        Err(RuntimeError::RestartBusy)
    ));
    assert_eq!(f.intent(run), before);
    assert_eq!(tx(&f).phase, RestartPhase::Capturing);
    let sql = rusqlite::Connection::open(&f.config.db_path).unwrap();
    sql.execute_batch("CREATE TRIGGER fail_terminal_save BEFORE UPDATE ON resume_intents BEGIN SELECT RAISE(ABORT,'terminal save failed'); END;").unwrap();
    f.runtime.shared.quiesce.fail("owner was lost");
    // The terminal interrupt is accepted, but its durable save fails separately.
    f.runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    assert_eq!(
        f.intent(run),
        before,
        "terminal persistence did not succeed"
    );
    assert_eq!(
        tx(&f).phase,
        RestartPhase::Invalidated,
        "independent invalidation wins despite terminal save failure"
    );
    assert!(!f.runtime.shared.dispatch_tails.capture_valid());
    assert_eq!(
        f.model.calls.load(Ordering::SeqCst),
        1,
        "capture adds no provider call"
    );
    sql.execute_batch("DROP TRIGGER fail_terminal_save")
        .unwrap();
    let _ = f
        .runtime
        .fence_dispatch_shutdown(|| async { Ok::<_, String>(()) })
        .await;
    f.cleanup(run).await;
}
#[tokio::test]
async fn restart_planned_admission_rejects_retryable_call_but_preserves_internal_queue() {
    let f = Fixture::new(0);
    let recipes = BTreeMap::new();
    let attempt = f.runtime.begin_planned_restart_quiesce(&recipes).unwrap();
    assert!(matches!(
        f.runtime.delegate_chat(
            "external",
            Role::Orchestrator,
            "retry".into(),
            RunConfig::default()
        ),
        Err(RuntimeError::RestartBusy)
    ));
    let internal = f.start();
    assert!(f.runtime.preflight_restart_capture(&recipes).is_err());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        f.intent(internal).wait,
        ResumeWait::AdmissionQueued
    ));
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    f.model.entered.notified().await;
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    assert!(f.runtime.execution_snapshot(internal).is_ok());
    f.cleanup(internal).await;
}
#[tokio::test]
async fn restart_unsupported_preflight_does_not_park_existing_execution() {
    let f = Fixture::new(0);
    let run = f.start();
    let recipes = BTreeMap::from([(run, recipe())]);
    assert!(f.runtime.begin_planned_restart_quiesce(&recipes).is_err());
    assert!(f.runtime.shared.quiesce.running());
    f.model.entered.notified().await;
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    f.cleanup(run).await;
}

#[tokio::test]
async fn restart_quarantined_corrupt_transaction_does_not_block_new_attempt() {
    for damage in ["expected_digest='corrupt'", "metadata_json='{'"] {
        let f = Fixture::new(0);
        let tx = create(&f);
        f._storage
            .handle()
            .mutate_restart(
                "restart".into(),
                Some(tx.revision),
                RestartMutation::BeginCapture(vec![RestartExpectedMember {
                    run_id: "excluded".into(),
                    incarnation: "old".into(),
                    root_run_id: "excluded".into(),
                    thread_id: None,
                    intent_revision: 0,
                    snapshot: None,
                    owner_id: None,
                    owner_generation: None,
                    lane_id: None,
                }]),
            )
            .unwrap();
        rusqlite::Connection::open(&f.config.db_path)
            .unwrap()
            .execute_batch(&format!("UPDATE restart_transactions SET {damage}"))
            .unwrap();
        f.runtime.quarantine_cold_restarts().unwrap();
        assert!(db(&f).cold_restart_candidates().unwrap().is_empty());
        assert!(
            db(&f)
                .restart_excluded_run_ids()
                .unwrap()
                .contains("excluded")
        );
        let attempt = f
            .runtime
            .begin_planned_restart_quiesce(&BTreeMap::new())
            .unwrap();
        f.runtime.cancel_restart_quiesce(&attempt).unwrap();
        assert!(f.runtime.shared.quiesce.running());
        let mut next = metadata();
        next.restart_id = "next".into();
        next.capture_id = "capture-next".into();
        let saved = f
            ._storage
            .handle()
            .mutate_restart("next".into(), None, RestartMutation::Create(Box::new(next)))
            .unwrap();
        assert_eq!(saved.phase, RestartPhase::Preparing);
    }
}
fn armed(f: &Fixture) -> RestartTransaction {
    let mut tx = create(f);
    for mutation in [
        RestartMutation::BeginCapture(vec![]),
        RestartMutation::Commit,
        RestartMutation::Arm {
            nonce_hash: "nonce".into(),
        },
    ] {
        tx = f
            ._storage
            .handle()
            .mutate_restart("restart".into(), Some(tx.revision), mutation)
            .unwrap();
    }
    tx
}
fn proof() -> core_instance::InheritedHandoffProof {
    core_instance::test_support::inherited_handoff_proof(
        "restart".into(),
        "config".into(),
        2,
        "nonce".into(),
        "new".into(),
    )
}
#[tokio::test]
async fn restart_adoption_reads_back_lost_ack_and_retains_proof_on_rollback() {
    for after_commit in [false, true] {
        let f = Fixture::new(0);
        let before = armed(&f);
        f._storage
            .handle()
            .inject_restart_write_fault(after_commit)
            .unwrap();
        let result = f.runtime.adopt_restart_handoff(proof());
        if after_commit {
            result.unwrap();
        } else {
            let failure = result.unwrap_err();
            assert_eq!(tx(&f), before);
            assert!(!f.runtime.has_planned_restart_handoff());
            assert_eq!(failure.proof.restart_id(), "restart");
            f.runtime.adopt_restart_handoff(failure.proof).unwrap();
        }
        assert!(f.runtime.has_planned_restart_handoff());
        assert_eq!(tx(&f).phase, RestartPhase::Adopted);
        assert_eq!(tx(&f).adopted_epoch, Some(2));
        assert_eq!(
            tx(&f).revision,
            before.revision + 1,
            "exactly one adoption committed"
        );
    }
}
#[tokio::test]
async fn restart_adoption_readback_rejects_changed_phase_epoch_nonce_and_identity() {
    for damage in [
        "phase='invalidated'",
        "adopted_epoch=99",
        "ticket_nonce_hash='foreign'",
        "metadata_json=json_set(metadata_json,'$.config_identity','foreign')",
        "metadata_json=json_set(metadata_json,'$.successor_build','foreign')",
    ] {
        let f = Fixture::new(0);
        armed(&f);
        let sql = rusqlite::Connection::open(&f.config.db_path).unwrap();
        sql.execute_batch(&format!("CREATE TRIGGER change_adoption AFTER UPDATE OF phase ON restart_transactions WHEN NEW.phase='adopted' BEGIN UPDATE restart_transactions SET {damage} WHERE restart_id=NEW.restart_id; END;")).unwrap();
        f._storage
            .handle()
            .inject_restart_write_fault(true)
            .unwrap();
        let failure = f.runtime.adopt_restart_handoff(proof()).unwrap_err();
        assert_eq!(failure.proof.restart_id(), "restart");
        assert!(!f.runtime.has_planned_restart_handoff());
    }
}
#[tokio::test]
async fn restart_preflight_rejects_missing_or_replaced_referenced_snapshot_before_parking() {
    for remove in [false, true] {
        let f = Fixture::new(0);
        let run = chat(&f);
        f.model.entered.notified().await;
        let intent = f.intent(run);
        assert!(
            intent
                .checkpoint
                .as_ref()
                .map(|c| &c.snapshot)
                .into_iter()
                .chain(intent.snapshot.as_ref())
                .any(|s| matches!(s, crate::checkpoint::SnapshotReference::RunContext { .. }))
        );
        let recipes = BTreeMap::from([(run, recipe())]);
        f.runtime.preflight_restart_capture(&recipes).unwrap();
        let sql = rusqlite::Connection::open(&f.config.db_path).unwrap();
        sql.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        if remove {
            sql.execute(
                "DELETE FROM run_contexts WHERE run_id=?1",
                [run.to_string()],
            )
            .unwrap();
        } else {
            sql.execute(
                "UPDATE run_contexts SET updated_at_ns=updated_at_ns+1 WHERE run_id=?1",
                [run.to_string()],
            )
            .unwrap();
        }
        assert!(f.runtime.begin_planned_restart_quiesce(&recipes).is_err());
        assert!(f.runtime.shared.quiesce.running());
        assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
        f.cleanup(run).await;
    }
}
#[tokio::test]
async fn restart_preflight_preserves_durable_snapshot_free_admission_queue() {
    let f = Fixture::with_admission(0, true);
    let run = chat(&f);
    f.model.admission_entered.notified().await;
    // Model an adopted durable queue: admission has not registered a context,
    // and the intent, rather than a RAM-only launch closure, owns its input.
    f.runtime
        .shared
        .quiesce
        .update(
            run,
            &f.intent(run).incarnation,
            f.runtime.shared.run_store.get(),
            |i| i.wait = ResumeWait::AdmissionQueued,
        )
        .unwrap();
    let intent = f.intent(run);
    assert!(matches!(intent.wait, ResumeWait::AdmissionQueued));
    assert!(intent.checkpoint.is_none() && intent.snapshot.is_none());
    assert!(db(&f).run_context(&run.to_string()).unwrap().is_none());
    let recipes = BTreeMap::from([(run, recipe())]);
    f.runtime.preflight_restart_capture(&recipes).unwrap();
    let attempt = f.runtime.begin_planned_restart_quiesce(&recipes).unwrap();
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    f.cleanup(run).await;
}
