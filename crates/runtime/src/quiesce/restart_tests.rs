use super::fixture::Fixture;
use super::*;
use crate::runtime::HookPoint;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn preflight_failure_does_not_park_or_clear_storage_health() {
    let f = Fixture::new(0);
    f.runtime.shared.quiesce.fail("existing failure");
    assert!(f.runtime.begin_restart_quiesce().is_err());
    assert!(f.runtime.shared.quiesce.running());
    let run = f.start();
    f.model.entered.notified().await;
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        f.runtime.quiesce_state(),
        QuiesceState::Failed { .. }
    ));
    f.cleanup(run).await;
}

#[tokio::test]
async fn cancelling_park_invalidates_checkpoint_before_same_execution_resumes() {
    let f = Fixture::new(0);
    let run = f.start();
    let execution = f.runtime.execution_snapshot(run).unwrap();
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    assert_eq!(
        attempt.generation(),
        f.runtime.begin_restart_quiesce().unwrap().generation()
    );
    f.runtime.wait_quiesced().await.unwrap();
    assert!(f.intent(run).checkpoint.is_some());
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    assert!(f.intent(run).checkpoint.is_none());
    assert!(f.intent(run).snapshot.is_some());
    assert!(std::sync::Arc::ptr_eq(
        &execution,
        &f.runtime.execution_snapshot(run).unwrap()
    ));
    f.model.entered.notified().await;
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    assert!(f.runtime.cancel_restart_quiesce(&attempt).is_err());
    f.cleanup(run).await;
}

#[tokio::test]
async fn failed_checkpoint_invalidation_keeps_park_and_failure_sticky() {
    let f = Fixture::new(0);
    let run = f.start();
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    f.runtime.wait_quiesced().await.unwrap();
    *f.runtime.shared.quiesce.fault.lock().unwrap() = Some(StorageStep::Save);
    assert!(f.runtime.cancel_restart_quiesce(&attempt).is_err());
    assert!(!f.runtime.shared.quiesce.running());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    assert!(f.runtime.begin_restart_quiesce().is_err());
    assert!(f.runtime.cancel_restart_quiesce(&attempt).is_err());
    f.cleanup(run).await;
}

#[tokio::test]
async fn stop_after_capture_invalidates_capture_without_resuming_other_parked_turns() {
    let f = Fixture::new(0);
    let target = f.start();
    let other = f.start();
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    f.runtime.begin_recovery_capture(&fence).unwrap();
    f.runtime.stop(target, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(target).await.unwrap();
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    assert!(!f.runtime.shared.quiesce.running());
    assert!(f.intent(other).checkpoint.is_some());
    assert!(f.runtime.cancel_restart_quiesce(&attempt).is_err());
    assert!(!f.runtime.shared.dispatch_tails.capture_valid());
    assert!(f.runtime.recovery_plan().is_err());
    assert!(f.runtime.begin_recovery_capture(&fence).is_err());
    assert!(f.runtime.save_recovery_graph("invalidated", &[]).is_err());
    f.cleanup(other).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_stop_registers_terminal_tail_and_next_attempt_drains_it() {
    let f = Fixture::new(0);
    let run = f.start();
    f.runtime.begin_restart_quiesce().unwrap();
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    let execution = f.runtime.execution_snapshot(run).unwrap();
    let reached = std::sync::Arc::new(tokio::sync::Notify::new());
    let notify = reached.clone();
    let (release, held) = std::sync::mpsc::channel();
    f.runtime
        .on_execution(HookPoint::TerminalRegistered, &execution, move || {
            notify.notify_one();
            held.recv().unwrap();
        });
    f.runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    reached.notified().await;
    assert!(f.runtime.validate_restart_dispatch_fence(&fence).is_err());
    assert_eq!(f.runtime.quiesce_state(), QuiesceState::Running);
    let next = f.runtime.fence_dispatch_restart().await.unwrap();
    let drain = f.runtime.drain_dispatch_tails(&next);
    tokio::pin!(drain);
    assert!(futures_util::poll!(&mut drain).is_pending());
    release.send(()).unwrap();
    drain.await.unwrap();
    assert_eq!(
        f.runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn sealed_shutdown_never_grants_a_turn_resume() {
    let f = Fixture::new(0);
    let run = f.start();
    let restart = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&restart).await.unwrap();
    let shutdown = f
        .runtime
        .fence_dispatch_shutdown(|| async { Ok::<_, String>(()) })
        .await
        .unwrap();
    f.runtime.drain_dispatch_tails(&shutdown).await.unwrap();
    assert_eq!(
        f.runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn accepted_input_invalidates_the_current_ack_before_a_new_seal() {
    let f = Fixture::new(0);
    let run = f.start();
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    f.runtime.wait_quiesced().await.unwrap();
    let incarnation = f.intent(run).incarnation;
    assert!(f.runtime.shared.quiesce.acknowledged(run, &incarnation));
    f.runtime
        .queue_intent_input(
            run,
            DurableInput {
                id: "during-park".into(),
                text: "new input".into(),
                images: vec![],
                trusted_user: true,
            },
        )
        .unwrap();
    assert!(!f.runtime.shared.quiesce.acknowledged(run, &incarnation));
    assert!(!f.runtime.complete_restart_safe_points().unwrap());
    f.runtime.wait_quiesced().await.unwrap();
    assert!(f.runtime.shared.quiesce.acknowledged(run, &incarnation));
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    f.cleanup(run).await;
}

#[tokio::test]
async fn stop_before_sealing_keeps_attempt_and_other_parked_execution() {
    let f = Fixture::new(0);
    let target = f.start();
    f.model.entered.notified().await;
    let other = f.start();
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    let mut changes = f.runtime.subscribe_restart_state();
    while f.intent(other).checkpoint.is_none() {
        changes.changed().await.unwrap();
    }
    f.runtime.stop(target, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(target).await.unwrap();
    assert_eq!(
        attempt.generation(),
        f.runtime.begin_restart_quiesce().unwrap().generation()
    );
    f.runtime.wait_quiesced().await.unwrap();
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    assert!(f.intent(other).checkpoint.is_some());
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    f.model.entered.notified().await;
    f.cleanup(other).await;
}
