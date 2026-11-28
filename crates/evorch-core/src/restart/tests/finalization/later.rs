use super::*;
mod last;
mod latch;
use std::{path::Path, sync::atomic::Ordering};

use crate::restart::tests::checked_target;
async fn target(dir: &Path) -> (FinalizationTarget, RestartMetadata) {
    let (current, checked) = checked_target::checked_target(dir).await;
    let mut metadata = metadata();
    metadata.successor_build = checked.request().description().build_identity.clone();
    (FinalizationTarget::new(current, checked), metadata)
}
async fn owned_chat(host: &BootstrappedHost, core: &RestartCoordinator) -> (RunId, RestartId) {
    let (runs, id) = owned_chats(host, core, 1).await;
    (runs[0], id)
}
async fn owned_chats(
    host: &BootstrappedHost,
    core: &RestartCoordinator,
    count: usize,
) -> (Vec<RunId>, RestartId) {
    let runs = parked_runs(host, count).await;
    let id = seal_runs(core, &runs).await;
    (runs, id)
}
async fn parked_runs(host: &BootstrappedHost, count: usize) -> Vec<RunId> {
    let mut runs = Vec::new();
    for index in 0..count {
        let events = host.runtime.event_bus().subscribe();
        let config = runtime::RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        };
        // A thread admits one root chat at a time; further runs of the same
        // thread are its descendants under the same binding.
        let run = if index == 0 {
            host.runtime
                .delegate_chat("active", runtime::Role::Orchestrator, "park".into(), config)
                .unwrap()
        } else {
            host.runtime
                .delegate_background_as_child(runs[0], runtime::Role::Worker, "park", config)
                .unwrap()
        };
        waiting(run, events).await;
        runs.push(run);
    }
    runs
}
async fn seal_runs(core: &RestartCoordinator, runs: &[RunId]) -> RestartId {
    let id = core
        .request_planned(
            runs.iter()
                .take(1)
                .map(|run| {
                    (
                        *run,
                        runtime::quiesce::RecoveryRecipe {
                            settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE
                                .into(),
                            policy: runtime::quiesce::ResumePolicy::CoreOwned,
                            goal_id: None,
                            team: None,
                            lane: None,
                            descriptor: None,
                        },
                    )
                })
                .collect(),
        )
        .unwrap();
    core.wait_sealed(id).await.unwrap();
    id
}
fn create(host: &BootstrappedHost, mut metadata: RestartMetadata) -> RestartTransaction {
    metadata.old_builtin_tool_names = host.runtime.restart_builtin_tool_names();
    host.composition
        .storage_writer
        .mutate_restart(
            "restart".into(),
            None,
            RestartMutation::Create(Box::new(metadata)),
        )
        .unwrap()
}
async fn reclaimed(host: &BootstrappedHost, core: &RestartCoordinator) {
    let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
    assert!(resources.lock().await.cleanup().await.is_ok());
    let tx = saved(host);
    assert_eq!(
        tx.phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
    assert!(host.restart_producers().iter().all(|gate| !gate.is_open()));
}

#[tokio::test]
async fn stages_six_to_eight_check_transaction_read_back_arm_then_invalidate_without_exec() {
    six_to_eight_with_parked_runs(1).await;
}

#[tokio::test]
async fn stages_six_to_eight_check_several_parked_runs_of_one_thread() {
    six_to_eight_with_parked_runs(3).await;
}

async fn six_to_eight_with_parked_runs(count: usize) {
    let (dir, mut host) = boot().await;
    let (mut target, metadata) = target(dir.path()).await;
    let core = RestartCoordinator::new(&host);
    let (runs, id) = owned_chats(&host, &core, count).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let record = order.clone();
    target.observe = Some(Arc::new(move |stage| record.lock().unwrap().push(stage)));
    let tx = create(&host, metadata);
    core.start_finalization(id, &mut host, tx, target).unwrap();
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::Armed),
        "tx={:?}",
        saved(&host)
    );
    assert_eq!(
        *order.lock().unwrap(),
        [
            ExitStage::TargetChecked,
            ExitStage::TransactionChecked,
            ExitStage::Armed
        ]
    );
    reclaimed(&host, &core).await;
    let tx = saved(&host);
    assert!(
        tx.ticket_nonce_hash
            .as_ref()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(tx.expected.len(), count);
    assert_eq!(
        tx.expected
            .iter()
            .map(|m| m.run_id.clone())
            .collect::<BTreeSet<_>>(),
        runs.iter().map(ToString::to_string).collect()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("probes")).unwrap(),
        "probe\nprobe\n"
    );
    for run in runs {
        host.runtime.stop(run, StopScope::SelfOnly).unwrap();
        host.runtime.wait(run).await.unwrap();
    }
    core.transition_to_shutdown();
    assert!(host.shutdown().await.is_err());
}

#[derive(Clone, Copy, Debug)]
enum Race {
    Stop,
    Shutdown,
    Failure,
    FailureStop,
    FailureShutdown,
}

#[tokio::test]
async fn shared_cleanup_retries_unconfirmed_invalidation_without_resume() {
    let model = Arc::new(Model::default());
    let (dir, mut host) = boot_composition(model.clone(), true).await;
    let (mut target, metadata) = target(dir.path()).await;
    let core = RestartCoordinator::new(&host);
    let (run, id) = owned_chat(&host, &core).await;
    let path = host.composition.storage.db_path.clone();
    let fault_path = path.clone();
    target.after = Some(Arc::new(move |stage| {
        if stage == ExitStage::Armed {
            rusqlite::Connection::open(&fault_path).unwrap().execute_batch(
                "CREATE TRIGGER deny_cleanup BEFORE UPDATE ON restart_transactions WHEN NEW.phase='invalidated' BEGIN SELECT RAISE(ABORT,'invalidation unconfirmed'); END;"
            ).unwrap();
        }
    }));
    let tx = create(&host, metadata);
    core.start_finalization(id, &mut host, tx, target).unwrap();
    assert!(core.wait_finalization(id).await.is_err());
    let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
    let mut resources = resources.lock().await;
    assert!(resources.cleanup().await.is_err());
    assert_eq!(
        saved(&host).phase,
        storage::recovery::restart::RestartPhase::Armed
    );
    assert!(host.restart_producers().iter().all(|g| !g.is_open()));
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch("DROP TRIGGER deny_cleanup")
        .unwrap();
    // Independent reclamation retries, but never hides the original failure.
    assert!(resources.cleanup().await.is_err());
    let tx = saved(&host);
    assert_eq!(
        tx.phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
    assert!(resources.cleanup().await.is_err());
    assert_eq!(saved(&host).revision, tx.revision, "cleanup is idempotent");
    assert_eq!(model.0.load(Ordering::SeqCst), 1);
    drop(resources);
    host.runtime.stop(run, StopScope::SelfOnly).unwrap();
    host.runtime.wait(run).await.unwrap();
    assert!(host.shutdown().await.is_err());
}
async fn matrix(stage: ExitStage) {
    for race in [
        Race::Stop,
        Race::Shutdown,
        Race::Failure,
        Race::FailureStop,
        Race::FailureShutdown,
    ]
    .into_iter()
    .filter(|race| {
        stage != ExitStage::TransactionChecked || matches!(race, Race::Stop | Race::Shutdown)
    }) {
        let model = Arc::new(Model::default());
        let (dir, mut host) = boot_composition(model.clone(), true).await;
        let (mut target, metadata) = target(dir.path()).await;
        let (returned, service_returned) = tokio::sync::oneshot::channel();
        host.services
            .start("later-cleanup-witness", |stop| async move {
                stop.await?;
                returned.send(()).unwrap();
                Ok(())
            });
        let core = RestartCoordinator::new(&host);
        let (run, id) = owned_chat(&host, &core).await;
        let (arrived, ready) = tokio::sync::oneshot::channel();
        let (release, receive) = std::sync::mpsc::channel();
        let barrier = Mutex::new(Some((arrived, receive)));
        target.observe = Some(Arc::new(move |now| {
            if now == stage {
                let (arrived, receive) = barrier.lock().unwrap().take().unwrap();
                arrived.send(()).unwrap();
                receive.recv().unwrap();
            }
        }));
        let tx = create(&host, metadata);
        core.start_finalization(id, &mut host, tx, target).unwrap();
        ready.await.unwrap();
        assert_eq!(model.0.load(Ordering::SeqCst), 1);
        if matches!(
            race,
            Race::Failure | Race::FailureStop | Race::FailureShutdown
        ) {
            match stage {
                ExitStage::TargetChecked => {
                    std::fs::write(dir.path().join("target"), "changed").unwrap()
                }
                ExitStage::Armed => {
                    let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
                    db.execute_batch("CREATE TRIGGER fail_arm BEFORE UPDATE OF phase ON restart_transactions WHEN NEW.phase='armed' BEGIN SELECT RAISE(ABORT,'Arm rejected'); END;").unwrap();
                }
                _ => unreachable!(),
            }
        }
        let shutdown = matches!(race, Race::Shutdown | Race::FailureShutdown);
        if shutdown {
            core.transition_to_shutdown();
            // Unlike capture-time Stop, ShuttingDown delivers terminal-only.
            host.runtime.stop(run, StopScope::SelfOnly).unwrap();
            host.runtime.wait(run).await.unwrap();
            assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
        } else if matches!(race, Race::Stop | Race::FailureStop) {
            for direct in [true, false] {
                let stopped = if direct {
                    host.runtime.stop(run, StopScope::SelfOnly)
                } else {
                    RestartStopHandler::stop(&core, run, StopScope::SelfOnly)
                };
                assert!(
                    matches!(stopped, Err(RuntimeError::RestartBusy)),
                    "{stage:?}/{race:?}"
                );
            }
            assert!(core.cancel(id).is_err());
        }
        release.send(()).unwrap();
        assert!(core.wait_finalization(id).await.is_err());
        let last = core.0.state.lock().unwrap().completed_stage.unwrap();
        let failed = matches!(race, Race::Failure | Race::FailureStop);
        assert_eq!(
            last,
            if failed || shutdown {
                match stage {
                    ExitStage::TargetChecked => ExitStage::StorageDrained,
                    ExitStage::TransactionChecked => ExitStage::TargetChecked,
                    ExitStage::Armed => ExitStage::TransactionChecked,
                    _ => unreachable!(),
                }
            } else {
                ExitStage::Armed
            },
            "{stage:?}/{race:?}; tx={:?}",
            saved(&host)
        );
        reclaimed(&host, &core).await;
        assert_eq!(model.0.load(Ordering::SeqCst), 1, "no double resume");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("probes")).unwrap(),
            "probe\nprobe\n"
        );
        core.transition_to_shutdown();
        assert!(host.shutdown().await.is_err());
        service_returned.await.unwrap();
        assert_eq!(model.0.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn target_failure_stop_shutdown_matrix_is_failed_closed_and_reclaims() {
    matrix(ExitStage::TargetChecked).await;
}
#[tokio::test]
async fn transaction_check_stop_shutdown_matrix_is_failed_closed_and_reclaims() {
    matrix(ExitStage::TransactionChecked).await;
}
#[tokio::test]
async fn arm_failure_stop_shutdown_matrix_is_failed_closed_and_reclaims() {
    matrix(ExitStage::Armed).await;
}

#[tokio::test]
async fn stop_and_shutdown_after_each_effect_never_resume_or_strand_armed_transactions() {
    for stage in [
        ExitStage::TargetChecked,
        ExitStage::TransactionChecked,
        ExitStage::Armed,
    ] {
        for shutdown in [false, true] {
            let model = Arc::new(Model::default());
            let (dir, mut host) = boot_composition(model.clone(), true).await;
            let (mut target, metadata) = target(dir.path()).await;
            let core = RestartCoordinator::new(&host);
            let (run, id) = owned_chat(&host, &core).await;
            let (arrived, ready) = tokio::sync::oneshot::channel();
            let (release, receive) = std::sync::mpsc::channel();
            let barrier = Mutex::new(Some((arrived, receive)));
            target.after = Some(Arc::new(move |now| {
                if now == stage {
                    let (arrived, receive) = barrier.lock().unwrap().take().unwrap();
                    arrived.send(()).unwrap();
                    receive.recv().unwrap();
                }
            }));
            let tx = create(&host, metadata);
            core.start_finalization(id, &mut host, tx, target).unwrap();
            ready.await.unwrap();
            let tx = saved(&host);
            assert_eq!(
                tx.phase,
                if stage == ExitStage::Armed {
                    storage::recovery::restart::RestartPhase::Armed
                } else {
                    storage::recovery::restart::RestartPhase::Committed
                }
            );
            if shutdown {
                core.transition_to_shutdown();
                host.runtime.stop(run, StopScope::SelfOnly).unwrap();
                host.runtime.wait(run).await.unwrap();
            } else {
                assert!(matches!(
                    host.runtime.stop(run, StopScope::SelfOnly),
                    Err(RuntimeError::RestartBusy)
                ));
                assert!(matches!(
                    RestartStopHandler::stop(&core, run, StopScope::SelfOnly),
                    Err(RuntimeError::RestartBusy)
                ));
            }
            release.send(()).unwrap();
            assert!(core.wait_finalization(id).await.is_err());
            assert_eq!(
                core.0.state.lock().unwrap().completed_stage,
                Some(if shutdown { stage } else { ExitStage::Armed })
            );
            reclaimed(&host, &core).await;
            assert_eq!(
                core.snapshot().phase,
                if shutdown {
                    RestartPhase::ShuttingDown
                } else {
                    RestartPhase::FailedClosed
                }
            );
            assert_eq!(model.0.load(Ordering::SeqCst), 1);
            core.transition_to_shutdown();
            assert!(host.shutdown().await.is_err());
        }
    }
}

#[tokio::test]
async fn independent_readback_rejects_arm_nonce_mismatch() {
    let (dir, mut host) = boot().await;
    let (target, metadata) = target(dir.path()).await;
    let core = RestartCoordinator::new(&host);
    let (_run, id) = owned_chat(&host, &core).await;
    let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    db.execute_batch("CREATE TRIGGER corrupt_nonce AFTER UPDATE OF phase ON restart_transactions WHEN NEW.phase='armed' BEGIN UPDATE restart_transactions SET ticket_nonce_hash='wrong-nonce' WHERE restart_id=NEW.restart_id; END;").unwrap();
    let tx = create(&host, metadata);
    core.start_finalization(id, &mut host, tx, target).unwrap();
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::TransactionChecked)
    );
    reclaimed(&host, &core).await;
    let tx = saved(&host);
    assert_eq!(tx.ticket_nonce_hash.as_deref(), Some("wrong-nonce"));
    assert!(tx.failure_reason.unwrap().contains("readback mismatch"));
    core.transition_to_shutdown();
    assert!(host.shutdown().await.is_err());
}
