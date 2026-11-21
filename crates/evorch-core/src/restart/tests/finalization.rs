use super::*;
mod audit_sealing;
#[cfg(target_os = "linux")]
mod later;
mod m2;
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use storage::recovery::restart::RestartMetadata;
use storage::recovery::restart::{RestartMutation, RestartTransaction};
use workbench_host::bootstrap::planned_exit::ExitStage;
#[path = "../../../shared-test-support/restart_metadata.rs"]
mod restart_metadata;
use restart_metadata::metadata;

#[cfg(target_os = "linux")]
pub(crate) fn assert_workers_joined(core: &RestartCoordinator) {
    let state = core.0.state.lock().unwrap();
    assert!(
        state
            .completed_stage
            .is_some_and(|stage| stage >= ExitStage::WorkersJoined),
        "{:?}",
        state.completed_stage
    );
}

fn transaction(host: &BootstrappedHost) -> RestartTransaction {
    let mut metadata = metadata();
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
fn saved(host: &BootstrappedHost) -> RestartTransaction {
    storage::Database::open(&host.composition.storage)
        .unwrap()
        .restart_transaction("restart")
        .unwrap()
        .unwrap()
}
async fn planned_chat(host: &BootstrappedHost, core: &RestartCoordinator) -> (RunId, RestartId) {
    let events = host.runtime.event_bus().subscribe();
    let run = host
        .runtime
        .delegate_chat(
            "thread",
            runtime::Role::Orchestrator,
            "park".into(),
            runtime::RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    waiting(run, events).await;
    let recipe = runtime::quiesce::RecoveryRecipe {
        settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
        policy: runtime::quiesce::ResumePolicy::CoreOwned,
        goal_id: None,
        team: None,
        lane: None,
        descriptor: None,
    };
    let id = core
        .request_planned(BTreeMap::from([(run, recipe)]))
        .unwrap();
    core.wait_sealed(id).await.unwrap();
    (run, id)
}
#[tokio::test(start_paused = true)]
async fn atomic_capture_owns_sealed_rejects_both_stop_entries_and_never_grants_exec() {
    let model = Arc::new(Model::default());
    let (_dir, mut host) = boot_composition(model.clone(), true).await;
    let core = RestartCoordinator::new(&host);
    let (run, id) = planned_chat(&host, &core).await;
    let observation = core.wait_sealed(id).await.unwrap();
    let another = core.wait_sealed(id).await.unwrap();
    let tx = transaction(&host);
    core.start_finalization_without_target(id, &mut host, tx)
        .unwrap();
    // No await: the memory boundary and Capturing are one synchronous action.
    assert_eq!(core.snapshot().phase, RestartPhase::Capturing);
    assert!(observation.validate(&host.runtime).is_err());
    assert!(another.validate(&host.runtime).is_err());
    assert!(matches!(
        host.runtime.stop(run, StopScope::SelfOnly),
        Err(RuntimeError::RestartBusy)
    ));
    assert!(matches!(
        RestartStopHandler::stop(&core, run, StopScope::SelfOnly),
        Err(RuntimeError::RestartBusy)
    ));
    assert!(core.cancel(id).is_err());
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::StorageDrained)
    );
    let tx = saved(&host);
    assert_eq!(
        tx.phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
    assert!(
        tx.manifest_digest.is_some(),
        "Commit really completed, but is not exec authority"
    );
    assert_eq!(tx.expected.len(), 1);
    assert!(
        tx.owner_progress.is_empty(),
        "the targetless fixture must not install holds"
    );
    assert!(host.spawn_ledger.is_fenced());
    assert!(core.request().is_err());
    assert!(
        core.start_finalization_without_target(id, &mut host, tx)
            .is_err()
    );
    assert!(host.restart_producers().iter().all(|gate| !gate.is_open()));
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    // FailedClosed permits terminal-only Stop, never replay/provider work.
    host.runtime.stop(run, StopScope::SelfOnly).unwrap();
    assert_eq!(
        host.runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    host.composition.storage_writer.barrier().unwrap(); // writer is not stopped
    core.transition_to_shutdown();
    assert!(
        host.shutdown().await.is_err(),
        "FailedClosed remains visible to operator shutdown"
    );
}
#[tokio::test]
async fn stop_before_atomic_capture_cancels_planned_attempt_and_stale_capture_is_rejected() {
    let (_dir, mut host) = boot_composition(Arc::new(Model::default()), true).await;
    let core = RestartCoordinator::new(&host);
    let (run, id) = planned_chat(&host, &core).await;
    let tx = transaction(&host);
    host.runtime.stop(run, StopScope::SelfOnly).unwrap();
    phase(&core, RestartPhase::Running).await;
    assert!(
        core.start_finalization_without_target(id, &mut host, tx)
            .is_err()
    );
    assert!(!core.0.state.lock().unwrap().capture_started);
    assert_eq!(
        saved(&host).phase,
        storage::recovery::restart::RestartPhase::Preparing
    );
    assert!(host.restart_producers().iter().all(|gate| gate.is_open()));
    assert_eq!(
        host.runtime.quiesce_state(),
        runtime::quiesce::QuiesceState::Running
    );
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn capture_commit_failure_is_failed_closed_before_any_m2_stage() {
    let (_dir, mut host) = boot().await;
    let witness = cleanup_service(&mut host);
    let core = RestartCoordinator::new(&host);
    let id = core.request_planned(BTreeMap::new()).unwrap();
    core.wait_sealed(id).await.unwrap();
    let tx = transaction(&host);
    let sql = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    sql.execute_batch("CREATE TRIGGER reject_commit BEFORE UPDATE OF phase ON restart_transactions WHEN NEW.phase='committed' BEGIN SELECT RAISE(ABORT,'commit rejected'); END;").unwrap();
    core.start_finalization_without_target(id, &mut host, tx)
        .unwrap();
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert_eq!(core.0.state.lock().unwrap().completed_stage, None);
    assert_eq!(
        saved(&host).phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
    assert!(host.runtime.producer_gate().try_enter().is_none());
    core.transition_to_shutdown();
    shutdown_joins_service(&mut host, witness).await;
    let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
    assert!(resources.lock().await.cleanup().await.is_ok());
    assert!(host.spawn_ledger.is_fenced());
}
#[tokio::test]
async fn shutdown_after_capture_retains_finalizer_and_prevents_m2_progress() {
    let (_dir, mut host) = boot_composition(Arc::new(Model::default()), true).await;
    let witness = cleanup_service(&mut host);
    let core = RestartCoordinator::new(&host);
    let id = core.request_planned(BTreeMap::new()).unwrap();
    core.wait_sealed(id).await.unwrap();
    let tx = transaction(&host);
    core.start_finalization_without_target(id, &mut host, tx)
        .unwrap();
    // Shutdown wins before the detached worker's first poll, not by a sleep.
    core.transition_to_shutdown();
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
    assert_eq!(core.0.state.lock().unwrap().completed_stage, None);
    assert!(core.0.state.lock().unwrap().finalization.is_some());
    assert_eq!(
        saved(&host).phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
    shutdown_joins_service(&mut host, witness).await;
    let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
    assert!(resources.lock().await.cleanup().await.is_ok());
    assert!(host.spawn_ledger.is_fenced());
}

struct CleanupService {
    stopped: tokio::sync::oneshot::Receiver<()>,
    release: tokio::sync::oneshot::Sender<()>,
    returned: tokio::sync::oneshot::Receiver<()>,
}
fn cleanup_service(host: &mut BootstrappedHost) -> CleanupService {
    let (stopped, stop_seen) = tokio::sync::oneshot::channel();
    let (release, held) = tokio::sync::oneshot::channel();
    let (returned, return_seen) = tokio::sync::oneshot::channel();
    host.services.start("cleanup-witness", |stop| async move {
        stop.await?;
        stopped.send(()).unwrap();
        held.await?;
        returned.send(()).unwrap();
        Ok(())
    });
    CleanupService {
        stopped: stop_seen,
        release,
        returned: return_seen,
    }
}
async fn shutdown_joins_service(host: &mut BootstrappedHost, witness: CleanupService) {
    witness.stopped.await.unwrap();
    shutdown_joins_return(host, witness.release, witness.returned).await;
}
async fn shutdown_joins_return(
    host: &mut BootstrappedHost,
    release: tokio::sync::oneshot::Sender<()>,
    returned: tokio::sync::oneshot::Receiver<()>,
) {
    use std::future::Future;
    let mut shutdown = Box::pin(host.shutdown());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(shutdown.as_mut().poll(cx).is_pending()))
            .await,
        "operator shutdown must wait for transferred service completion"
    );
    release.send(()).unwrap();
    returned.await.unwrap();
    assert!(
        shutdown.await.is_err(),
        "the restart failure remains reported"
    );
}

#[tokio::test]
async fn host_shutdown_reclaims_transferred_services_without_polling_the_finalizer() {
    use std::future::Future;
    for fail in [false, true] {
        let (_dir, mut host) = boot().await;
        let witness = cleanup_service(&mut host);
        host.services.start("cleanup-result", |stop| async move {
            stop.await?;
            if fail {
                Err("host-only cleanup failure".into())
            } else {
                Ok(())
            }
        });
        let core = RestartCoordinator::new(&host);
        let id = core.request_planned(BTreeMap::new()).unwrap();
        core.wait_sealed(id).await.unwrap();
        let tx = transaction(&host);
        // Split transfer from scheduling the coordinator driver. Shutdown alone
        // must reclaim resources, even if that driver is never polled.
        let _begun = core.capture_boundary(id, &tx).unwrap();
        let resources = host.take_planned_exit(&core.0.gate).unwrap();
        core.transition_to_shutdown();
        let mut shutdown = Box::pin(host.shutdown());
        // Drive shutdown while awaiting its stop receipt, not just one poll.
        tokio::select! {
            result = &mut shutdown => panic!("shutdown returned before service stop/join: {result:?}"),
            stopped = witness.stopped => stopped.unwrap(),
        }
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                shutdown.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        witness.release.send(()).unwrap();
        witness.returned.await.unwrap();
        let result = shutdown.await;
        let cleanup = resources.lock().await.cleanup().await;
        // No coordinator driver has failed the runtime attempt in this seam:
        // operator shutdown must report the actual reclamation result instead.
        if fail {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("host-only cleanup failure")
            );
            assert!(cleanup.unwrap_err().contains("host-only cleanup failure"));
        } else {
            result.unwrap();
            cleanup.unwrap();
        }
        assert_eq!(core.0.state.lock().unwrap().completed_stage, None);
        assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
    }
}
