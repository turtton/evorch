use super::*;
use crate::quiesce::fixture::Fixture;
use std::sync::{Barrier, mpsc};

#[tokio::test]
async fn foreign_sync_parent_does_not_hide_same_runtime_async_lineage() {
    let own = Arc::new(DispatchTails::default());
    let foreign = Arc::new(DispatchTails::default());
    let parent = own.register(false).unwrap();
    let other = foreign.register(false).unwrap();
    own.state.lock().unwrap().closed = true;
    parent
        .scope(async {
            let child = other.enter(|| own.register(false)).unwrap();
            assert_eq!(own.state.lock().unwrap().count, 2);
            drop(child);
        })
        .await;
    own.idle().await;
    assert!(own.result().is_ok());
}

#[tokio::test]
async fn pre_spawn_guard_and_descendant_keep_tail_drain_pending_until_both_complete() {
    let tails = Arc::new(DispatchTails::default());
    let parent = tails.register(false).unwrap();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    let task = parent.enter(|| {
        let child = tails.register(false).unwrap();
        tokio::spawn(child.scope(async move {
            entered.send(()).unwrap();
            resume.await.unwrap();
        }))
    });
    assert_eq!(tails.state.lock().unwrap().count, 2);
    drop(parent);
    started.await.unwrap();
    let idle = tails.idle();
    tokio::pin!(idle);
    assert!(futures_util::poll!(&mut idle).is_pending());
    release.send(()).unwrap();
    task.await.unwrap();
    idle.await;
    assert!(tails.try_close());
    assert!(tails.try_close());
    assert!(tails.register(false).is_err());
    assert!(tails.result().is_ok());
}
#[test]
fn close_and_root_registration_have_one_atomic_winner() {
    for _ in 0..16 {
        let tails = Arc::new(DispatchTails::default());
        let start = Arc::new(Barrier::new(3));
        let (release, resume) = mpsc::channel();
        let (registered, registration) = mpsc::channel();
        let t = tails.clone();
        let s = start.clone();
        let root = std::thread::spawn(move || {
            s.wait();
            let guard = t.register(false);
            registered.send(guard.is_ok()).unwrap();
            resume.recv().unwrap();
            drop(guard);
        });
        let t = tails.clone();
        let s = start.clone();
        let closing = std::thread::spawn(move || {
            s.wait();
            t.try_close()
        });
        start.wait();
        let closed = closing.join().unwrap();
        let registered = registration.recv().unwrap();
        assert_ne!(closed, registered);
        release.send(()).unwrap();
        root.join().unwrap();
        assert!(tails.try_close());
        assert!(tails.result().is_ok());
    }
}
#[test]
fn descendant_fork_preserves_lineage_even_if_close_has_been_requested() {
    let tails = Arc::new(DispatchTails::default());
    let parent = tails.register(false).unwrap();
    // Explicit tracker close request while a registered parent still owns work.
    tails.state.lock().unwrap().closed = true;
    let child = parent.enter(|| tails.register(false).unwrap());
    drop(parent);
    assert_eq!(tails.state.lock().unwrap().count, 1);
    drop(child);
    assert!(tails.result().is_ok());
    assert!(tails.register(true).is_err());
    assert!(tails.result().is_err());
    assert!(tails.try_close());
    assert!(tails.result().is_err());
}
#[tokio::test]
async fn aborted_or_panicking_tail_is_a_sticky_failure_not_idle_success() {
    let tails = Arc::new(DispatchTails::default());
    let guard = tails.register(false).unwrap();
    let (entered, started) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(guard.scope(async move {
        entered.send(()).unwrap();
        std::future::pending::<()>().await;
    }));
    started.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tails.idle().await;
    assert!(tails.result().is_err());
    assert!(tails.result().is_err());
    let tails = Arc::new(DispatchTails::default());
    let guard = tails.register(false).unwrap();
    let result = std::panic::catch_unwind(|| {
        let _guard = guard;
        panic!("injected tail panic");
    });
    assert!(result.is_err());
    assert!(tails.result().is_err());
}
#[tokio::test]
async fn parked_run_is_not_a_tail_and_shutdown_stop_tracks_its_terminal_continuation() {
    let f = Fixture::new(0);
    let run = f.start();
    f.runtime.quiesce().await.unwrap();
    assert_eq!(
        f.runtime.shared.dispatch_tails.state.lock().unwrap().count,
        0
    );
    // Operator shutdown after a mere quiesce is supported (no restart close yet).
    let fence = f
        .runtime
        .fence_dispatch_shutdown(|| async { Ok::<(), String>(()) })
        .await
        .unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    assert_eq!(
        f.runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert_eq!(
        f.intent(run).stop_reason,
        Some(crate::quiesce::StopReason::OperatorStopped)
    );
}
#[tokio::test]
async fn restart_drain_finishes_with_a_parked_run_without_claiming_it_is_terminal() {
    let f = Fixture::new(0);
    let run = f.start();
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    assert!(f.intent(run).active);
    assert!(matches!(
        f.runtime.inspect_agent(run).unwrap().phase,
        event_bus::AgentRunPhase::Pending
            | event_bus::AgentRunPhase::Running
            | event_bus::AgentRunPhase::Waiting
    ));
    // The caller now owns restart/exit. Dropping the fixture/runtime aborts no job
    // here; explicit cancel is just cleanup after testing the sealed boundary.
    f.cleanup(run).await;
}
#[tokio::test]
async fn non_lane_provider_admission_is_stopped_and_joined_by_dispatch_drain() {
    let f = Fixture::with_admission(0, true);
    let run = f.start();
    f.model.admission_entered.notified().await;
    let fence = f
        .runtime
        .fence_dispatch_shutdown(|| async { Ok::<(), String>(()) })
        .await
        .unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    assert!(!f.intent(run).active);
    assert_eq!(f.model.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        f.intent(run).stop_reason,
        Some(crate::quiesce::StopReason::OperatorStopped)
    );
}
#[tokio::test]
async fn foreign_fence_is_rejected_without_poisoning_and_flush_failure_is_sticky() {
    let a = Fixture::new(0);
    let b = Fixture::new(0);
    let fence = a
        .runtime
        .fence_dispatch_shutdown(|| async { Ok::<(), String>(()) })
        .await
        .unwrap();
    assert!(b.runtime.drain_dispatch_tails(&fence).await.is_err());
    assert!(b.runtime.shared.dispatch_tails.result().is_ok());
    a._storage.close();
    assert!(a.runtime.drain_dispatch_tails(&fence).await.is_err());
    assert!(a.runtime.drain_dispatch_tails(&fence).await.is_err());
}

#[test]
fn tail_aborted_before_its_first_poll_cannot_look_like_successful_drain() {
    let tails = Arc::new(DispatchTails::default());
    let guard = tails.register(false).unwrap();
    let future = guard.scope(std::future::pending::<()>());
    drop(future);
    assert!(tails.try_close());
    assert!(tails.result().is_err());
}

#[tokio::test]
async fn reopen_before_seal_preserves_live_tail_and_failure() {
    let tails = Arc::new(DispatchTails::default());
    let parent = tails.register(false).unwrap();
    let epoch = tails.epoch();
    tails.reopen().unwrap();
    let child = parent.fork();
    assert_eq!(tails.epoch(), epoch);
    assert_eq!(tails.state.lock().unwrap().count, 2);
    drop(parent);
    assert!(!tails.try_close());
    drop(child);
    tails.idle().await;
    assert!(tails.result().is_ok());
    assert!(tails.try_close());
    tails.state.lock().unwrap().drained = true;
    assert!(tails.register(true).is_err());
    tails.reopen().unwrap();
    assert_ne!(tails.epoch(), epoch);
    assert!(
        tails.result().is_err(),
        "reopen must never clear a required-tail failure"
    );
}
