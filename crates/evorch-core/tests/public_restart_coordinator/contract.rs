#[path = "approvals.rs"]
mod approvals;
#[path = "fixture.rs"]
mod fixture;
#[path = "invocation.rs"]
mod invocation;
#[path = "shell.rs"]
mod shell;
use fixture::*;
use std::sync::atomic::Ordering;
use workbench_host::admin::*;
use workbench_protocol::{
    namespaces::lifecycle::{restart::*, *},
    wire::Revision,
};

async fn result(f: &Fixture, request: RequestRestart) -> Result<AdminAccepted, AdminFailure> {
    f.submit(command(request))?.await.unwrap()
}

#[tokio::test]
async fn refusals_leave_gate_and_transaction_unchanged() {
    let f = Fixture::new().await;
    let base = f.request("base").await;
    let mut unconfirmed = base.clone();
    unconfirmed.confirmed = false;
    assert!(f.submit(command(unconfirmed)).is_err());
    let mut unknown = base.clone();
    unknown.binding.target_id = id("not-allowlisted");
    unknown.binding.operation_id = id("unknown");
    assert!(result(&f, unknown).await.is_err());
    let mut stale = base.clone();
    stale.binding.target_revision = Revision(999);
    stale.binding.operation_id = id("stale");
    assert!(result(&f, stale).await.is_err());
    f.graphs.state.lock().unwrap().1 = 1;
    assert!(result(&f, base).await.is_err());
    f.assert_running_untouched();
    f.shutdown().await;
}

#[tokio::test]
async fn graph_change_between_inspection_and_commit_is_rejected_before_gate() {
    let mut f = Fixture::new().await;
    let request = f.request("graph-race").await;
    f.port.hold.store(2, Ordering::SeqCst);
    let response = f.submit(command(request)).unwrap();
    f.event(2, "graph-race").await;
    *f.graphs.state.lock().unwrap() = (Revision(2), 1);
    f.port.release();
    assert!(response.await.unwrap().is_err());
    f.assert_running_untouched();
    f.shutdown().await;
}

#[tokio::test]
async fn graph_revision_lock_covers_gate_transition_against_a_competing_collector() {
    let f = Fixture::new().await;
    let graphs = f.graphs.clone();
    let (enter, entered) = std::sync::mpsc::channel();
    let (checked, check) = std::sync::mpsc::channel();
    let competitor = std::thread::spawn(move || {
        entered.recv().unwrap();
        assert!(
            graphs.state.try_lock().is_err(),
            "live collector lock must span the gate transition"
        );
        checked.send(()).unwrap();
    });
    *f.graphs.transition_hook.lock().unwrap() = Some(Box::new(move || {
        enter.send(()).unwrap();
        check.recv().unwrap();
    }));
    let request = f.request("atomic").await;
    result(&f, request).await.unwrap();
    competitor.join().unwrap();
    assert!(f.core.snapshot().id.is_some());
    f.shutdown().await;
}

#[tokio::test]
async fn audit_admission_failures_have_no_restart_effects() {
    for stage in [1, 2] {
        let f = Fixture::new().await;
        let request = f.request(&format!("audit-{stage}")).await;
        f.port.fail.store(stage, Ordering::SeqCst);
        assert!(result(&f, request).await.is_err());
        f.assert_running_untouched();
        f.shutdown().await;
    }
}

#[tokio::test]
async fn held_restart_mapping_ack_blocks_sealing_but_not_cancellation() {
    let mut f = Fixture::new().await;
    f.port.hold.store(3, Ordering::SeqCst);
    let request = f.request("mapping-held").await;
    let response = f.submit(command(request)).unwrap();
    f.event(3, "mapping-held").await;
    let mut changes = f.core.subscribe();
    loop {
        if f.core.snapshot().phase == evorch_core::restart::RestartPhase::Sealing {
            break;
        }
        changes.changed().await.unwrap();
    }
    assert_eq!(f.finalizer.calls.load(Ordering::SeqCst), 0);
    let cancel = f.cancel("cancel-mapping-held");
    f.submit(cancel).unwrap().await.unwrap().unwrap();
    f.port.release();
    response.await.unwrap().unwrap();
    f.event(4, "mapping-held").await;
    assert_eq!(f.finalizer.calls.load(Ordering::SeqCst), 0);
    f.shutdown().await;
}

#[tokio::test]
async fn canonical_requests_reuse_one_attempt_and_finalization() {
    let f = Fixture::new().await;
    let hold = f.host.restart_producers()[1].try_enter().unwrap();
    let request = f.request("leader").await;
    result(&f, request.clone()).await.unwrap();
    let before = f.core.snapshot();
    let mut duplicate = request.clone();
    duplicate.binding.operation_id = id("duplicate");
    result(&f, duplicate).await.unwrap();
    let after = f.core.snapshot();
    assert_eq!(before.id, after.id);
    assert_eq!(before.revision, after.revision);
    let mut other = request;
    other.binding.target_id = id("other");
    other.binding.operation_id = id("different-target");
    assert!(result(&f, other).await.is_err());
    drop(hold);
    f.finalizer.reached.notified().await;
    assert_eq!(f.finalizer.calls.load(Ordering::SeqCst), 1);
    // Cancel is the sole management effect which survives unavailable audit.
    f.port.fail.store(1, Ordering::SeqCst);
    let cancel = f.cancel("cancel");
    let receipt = f
        .submit(cancel.clone())
        .unwrap()
        .await
        .unwrap()
        .unwrap()
        .into_receipt();
    assert_eq!(receipt.outcome, AdminOutcome::Cancelled);
    assert!(receipt.audit_warning);
    assert_eq!(
        f.core.snapshot().phase,
        evorch_core::restart::RestartPhase::Running
    );
    let duplicate = f
        .submit(cancel)
        .unwrap()
        .await
        .unwrap()
        .unwrap()
        .into_receipt();
    assert_eq!(duplicate.outcome, AdminOutcome::Cancelled);
    f.shutdown().await;
}

#[tokio::test]
async fn failed_restart_audit_binding_cancels_before_finalization() {
    let mut f = Fixture::new().await;
    let request = f.request("bind-failure").await;
    f.port.fail.store(3, Ordering::SeqCst);
    result(&f, request).await.unwrap();
    f.event(4, "bind-failure").await;
    assert_eq!(
        f.core.snapshot().phase,
        evorch_core::restart::RestartPhase::Running
    );
    assert_eq!(f.finalizer.calls.load(Ordering::SeqCst), 0);
    assert!(
        storage::Database::open(&f.host.composition.storage)
            .unwrap()
            .restart_transactions()
            .unwrap()
            .is_empty()
    );
    f.shutdown().await;
}

#[tokio::test]
async fn listing_neither_probes_targets_nor_exposes_local_paths() {
    let f = Fixture::new().await;
    std::fs::remove_file(f.root().join("target.sh")).unwrap();
    let reply = f
        .leaf
        .query(
            &LifecycleQuery::Restart(RestartQuery::Targets),
            &f.connection.verified,
        )
        .await
        .unwrap();
    let text = serde_json::to_string(&reply).unwrap();
    assert!(!text.contains(f.root().to_str().unwrap()));
    assert!(!text.contains("target.sh"));
    let LifecycleReply::Restart(RestartReply::Targets(targets)) = reply else {
        panic!()
    };
    assert_eq!(targets.targets.len(), 3);
    let request = f.request("missing-binary").await;
    assert!(result(&f, request).await.is_err());
    f.assert_running_untouched();
    f.shutdown().await;
}

#[tokio::test]
async fn public_cancel_checks_revision_and_existing_capture_boundary() {
    use core_launcher::handoff::{
        ProbeLimits,
        targets::{RestartTarget, TargetRequirements, TargetSelection},
    };
    use storage::recovery::restart::{RestartMetadata, RestartMutation};
    let mut f = Fixture::new().await;
    result(&f, f.request("capture").await).await.unwrap();
    f.finalizer.reached.notified().await;
    let id = f.core.snapshot().id.unwrap();
    let before = f.core.snapshot();
    let mut stale = f.cancel("stale-cancel");
    let LifecycleCommand::Restart(RestartCommand::Cancel(ref mut c)) = stale else {
        panic!()
    };
    c.expected_revision = Revision(before.revision + 1);
    assert!(f.submit(stale).is_err());
    assert_eq!(before.revision, f.core.snapshot().revision);
    let path = f.root().join("target.sh");
    let checked = RestartTarget::preflight(
        TargetSelection {
            id: "selected",
            configured_path: Some(&path),
            timeout: std::time::Duration::from_secs(300),
        },
        &f.current,
        TargetRequirements {
            handoff: 1,
            snapshot: 1,
            storage_schema: 62,
            protocol: 1,
            platform: &f.description.platform,
        },
        ProbeLimits::default(),
        std::future::pending(),
    )
    .await
    .unwrap()
    .before_capture(&f.current, ProbeLimits::default(), std::future::pending())
    .await
    .unwrap();
    let metadata = RestartMetadata {
        restart_id: "capture-tx".into(),
        capture_id: "capture".into(),
        config_identity: "config".into(),
        old_epoch: 7,
        successor_epoch: 8,
        old_build: "old".into(),
        successor_build: f.description.build_identity.clone(),
        target_identity: "selected".into(),
        handoff_version: 1,
        snapshot_version: 1,
        storage_version: 62,
        old_builtin_tool_names: f.host.runtime.restart_builtin_tool_names(),
        old_compiled_catalog_fingerprint: None,
        new_compiled_catalog_fingerprint: None,
        old_effective_tools_fingerprint: None,
        new_effective_tools_fingerprint: None,
    };
    let tx = f
        .host
        .composition
        .storage_writer
        .mutate_restart(
            "capture-tx".into(),
            None,
            RestartMutation::Create(Box::new(metadata)),
        )
        .unwrap();
    f.core
        .start_finalization(
            id,
            &mut f.host,
            tx,
            evorch_core::restart::FinalizationTarget::new(f.current.clone(), checked),
        )
        .unwrap();
    assert!(f.submit(f.cancel("after-capture")).is_err());
    assert_ne!(
        f.core.snapshot().phase,
        evorch_core::restart::RestartPhase::Running
    );
    // #710 intentionally ends fail-closed without an installed exec environment.
    assert!(f.core.wait_finalization(id).await.is_err());
    assert!(f.submit(f.cancel("after-finalization")).is_err());
    f.shutdown().await;
}

#[tokio::test]
async fn lost_request_reply_reads_back_without_new_inspection_or_restart() {
    let mut f = Fixture::new().await;
    let hold = f.host.restart_producers()[1].try_enter().unwrap();
    let request = f.request("lost-reply").await;
    drop(f.submit(command(request.clone())).unwrap());
    f.event(3, "lost-reply").await;
    let before = f.core.snapshot();
    std::fs::remove_file(f.root().join("target.sh")).unwrap();
    result(&f, request.clone()).await.unwrap();
    let mut changed = request;
    changed.binding.target_id = id("other");
    assert!(result(&f, changed).await.is_err());
    assert_eq!(before.id, f.core.snapshot().id);
    assert_eq!(before.revision, f.core.snapshot().revision);
    f.submit(f.cancel("cancel-lost"))
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    drop(hold);
    assert_eq!(f.finalizer.calls.load(Ordering::SeqCst), 0);
    f.shutdown().await;
}

#[tokio::test]
async fn strict_local_config_and_pre_capture_reprobe_fail_before_transaction() {
    let mut f = Fixture::new().await;
    let request = f.request("bad-config").await;
    let config = f.root().join("config.toml");
    let original = std::fs::read_to_string(&config).unwrap();
    for invalid in [
        "[core.restart]\ntimeout_secs=0\n",
        "[core.restart]\ntimeout_secs=3601\n",
        "[core.restart]\nunknown_field=true\n",
    ] {
        std::fs::write(&config, invalid).unwrap();
        assert!(result(&f, request.clone()).await.is_err());
        f.assert_running_untouched();
    }
    std::fs::write(&config, original).unwrap();
    let request = f.request("changed-binary").await;
    f.port.hold.store(2, Ordering::SeqCst);
    let response = f.submit(command(request)).unwrap();
    f.event(2, "changed-binary").await;
    // Inspection saw the original ELF/description. Stage 2 must re-probe and
    // compare the same immutable target through #710, never retarget it.
    std::fs::write(
        f.root().join("target.sh"),
        "#!/bin/sh\nprintf invalid-description\\n\n",
    )
    .unwrap();
    f.port.release();
    response.await.unwrap().unwrap();
    f.event(4, "changed-binary").await;
    assert_eq!(
        f.core.snapshot().phase,
        evorch_core::restart::RestartPhase::Running
    );
    assert_eq!(f.finalizer.calls.load(Ordering::SeqCst), 0);
    assert!(
        storage::Database::open(&f.host.composition.storage)
            .unwrap()
            .restart_transactions()
            .unwrap()
            .is_empty()
    );
    f.shutdown().await;
}

#[tokio::test]
async fn recipe_binding_aba_after_inspect_refuses_before_gate_and_transaction() {
    let mut f = Fixture::new().await;
    let request = f.request("recipe-binding-race").await;
    f.port.hold.store(2, Ordering::SeqCst);
    let response = f.submit(command(request)).unwrap();
    f.event(2, "recipe-binding-race").await;
    // Equal contents do not restore an older binding observation.
    f.recipes.1.replace(Default::default());
    f.recipes.1.replace(Default::default());
    f.port.release();
    assert!(response.await.unwrap().is_err());
    f.assert_running_untouched();
    f.shutdown().await;
}

#[tokio::test]
async fn live_adapter_releases_reservation_only_after_canonical_reversible_cancel() {
    use evorch_core::restart::public::{LiveUndisposedAdmission, UndisposedAdmission};
    let f = Fixture::new().await;
    let epoch = f.host.host.lock().unwrap().projection().instance_epoch();
    let collector = f.host.graph_collector().clone();
    let adapter = LiveUndisposedAdmission {
        collector: collector.clone(),
        core: f.core.clone(),
        epoch,
    };
    let revision = collector.snapshot().unwrap().snapshot_revision;
    let id = adapter
        .admit(revision, &mut || f.core.request().map_err(|_| conflict()))
        .unwrap();
    assert!(
        adapter
            .admit(revision, &mut || -> Result<_, AdminFailure> {
                panic!("reservation must refuse before transition")
            })
            .is_err()
    );
    f.core.cancel(id).unwrap();
    assert_eq!(
        f.core.snapshot().phase,
        evorch_core::restart::RestartPhase::Running
    );
    let next = adapter
        .admit(revision, &mut || f.core.request().map_err(|_| conflict()))
        .unwrap();
    assert_ne!(id, next);
    f.core.cancel(next).unwrap();
    f.shutdown().await;
}

#[tokio::test]
async fn recipe_collection_metadata_race_refuses_prepared_before_gate_or_transaction() {
    let f = Fixture::new().await;
    // A retained run's metadata changes, not its live-set membership. The runtime
    // unit regression also covers active runs with unchanged incarnations.
    let run = f.host.runtime.delegate_background(
        runtime::Role::Worker,
        "done".into(),
        Default::default(),
    );
    f.host.runtime.wait(run).await.unwrap();
    let request = f.request("recipe-metadata-race").await;
    let entered = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let release = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    *f.recipes.2.lock().unwrap() = Some((entered.clone(), release.clone()));
    let response = f.submit(command(request)).unwrap();
    entered.wait().await;
    f.host
        .runtime
        .bind_thread_root("changed-during-collect", run)
        .unwrap();
    release.wait().await;
    assert!(response.await.unwrap().is_err());
    f.assert_running_untouched();
    f.shutdown().await;
}
