use super::*;
use runtime::quiesce::HostCommandPermit;

async fn audit_after_host_drain(core: &RestartCoordinator) -> (RestartId, HostCommandPermit) {
    let gate = core.0.gate.clone();
    let (sent, received) = tokio::sync::oneshot::channel();
    *core.0.after_host_drain.lock().unwrap() = Some(Box::new(move || {
        assert_eq!(gate.phase(), HostRestartPhase::Sealing);
        // The real admission API, at the exact gap after drained() returned.
        assert!(sent.send(gate.enter_admin_audit().unwrap()).is_ok());
    }));
    let id = core.request_planned(BTreeMap::new()).unwrap();
    (id, received.await.unwrap())
}

async fn assert_waiting_for_audit(
    core: &RestartCoordinator,
    id: RestartId,
    tx: &RestartTransaction,
) {
    let mut sealed = Box::pin(core.wait_sealed(id));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(sealed.as_mut().poll(cx).is_pending()))
            .await
    );
    assert_eq!(core.snapshot().phase, RestartPhase::Sealing);
    let state = core.0.state.lock().unwrap();
    assert!(!state.capture_started);
    assert!(state.sealed.is_none());
    drop(state);
    assert!(core.capture_boundary(id, tx).is_err());
}

#[tokio::test]
async fn late_audit_after_host_drain_retries_seal_then_allows_capture() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let tx = transaction(&host);
    let (id, permit) = audit_after_host_drain(&core).await;
    assert_waiting_for_audit(&core, id, &tx).await;
    let deadline = core.0.state.lock().unwrap().preparation_deadline;
    // A reply clone going away is not callback completion. A second accepted
    // write must also be drained, even when the first release notifies seal.
    let callback_permit = permit.clone();
    drop(permit);
    let second = core.0.gate.enter_admin_audit().unwrap();
    drop(callback_permit);
    assert_waiting_for_audit(&core, id, &tx).await;
    drop(second);
    core.wait_sealed(id).await.unwrap();
    assert_eq!(core.0.state.lock().unwrap().preparation_deadline, deadline);
    assert_eq!(core.snapshot().id, Some(id));
    assert!(core.0.gate.enter_admin_audit().is_err());
    let begun = core.capture_boundary(id, &tx).unwrap();
    assert_eq!(core.snapshot().phase, RestartPhase::Capturing);
    // The admission shared by background and request writes remains closed:
    // neither kind can enqueue a new command after capture.
    assert!(core.0.gate.enter_admin_audit().is_err());
    core.own_finalizer(id, &mut host, begun, tx, None);
    // This fixture deliberately has no exec target. Reaching its durable
    // manifest proves capture succeeded; its later finalization fails closed.
    assert!(core.wait_finalization(id).await.is_err());
    assert!(saved(&host).manifest_digest.is_some());
    core.transition_to_shutdown();
    assert!(host.shutdown().await.is_err());
}

#[tokio::test]
async fn late_audit_after_host_drain_expires_to_running_without_capture() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let tx = transaction(&host);
    let (id, permit) = audit_after_host_drain(&core).await;
    assert_waiting_for_audit(&core, id, &tx).await;
    // Bootstrap/preflight really finished before pausing; no blocking I/O is
    // synchronized by virtual time. Only the existing deadline is advanced.
    tokio::time::pause();
    let deadline = core.0.state.lock().unwrap().preparation_deadline.unwrap();
    tokio::time::advance(
        deadline - tokio::time::Instant::now() - std::time::Duration::from_secs(1),
    )
    .await;
    assert_waiting_for_audit(&core, id, &tx).await;
    // Repeated drain notifications do not grant a fresh 300-second budget.
    drop(core.0.gate.enter_admin_audit().unwrap());
    assert_eq!(
        core.0.state.lock().unwrap().preparation_deadline,
        Some(deadline)
    );
    // Tokio rounds timers up to milliseconds. The manually paused origin may
    // be fractional, so cross the first tick after the absolute deadline.
    tokio::time::advance(
        deadline - tokio::time::Instant::now() + std::time::Duration::from_millis(1),
    )
    .await;
    phase(&core, RestartPhase::Running).await;
    assert_eq!(core.0.gate.phase(), HostRestartPhase::Open);
    assert!(!core.0.state.lock().unwrap().capture_started);
    assert!(core.0.state.lock().unwrap().sealed.is_none());
    assert!(core.wait_sealed(id).await.is_err());
    assert!(core.capture_boundary(id, &tx).is_err());
    assert!(saved(&host).manifest_digest.is_none());
    // The accepted callback may finish after cancellation, but cannot revive
    // the old attempt or manufacture a capture proof for a newer generation.
    drop(permit);
    core.0.gate.drained().await;
    assert_eq!(core.snapshot().phase, RestartPhase::Running);
    tokio::time::resume();
    let next = core.request_planned(BTreeMap::new()).unwrap();
    assert_ne!(next, id);
    core.wait_sealed(next).await.unwrap();
    assert!(core.wait_sealed(id).await.is_err());
    core.cancel(next).unwrap();
    host.shutdown().await.unwrap();
}
