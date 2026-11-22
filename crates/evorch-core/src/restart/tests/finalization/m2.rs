use super::*;
use tokio::sync::oneshot;

async fn start(host: &mut BootstrappedHost, core: &RestartCoordinator) -> RestartId {
    let id = core.request_planned(BTreeMap::new()).unwrap();
    core.wait_sealed(id).await.unwrap();
    let tx = transaction(host);
    core.start_finalization_without_target(id, host, tx)
        .unwrap();
    id
}

async fn completed(core: &RestartCoordinator, stage: ExitStage) {
    let mut changed = core.subscribe();
    loop {
        {
            let state = core.0.state.lock().unwrap();
            if state.completed_stage.is_some_and(|done| done >= stage) {
                return;
            }
            assert!(!state.finalization_done, "finalizer ended before {stage:?}");
        }
        changed.changed().await.unwrap();
    }
}

async fn failed(
    host: &mut BootstrappedHost,
    core: &RestartCoordinator,
    id: RestartId,
    last: Option<ExitStage>,
) {
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert_eq!(core.0.state.lock().unwrap().completed_stage, last);
    assert!(core.0.state.lock().unwrap().finalization.is_some());
    assert_eq!(
        saved(host).phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
    assert!(host.restart_producers().iter().all(|gate| !gate.is_open()));
    assert!(core.cancel(id).is_err());
    core.transition_to_shutdown();
    assert!(host.shutdown().await.is_err());
}

#[cfg(target_os = "linux")]
#[path = "guard_lost.rs"]
mod guard_lost;

// Real process I/O uses the real clock; never start_paused here.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn unconfirmed_reap_blocks_worker_join_and_storage_drain() {
    if !guard_lost::controller() {
        return;
    }
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let _family = guard_lost::lose_guard(&host.spawn_ledger).await;
    assert_eq!(host.spawn_ledger.child_stats().unconfirmed, 1);
    let id = start(&mut host, &core).await;
    failed(&mut host, &core, id, Some(ExitStage::StopRequested)).await;
    assert!(
        saved(&host)
            .failure_reason
            .unwrap()
            .contains("reap unconfirmed")
    );
}

#[tokio::test(start_paused = true)]
async fn ledger_worker_panic_is_not_treated_as_a_successful_join() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let ledger = host.spawn_ledger.clone();
    let mut workers = ledger.watch_workers();
    ledger
        .admit()
        .unwrap()
        .spawn_worker("panicked", async { panic!("owned worker failed") });
    workers.wait_for(|state| state.live == 0).await.unwrap();
    assert_eq!(ledger.worker_stats().panicked, 1);
    let id = start(&mut host, &core).await;
    failed(&mut host, &core, id, Some(ExitStage::ChildrenReaped)).await;
    assert!(
        saved(&host)
            .failure_reason
            .unwrap()
            .contains("ledger worker panicked")
    );
}

#[tokio::test(start_paused = true)]
async fn background_join_panic_blocks_bridge_flush() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    host.services.start("panic-on-stop", |stop| async move {
        stop.await?;
        panic!("owned background worker failed");
    });
    let id = start(&mut host, &core).await;
    failed(&mut host, &core, id, Some(ExitStage::ChildrenReaped)).await;
    assert!(
        saved(&host)
            .failure_reason
            .unwrap()
            .contains("panic-on-stop")
    );
}

#[tokio::test(start_paused = true)]
async fn bridge_flush_failure_never_completes_stage_five_or_releases_writer() {
    let (_dir, mut host) = boot().await;
    let witness = cleanup_service(&mut host);
    let core = RestartCoordinator::new(&host);
    let sql = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    sql.execute_batch(
        "CREATE TRIGGER reject_final_event BEFORE INSERT ON events
         BEGIN SELECT RAISE(ABORT,'final event rejected'); END;",
    )
    .unwrap();
    host.runtime.event_bus().emit(event_bus::Event::new(
        event_bus::MessageEvent::ReasoningDelta {
            run_id: None,
            delta: "last event".into(),
        },
    ));
    let id = start(&mut host, &core).await;
    witness.stopped.await.unwrap();
    witness.release.send(()).unwrap();
    witness.returned.await.unwrap();
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::WorkersJoined)
    );
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert_eq!(
        saved(&host).phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
    host.composition.storage_writer.barrier().unwrap();
    sql.execute_batch("DROP TRIGGER reject_final_event;")
        .unwrap();
    core.transition_to_shutdown();
    assert!(host.shutdown().await.is_err());
    let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
    // The same cleanup owner completed after joining the normally returned service.
    let first = resources.lock().await.cleanup().await;
    assert_eq!(resources.lock().await.cleanup().await, first);
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::WorkersJoined)
    );
}

#[tokio::test(start_paused = true)]
async fn join_deadline_retains_cleanup_and_late_return_cannot_advance_stage_five() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let (stopped, stop_seen) = oneshot::channel();
    let (release, held) = oneshot::channel();
    let (returned, return_seen) = oneshot::channel();
    host.services.start("held-service", |stop| async move {
        stop.await?;
        stopped.send(()).unwrap();
        held.await?;
        returned.send(()).unwrap();
        Ok(())
    });
    let id = start(&mut host, &core).await;
    stop_seen.await.unwrap();
    completed(&core, ExitStage::ChildrenReaped).await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::ChildrenReaped)
    );
    assert!(
        saved(&host)
            .failure_reason
            .unwrap()
            .contains("join deadline")
    );
    // Shutdown must observe the same retained cleanup, not merely an Err latch.
    core.transition_to_shutdown();
    shutdown_joins_return(&mut host, release, return_seen).await;
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::ChildrenReaped)
    );
    let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
    assert!(resources.lock().await.cleanup().await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn shutdown_while_spawn_fence_waits_is_terminal_only_and_preserves_cleanup_owner() {
    use std::future::Future;
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let (run, id) = planned_chat(&host, &core).await;
    let ledger = host.spawn_ledger.clone();
    let admitted = ledger.admit().unwrap();
    let mut fence = ledger.watch_fence();
    let tx = transaction(&host);
    core.start_finalization_without_target(id, &mut host, tx)
        .unwrap();
    fence
        .wait_for(|state| *state == sandbox::ledger::FenceState::Closing)
        .await
        .unwrap();
    assert_eq!(core.snapshot().phase, RestartPhase::Finalizing);
    assert!(matches!(
        host.runtime.stop(run, StopScope::SelfOnly),
        Err(RuntimeError::RestartBusy)
    ));
    let mut waiter = Box::pin(core.wait_finalization(id));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiter.as_mut().poll(cx).is_pending()))
            .await
    );
    drop(waiter);
    core.transition_to_shutdown();
    host.runtime.stop(run, StopScope::SelfOnly).unwrap();
    assert_eq!(
        host.runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    drop(admitted);
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::SpawnFenceClosed)
    );
    assert!(core.0.state.lock().unwrap().finalization.is_some());
    assert!(host.shutdown().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_worker_join_prevents_storage_stage_and_never_resumes() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let (stopped, stop_seen) = oneshot::channel();
    let (release, held) = oneshot::channel();
    host.services.start("held-service", |stop| async move {
        stop.await?;
        stopped.send(()).unwrap();
        held.await?;
        Ok(())
    });
    let id = start(&mut host, &core).await;
    stop_seen.await.unwrap();
    completed(&core, ExitStage::ChildrenReaped).await;
    core.transition_to_shutdown();
    release.send(()).unwrap();
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::WorkersJoined)
    );
    assert!(!host.runtime.learning_gate().is_open());
    assert!(host.shutdown().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn final_business_event_and_usage_are_durable_without_periodic_flush() {
    use event_bus::{Event, MessageEvent, ProviderEvent};
    let (_dir, mut host) = boot_composition(Arc::new(Model::default()), true).await;
    let core = RestartCoordinator::new(&host);
    let event = Event::new(MessageEvent::ReasoningDelta {
        run_id: None,
        delta: "final business delta".into(),
    });
    let bus = host.runtime.event_bus();
    let tail = event.clone();
    host.services.start("last-event", |stop| async move {
        stop.await?;
        bus.emit(tail);
        let mut buffered = Event::new(event_bus::UsageEvent::Usage {
            provider: "fixture".into(),
            model: "final-metrics".into(),
            input_tokens: 11,
            output_tokens: 5,
            cache_read_tokens: 4,
            cache_write_tokens: 2,
        });
        buffered.meta.wall_clock = std::time::UNIX_EPOCH + std::time::Duration::from_secs(125);
        bus.emit(buffered);
        bus.emit(Event::new(ProviderEvent::RequestCompleted {
            usage_reported: true,
            request_id: "final-request".into(),
            provider: "fixture".into(),
            profile: None,
            protocol: "openai-chat-completions".into(),
            model: "final-model".into(),
            streaming: false,
            duration_ms: 0,
            input_tokens: 7,
            output_tokens: 3,
            cache_read_tokens: 2,
            cache_write_tokens: 1,
            finish_reason: "stop".into(),
            service_tier: None,
            run_id: Some("final-run".into()),
            reasoning_tokens: None,
            purpose: None,
        }));
        Ok(())
    });
    let id = start(&mut host, &core).await;
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::StorageDrained)
    );
    // Independent DB reader proves the ACK, not a mocked counter.
    let db = storage::Database::open(&host.composition.storage).unwrap();
    assert!(
        db.events_all_ordered()
            .unwrap()
            .iter()
            .any(|row| row.event == event)
    );
    let usage = db.usage_requests_for_runs(&["final-run".into()]).unwrap();
    let row = usage
        .iter()
        .find(|row| row.record.request_id == "final-request")
        .unwrap();
    assert_eq!(
        (
            row.record.input_tokens,
            row.record.output_tokens,
            row.record.cache_read_tokens,
            row.record.cache_write_tokens
        ),
        (7, 3, 2, 1)
    );
    let buckets = db.metrics_range(120, 180).unwrap();
    let bucket = buckets
        .iter()
        .find(|row| row.key.model == "final-metrics")
        .unwrap();
    assert_eq!(
        (
            bucket.input_tokens,
            bucket.output_tokens,
            bucket.cache_read_tokens,
            bucket.cache_write_tokens,
            bucket.request_count
        ),
        (11, 5, 4, 2, 1)
    );
    host.composition.storage_writer.barrier().unwrap();
    core.transition_to_shutdown();
    assert!(host.shutdown().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn busy_writer_barrier_stays_failed_closed_after_the_snapshot_is_released() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let reader = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    // This read snapshot is held until the FULL checkpoint reports busy. There
    // is no timing race or delayed release: correctness depends on the SQL lock.
    let id = start(&mut host, &core).await;
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::WorkersJoined)
    );
    assert!(
        saved(&host)
            .failure_reason
            .unwrap()
            .contains("WAL flush incomplete")
    );
    reader.execute_batch("ROLLBACK").unwrap();
    host.composition.storage_writer.barrier().unwrap();
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::WorkersJoined)
    );
    let tx = saved(&host);
    assert!(
        core.start_finalization_without_target(id, &mut host, tx)
            .is_err()
    );
    core.transition_to_shutdown();
    assert!(host.shutdown().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn atomic_boundary_does_not_take_the_host_stop_mutex_and_shutdown_cannot_strand_resources() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let (run, id) = planned_chat(&host, &core).await;
    let tx = transaction(&host);
    host.validate_planned_exit(&core.0.gate).unwrap();
    let shared_host = host.host.clone();
    let begun = {
        let command = shared_host.lock().unwrap();
        // Model the actual host Stop lock order, not just the direct runtime API.
        // Capture's memory-only boundary must work while this host lock is held.
        let begun = core.capture_boundary(id, &tx).unwrap();
        let handler = command.service().restart_stop_handler().unwrap();
        assert!(matches!(
            handler.stop(run, StopScope::SelfOnly),
            Err(RuntimeError::RestartBusy)
        ));
        core.transition_to_shutdown();
        begun
    };
    core.own_finalizer(id, &mut host, begun, tx, None);
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
    assert_eq!(core.0.state.lock().unwrap().completed_stage, None);
    assert!(core.0.state.lock().unwrap().finalization.is_some());
    assert_eq!(
        saved(&host).phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
    assert!(host.shutdown().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn cleanup_reports_late_join_failure_and_still_joins_remaining_services() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let (stopped, stop_seen) = oneshot::channel();
    let (release, held) = oneshot::channel();
    let (returned, return_seen) = oneshot::channel();
    host.services.start("late-error", |stop| async move {
        stop.await?;
        stopped.send(()).unwrap();
        held.await?;
        Err("late service failure".into())
    });
    host.services.start("after-late-error", |stop| async move {
        stop.await?;
        returned.send(()).unwrap();
        Ok(())
    });
    let id = start(&mut host, &core).await;
    stop_seen.await.unwrap();
    completed(&core, ExitStage::ChildrenReaped).await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(core.wait_finalization(id).await.is_err());
    core.transition_to_shutdown();
    shutdown_joins_return(&mut host, release, return_seen).await;
    let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
    let error = resources.lock().await.cleanup().await.unwrap_err();
    assert!(error.contains("late service failure"), "{error}");
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::ChildrenReaped)
    );
}
