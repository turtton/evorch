//! Production drain with real blocking workers, channel checkpoints and explicit
//! virtual-time advances. No elapsed wall time or test timeout decides correctness.
#![cfg(feature = "bootstrap-test-support")]
use super::*;
use std::{future::Future, task::Poll, time::Duration};

#[tokio::test]
async fn production_composition_storage_drain_budget_retains_cleanup_and_owner() {
    matrix("composition_drain_deadlines", 7, 0).await;
}

pub(in super::super) async fn run() {
    let current = Current::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(Observer(current.clone())),
    )
    .unwrap();
    for (phase, unknown_ack, finishes, shutdown) in [
        ("flush_begin", false, false, false),
        ("final_flush_begin", false, false, false),
        ("writer_barrier_begin", false, false, false),
        ("flush_begin", true, false, false),
        ("shared", false, false, false),
        ("shared", false, true, false),
        ("flush_begin", false, false, true),
    ] {
        eprintln!(
            "storage-drain {phase} unknown_ack={unknown_ack} finishes={finishes} shutdown={shutdown}"
        );
        let trace = Arc::new(Trace::default());
        *current.lock().unwrap() = Some(trace.clone());
        cell(trace, phase, unknown_ack, finishes, shutdown).await;
        *current.lock().unwrap() = None;
        println!("M2-CELL storage-drain {phase} {unknown_ack} {finishes} {shutdown}");
        std::io::stdout().flush().unwrap();
    }
}

async fn cell(
    trace: Arc<Trace>,
    phase: &'static str,
    unknown_ack: bool,
    finishes: bool,
    operator_shutdown: bool,
) {
    let model = Arc::new(Model::default());
    let (_dir, mut host) = boot_composition(model.clone(), true).await;
    let core = RestartCoordinator::new(&host);
    let (runs, id) = owned_chats(&host, &core, 3).await;
    let owner = host.owner.attach("active").unwrap();
    let shared = phase == "shared";
    let (entered, mut release) = trace.block(if shared { "flush_begin" } else { phase }, None);
    let later = shared.then(|| {
        (
            trace.block("subscriber_drain_begin", None),
            trace.block("writer_barrier_begin", None),
        )
    });
    if unknown_ack {
        // Drop the real reply sender after expiry. A negative/unknown ACK is not
        // evidence of termination: the retained task still must join the bridge.
        trace.state.lock().unwrap().panic_on = Some("flush_reply");
    }
    let tx = transaction(&host);
    // The production spawn_blocking task inhibits Tokio auto-advance while its
    // channel gate is held. Only explicit advance() crosses the drain deadline.
    tokio::time::pause();
    // Deliberately no target: stage 5 completion is not authority to exec.
    core.start_finalization_without_target(id, &mut host, tx)
        .unwrap();
    entered.await.unwrap();
    if let Some(((subscriber_entered, subscriber_release), (barrier_entered, barrier_release))) =
        later
    {
        // One shared budget, not a fresh allowance after each successful hop.
        tokio::time::advance(Duration::from_secs(4)).await;
        release.send(()).unwrap();
        subscriber_entered.await.unwrap();
        tokio::time::advance(Duration::from_secs(4)).await;
        subscriber_release.send(()).unwrap();
        barrier_entered.await.unwrap();
        tokio::time::advance(Duration::from_millis(1999)).await;
        assert!(!core.0.state.lock().unwrap().finalization_done);
        assert_eq!(
            core.0.state.lock().unwrap().completed_stage,
            Some(ExitStage::WorkersJoined)
        );
        release = barrier_release;
        if finishes {
            release.send(()).unwrap();
            assert!(core.wait_finalization(id).await.is_err());
            assert_eq!(
                core.0.state.lock().unwrap().completed_stage,
                Some(ExitStage::StorageDrained)
            );
            assert!(
                !saved(&host)
                    .failure_reason
                    .unwrap()
                    .contains("storage drain deadline")
            );
            let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
            assert!(resources.lock().await.cleanup().await.is_ok());
            verify_checkpoints(&host, &runs, false);
            core.transition_to_shutdown();
            assert!(host.shutdown().await.is_err());
            tokio::time::resume();
            return;
        }
        // Tokio rounds timer deadlines up to the next millisecond tick.
        tokio::time::advance(Duration::from_millis(2)).await;
    } else {
        tokio::time::advance(Duration::from_millis(10_001)).await;
    }
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert!(
        saved(&host)
            .failure_reason
            .unwrap()
            .contains("storage drain deadline")
    );
    assert_eq!(
        saved(&host).phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
    let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
    assert!(
        resources.try_lock().is_err(),
        "cleanup still owns the pending drain"
    );
    assert!(host.restart_producers().iter().all(|gate| !gate.is_open()));
    assert!(core.cancel(id).is_err());
    verify_checkpoints(&host, &runs, false);
    let retained = host.owner.attach("active").unwrap();
    assert_eq!(retained.lease.owner_id, owner.lease.owner_id);
    assert_eq!(retained.lease.generation, owner.lease.generation);
    assert_eq!(retained.state, runtime::ownership::OwnerState::Running);
    host.composition.storage_writer.barrier().unwrap();
    let expected = saved(&host).expected;
    if operator_shutdown {
        core.transition_to_shutdown();
        let mut shutdown = Box::pin(host.shutdown());
        std::future::poll_fn(|cx| {
            assert!(shutdown.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        release.send(()).unwrap();
        assert!(shutdown.await.is_err());
        // Explicit operator shutdown can replace mutable run contexts with
        // stopped snapshots, but must not rewrite the captured manifest receipts.
        assert_eq!(saved(&host).expected, expected);
    } else {
        let mut reclaimed = Box::pin(resources.lock());
        std::future::poll_fn(|cx| {
            assert!(reclaimed.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        release.send(()).unwrap();
        drop(reclaimed.await);
        verify_checkpoints(&host, &runs, false);
    }
    let mut cleanup = resources.lock().await;
    let result = cleanup.cleanup().await;
    assert_eq!(result.is_err(), unknown_ack);
    assert_eq!(
        cleanup.cleanup().await,
        result,
        "cleanup result stays sticky"
    );
    assert!(
        cleanup
            .first_five(|_| panic!("late success cannot restart M2"))
            .await
            .is_err()
    );
    assert_eq!(cleanup.completed(), Some(ExitStage::WorkersJoined));
    drop(cleanup);
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::WorkersJoined)
    );
    assert_eq!(
        model.0.load(Ordering::SeqCst),
        3,
        "old business never resumed"
    );
    host.composition.storage_writer.barrier().unwrap();
    {
        let events = &trace.state.lock().unwrap().events;
        assert!(events.iter().any(|e| e.0 == "subscriber_join_end"));
        assert!(events.iter().any(|e| e.0 == "bridge_worker_joined"));
        assert!(events.iter().all(|e| e.0 != "owner_stop_begin"));
    }
    if !operator_shutdown {
        core.transition_to_shutdown();
        assert!(host.shutdown().await.is_err());
    }
    tokio::time::resume();
}
