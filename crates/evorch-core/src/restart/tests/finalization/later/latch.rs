//! The syscall callback is inert here: channel checkpoints prove lock ordering
//! without scheduling sleeps, clock polling or a hidden production exec hook.
use super::*;
#[tokio::test]
async fn shutdown_wins_before_raw_boundary() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let id = core.request().unwrap();
    core.wait_sealed(id).await.unwrap();
    {
        core.0.state.lock().unwrap().phase = RestartPhase::Finalizing;
    }
    core.transition_to_shutdown();
    let called = std::sync::atomic::AtomicBool::new(false);
    assert_eq!(
        core.with_exec_latches(id, || called.store(true, Ordering::SeqCst)),
        None
    );
    assert!(!called.load(Ordering::SeqCst));
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn raw_boundary_serializes_shutdown_and_return_releases_every_latch() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let id = core.request().unwrap();
    core.wait_sealed(id).await.unwrap();
    {
        core.0.state.lock().unwrap().phase = RestartPhase::Finalizing;
    }
    let attempt = core.0.state.lock().unwrap().attempt.clone().unwrap();
    let (entered, receive) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let raw_core = core.clone();
    let raw = tokio::task::spawn_blocking(move || {
        raw_core.with_exec_latches(id, || {
            entered.send(()).unwrap();
            wait.recv().unwrap();
            5
        })
    });
    receive.await.unwrap();
    // This is the exact synchronous lock used by transition_to_shutdown. Failure
    // to acquire is direct evidence of serialization, not a timing inference.
    assert!(matches!(
        core.0.state.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    release.send(()).unwrap();
    assert_eq!(raw.await.unwrap(), Some(5));
    // Both guards have been released on return (the real exec-failure branch).
    assert_eq!(
        host.runtime.restart_attempt_state(&attempt),
        runtime::quiesce::RestartAttemptState::Active
    );
    core.transition_to_shutdown();
    assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
    assert_eq!(core.with_exec_latches(id, || 6), None);
    host.shutdown().await.unwrap();
}
