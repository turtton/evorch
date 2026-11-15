//! Durable capture tests use synchronous writer faults and explicit safe points.
use super::fixture::Fixture;
use super::*;
use crate::{Role, RunConfig, RuntimeError};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;
use storage::recovery::restart::*;
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
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    let transaction = create(f);
    f.runtime
        .begin_restart_capture_session(fence, &transaction, recipes)
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
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
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
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
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
