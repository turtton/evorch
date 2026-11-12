use super::{fixture::Fixture, *};
use crate::{AgentRuntime, Role, RunConfig, StopScope};
use event_bus::EventBus;
use std::{collections::BTreeMap, sync::Arc, sync::atomic::Ordering};

async fn saved(f: &Fixture) -> RunId {
    let run = f.start();
    f.runtime.request_quiesce();
    f.runtime.wait_quiesced().await.unwrap();
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    let recipe = RecoveryRecipe {
        settings_reference: "current-profile".into(),
        policy: ResumePolicy::CoreOwned,
        goal_id: None,
        team: None,
        lane: None,
        descriptor: None,
    };
    let records = f
        .runtime
        .capture_recovery_records(&BTreeMap::from([(run, recipe)]))
        .unwrap();
    f.runtime
        .save_recovery_graph("paused-test", &records)
        .unwrap();
    run
}
fn new_process(f: &Fixture) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(256));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
    )
    .with_run_store(RunStore::open(&f.config, f._storage.handle()).unwrap())
}
fn prepare(f: &Fixture, runtime: &AgentRuntime) -> crate::PausedRecoveryComponent {
    let root = f._dir.path().join("ownership");
    let leases =
        storage::shared_with_wait(&f.config.db_path, &root, std::time::Duration::ZERO).unwrap();
    let startup = leases
        .recovery_startup_complete(&f.config.db_path, &root)
        .unwrap();
    let plan = runtime.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 1, "{plan:?}");
    runtime
        .prepare_component(&plan.components[0], startup, &root)
        .unwrap()
}

#[tokio::test]
async fn prepare_and_drop_leave_exact_snapshot_and_pending_claim_without_execution() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let db = storage::Database::open(&f.config).unwrap();
    let before = db.run_context(&run.to_string()).unwrap().unwrap();
    let revision = db
        .resume_intent_revision(&run.to_string())
        .unwrap()
        .unwrap();
    let paused = prepare(&f, &runtime);
    assert!(runtime.has_paused_recovery());
    assert!(runtime.list_agents().is_empty());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    let after = db.run_context(&run.to_string()).unwrap().unwrap();
    assert_eq!(after.config_json, before.config_json);
    assert_eq!(after.messages_json, before.messages_json);
    assert_eq!(after.updated_at_ns, before.updated_at_ns);
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(revision + 1)
    );
    drop(paused);
    assert!(runtime.has_paused_recovery());
    assert_eq!(
        db.run_context(&run.to_string())
            .unwrap()
            .unwrap()
            .config_json,
        before.config_json
    );
    let plan = new_process(&f).recovery_plan().unwrap();
    assert!(plan.components.is_empty());
    assert!(
        plan.report
            .blocked
            .iter()
            .any(|b| b.reason == RecoveryReason::PendingClaim)
    );
    let fence = runtime
        .fence_dispatch_shutdown(|| async { Ok::<(), String>(()) })
        .await
        .unwrap();
    runtime.drain_dispatch_tails(&fence).await.unwrap();
    assert!(!runtime.has_paused_recovery());
    assert_eq!(
        db.recovery_claims().unwrap()[0].state,
        RecoveryClaimState::Released {
            reason: "shutdown".into()
        }
    );
}

#[tokio::test]
async fn paused_stop_cancel_and_subtree_cancel_fence_the_old_incarnation() {
    for mode in 0..3 {
        let f = Fixture::new(0);
        let run = saved(&f).await;
        let runtime = new_process(&f);
        let db = storage::Database::open(&f.config).unwrap();
        let paused = prepare(&f, &runtime);
        let old = paused.claim().members[0].old_incarnation.clone();
        let revision = paused.claim().members[0].reserved_revision;
        let reason = if mode == 0 {
            StopReason::OperatorStopped
        } else {
            StopReason::Cancelled
        };
        match mode {
            0 => runtime.stop(run, StopScope::SelfOnly).unwrap(),
            1 => runtime.cancel(run).unwrap(),
            _ => runtime.cancel_subtree(run).unwrap(),
        }
        let intent =
            ResumeIntent::decode(db.resume_intent(&run.to_string()).unwrap().unwrap()).unwrap();
        assert_eq!(intent.incarnation, old);
        assert_eq!(intent.stop_reason, Some(reason));
        assert!(!intent.active);
        assert!(
            db.resume_intent_revision(&run.to_string())
                .unwrap()
                .unwrap()
                > revision
        );
        let expected = if mode == 0 {
            RecoveryReason::OperatorStopped
        } else {
            RecoveryReason::Cancelled
        };
        let plan = new_process(&f).recovery_plan().unwrap();
        assert!(plan.components.is_empty());
        assert!(
            plan.report.skipped.iter().any(|s| s.reason == expected),
            "{plan:?}"
        );
        assert!(
            !plan
                .report
                .blocked
                .iter()
                .any(|s| s.reason == RecoveryReason::IntentRevisionConflict)
        );
        runtime
            .release_prepared_component(paused, "operator aborted recovery")
            .unwrap();
        assert!(!runtime.has_paused_recovery());
        assert!(!runtime.shared.quiesce.lock().intents.contains_key(&run));
        assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn released_claim_never_rolls_back_revision_or_blocks_manual_registration_in_memory() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    runtime.spawn_reserved(run, None, Role::Worker, "not allowed", RunConfig::default());
    assert!(runtime.list_agents().is_empty());
    let revision = paused.claim().members[0].reserved_revision;
    runtime
        .release_prepared_component(paused, "retry manually")
        .unwrap();
    let db = storage::Database::open(&f.config).unwrap();
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(revision)
    );
    assert!(!runtime.has_paused_recovery());
    assert!(!runtime.shared.quiesce.lock().intents.contains_key(&run));
    let plan = runtime.recovery_plan().unwrap();
    assert!(plan.components.is_empty());
    assert!(
        plan.report
            .skipped
            .iter()
            .any(|s| s.reason == RecoveryReason::IntentRevisionConflict)
    );
    // Do not run the old task in the original process after its snapshot was
    // deliberately claimed by this simulated new process.
}
