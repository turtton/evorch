use super::fixture::Fixture;
use super::*;
use crate::RunConfig;
use std::sync::{Arc, atomic::Ordering};

#[tokio::test]
async fn queued_launch_claims_the_same_execution_and_waiter_once() {
    for admission in [false, true] {
        let f = Fixture::with_admission(0, admission);
        let attempt = f.runtime.begin_restart_quiesce().unwrap();
        let run = f.start();
        let execution = f.runtime.execution_snapshot(run).unwrap();
        let mut waiter = Box::pin(f.runtime.wait_admission(run));
        assert!(futures_util::poll!(waiter.as_mut()).is_pending());
        assert_eq!(f.intent(run).wait, ResumeWait::AdmissionQueued);
        assert!(f.runtime.list_agents().is_empty());
        assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 0);
        f.runtime.cancel_restart_quiesce(&attempt).unwrap();
        assert_eq!(f.intent(run).wait, ResumeWait::Admission);
        if admission {
            f.model.admission_entered.notified().await;
            assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 1);
            f.model.admission_release.add_permits(1);
        }
        waiter.await.unwrap();
        f.model.entered.notified().await;
        assert!(Arc::ptr_eq(
            &execution,
            &f.runtime.execution_snapshot(run).unwrap()
        ));
        assert!(f.runtime.cancel_restart_quiesce(&attempt).is_err());
        assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
        f.cleanup(run).await;
    }
}

#[tokio::test]
async fn failed_queued_claim_settles_the_existing_admission_without_provider() {
    let f = Fixture::with_admission(0, true);
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    let run = f.start();
    *f.runtime.shared.quiesce.fault.lock().unwrap() = Some(StorageStep::Save);
    assert!(f.runtime.cancel_restart_quiesce(&attempt).is_err());
    assert!(f.runtime.wait_admission(run).await.is_err());
    assert!(f.runtime.list_agents().is_empty());
    assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 0);
    assert!(!f.runtime.shared.quiesce.running());
}

#[tokio::test]
async fn stopped_or_cancelled_queued_payload_is_never_claimed() {
    for seal in [false, true] {
        for cancel in [false, true] {
            let f = Fixture::with_admission(0, true);
            let attempt = f.runtime.begin_restart_quiesce().unwrap();
            let run = f.start();
            if seal {
                let fence = f.runtime.fence_dispatch_restart().await.unwrap();
                f.runtime.drain_dispatch_tails(&fence).await.unwrap();
            }
            if cancel {
                f.runtime.cancel(run).unwrap();
            } else {
                f.runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
            }
            if !seal {
                f.runtime.cancel_restart_quiesce(&attempt).unwrap();
            }
            assert!(f.runtime.wait_admission(run).await.is_err());
            assert!(f.runtime.list_agents().is_empty());
            assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn queued_lane_root_leaves_pending_admissions_on_registration() {
    let f = Fixture::with_admission(0, true);
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    let run = RunId::new(4401);
    f.runtime
        .spawn_lane_root(
            run,
            "queued-lane",
            "lane prompt".into(),
            RunConfig {
                lane_scope: Some(event_bus::LaneScope::Lane),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(f.runtime.pending_lane_admissions(), vec![run]);
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    f.model.admission_entered.notified().await;
    f.model.admission_release.add_permits(1);
    f.runtime.wait_admission(run).await.unwrap();
    assert!(f.runtime.pending_lane_admissions().is_empty());
    f.cleanup(run).await;
    f.runtime.drain_lane_admissions().await.unwrap();
}

#[tokio::test]
async fn manual_resume_is_rejected_without_effects_and_requires_explicit_retry() {
    let f = Fixture::new(0);
    let run =
        f.runtime
            .delegate_background(crate::Role::Worker, "done".into(), RunConfig::default());
    f.model.entered.notified().await;
    f.runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(run).await.unwrap();
    let execution = f.runtime.execution_snapshot(run).unwrap();
    let intents = f.runtime.resume_intents().unwrap();
    let thread = f.runtime.goal_thread(run);
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    let observed = f.runtime.observe_chat_resume(run);
    assert_eq!(
        f.runtime.resume_chat(run, RunConfig::default()),
        Err(crate::RuntimeError::RestartBusy)
    );
    assert_eq!(
        f.runtime
            .resume_chat_observed(run, RunConfig::default(), observed),
        Err(crate::RuntimeError::RestartBusy)
    );
    assert_eq!(
        f.runtime
            .continue_goal(run, "manual".into(), RunConfig::default()),
        Err(crate::RuntimeError::RestartBusy)
    );
    assert_eq!(f.runtime.resume_intents().unwrap(), intents);
    assert_eq!(f.runtime.goal_thread(run), thread);
    assert!(Arc::ptr_eq(
        &execution,
        &f.runtime.execution_snapshot(run).unwrap()
    ));
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.runtime.resume_intents().unwrap(), intents);
    f.runtime.resume_chat(run, RunConfig::default()).unwrap();
    f.model.entered.notified().await;
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 2);
    assert!(!Arc::ptr_eq(
        &execution,
        &f.runtime.execution_snapshot(run).unwrap()
    ));
    f.cleanup(run).await;
}

// Block the original admission task outside I/A/R, after provider admit succeeds.
async fn admitted_before_registration(f: &Fixture) -> (RunId, std::sync::mpsc::Sender<()>) {
    let run = f.start();
    f.model.admission_entered.notified().await;
    let execution = f.runtime.execution_snapshot(run).unwrap();
    let reached = Arc::new(tokio::sync::Notify::new());
    let signal = reached.clone();
    let (release, held) = std::sync::mpsc::channel();
    f.runtime.on_execution(
        crate::runtime::HookPoint::AdmissionReady,
        &execution,
        move || {
            signal.notify_one();
            tokio::task::block_in_place(|| held.recv().unwrap());
        },
    );
    f.model.admission_release.add_permits(1);
    reached.notified().await;
    (run, release)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admitted_task_registers_then_parks_without_reissuing_admission() {
    let f = Fixture::with_admission(0, true);
    let (run, release) = admitted_before_registration(&f).await;
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    let mut parked = Box::pin(f.runtime.wait_quiesced());
    assert!(futures_util::poll!(parked.as_mut()).is_pending());
    release.send(()).unwrap();
    parked.await.unwrap();
    assert!(f.intent(run).checkpoint.is_some());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    f.model.entered.notified().await;
    assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 1);
    f.cleanup(run).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admitted_task_seals_then_stop_uses_terminal_dispatch_without_provider() {
    let f = Fixture::with_admission(0, true);
    let (run, release) = admitted_before_registration(&f).await;
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    release.send(()).unwrap();
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    f.runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(run).await.unwrap();
    assert!(f.runtime.shared.quiesce.running());
    assert!(f.runtime.cancel_restart_quiesce(&attempt).is_err());
    assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    assert!(!f.intent(run).active);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registered_admission_guard_blocks_seal_until_original_task_releases_it() {
    let f = Fixture::with_admission(0, true);
    let (run, release) = admitted_before_registration(&f).await;
    let execution = f.runtime.execution_snapshot(run).unwrap();
    let registered = Arc::new(tokio::sync::Notify::new());
    let signal = registered.clone();
    let (finish, held) = std::sync::mpsc::channel();
    f.runtime.on_execution(
        crate::runtime::HookPoint::AdmissionRegistered,
        &execution,
        move || {
            signal.notify_one();
            tokio::task::block_in_place(|| held.recv().unwrap());
        },
    );
    let attempt = f.runtime.begin_restart_quiesce().unwrap();
    release.send(()).unwrap();
    registered.notified().await;
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    let mut drain = Box::pin(f.runtime.drain_dispatch_tails(&fence));
    assert!(futures_util::poll!(drain.as_mut()).is_pending());
    assert!(!f.runtime.dispatch_drained());
    finish.send(()).unwrap();
    drain.await.unwrap();
    f.runtime.cancel_restart_quiesce(&attempt).unwrap();
    f.model.entered.notified().await;
    assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 1);
    f.cleanup(run).await;
}
