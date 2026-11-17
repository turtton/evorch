use super::*;
use runtime::quiesce::QuiesceState;

async fn stop_after_shutdown(from: RestartPhase, direct: bool) {
    let model = Arc::new(Model::default());
    let (_dir, mut host) = boot_with_model(model.clone()).await;
    let core = RestartCoordinator::new(&host);
    let handler = host
        .host
        .lock()
        .unwrap()
        .service()
        .restart_stop_handler()
        .unwrap();
    let gate = host.host.lock().unwrap().service().restart_gate();
    // These accepted permits are barriers: the preparation cannot advance beyond
    // WaitingSafePoint / Sealing until the test explicitly releases its permit.
    let accepted_host = (from == RestartPhase::Sealing).then(|| {
        gate.enter(workbench_host::bootstrap::restart::CommandAccess::ExistingInput)
            .unwrap()
    });
    let producer = host.runtime.producer_gate();
    let mut accepted_producer = Some(producer.try_enter().unwrap());
    let target = idle_run(&host.runtime, runtime::Role::Worker, "shutdown Stop target").await;
    let id = core.request().unwrap();
    phase(&core, RestartPhase::WaitingSafePoint).await;
    let queued = host.runtime.delegate_background(
        runtime::Role::Worker,
        "must not execute during operator shutdown".into(),
        Default::default(),
    );
    if from == RestartPhase::Sealing {
        drop(accepted_producer.take());
        phase(&core, RestartPhase::Sealing).await;
    }
    assert_eq!(core.snapshot().phase, from);
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let attempt = core.0.state.lock().unwrap().attempt.clone().unwrap();
    core.transition_to_shutdown();
    let runtime_phase = host.runtime.quiesce_state();
    let result = if direct {
        host.runtime.stop(target, StopScope::SelfOnly)
    } else {
        handler.stop(target, StopScope::SelfOnly)
    };
    result.unwrap();
    assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
    assert_eq!(gate.phase(), HostRestartPhase::ShuttingDown);
    // Terminal intent writes invalidate a completed checkpoint, not the shutdown
    // decision. They must never transition runtime back to Running/replay.
    let expected_phase = match runtime_phase {
        QuiesceState::Checkpointed => QuiesceState::WaitingForSafePoints,
        other => other,
    };
    assert_eq!(host.runtime.quiesce_state(), expected_phase);
    assert_eq!(
        host.runtime.restart_attempt_state(&attempt),
        runtime::quiesce::RestartAttemptState::ShuttingDown
    );
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!host.runtime.learning_gate().is_open());
    assert!(
        !host
            .restart_preparing_services()
            .evaluations
            .producers
            .is_open()
    );
    assert!(core.wait_sealed(id).await.is_err());
    assert!(core.cancel(id).is_err());
    drop(accepted_producer);
    drop(accepted_host);
    assert!(producer.enter().await.is_none());
    let mut waiting = Box::pin(host.runtime.wait(queued));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(waiting);
    host.shutdown().await.unwrap();
    assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
    assert_eq!(gate.phase(), HostRestartPhase::ShuttingDown);
    assert_eq!(
        host.runtime.wait(target).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    // This monotonic count covers the entire interval, including the host drain.
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn waiting_safe_point_shutdown_keeps_host_stop_terminal_only() {
    stop_after_shutdown(RestartPhase::WaitingSafePoint, false).await;
}
#[tokio::test]
async fn waiting_safe_point_shutdown_keeps_direct_stop_terminal_only() {
    stop_after_shutdown(RestartPhase::WaitingSafePoint, true).await;
}
#[tokio::test]
async fn sealing_shutdown_keeps_host_stop_terminal_only() {
    stop_after_shutdown(RestartPhase::Sealing, false).await;
}
#[tokio::test]
async fn sealing_shutdown_keeps_direct_stop_terminal_only() {
    stop_after_shutdown(RestartPhase::Sealing, true).await;
}
