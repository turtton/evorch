use super::stop_pending::waiting_chat;
use super::*;
use crate::runtime::cancellation::HookPoint;
use std::sync::{Barrier, Mutex};

async fn cleanup(runtime: &AgentRuntime, run: RunId) {
    let children = runtime.live_descendants(run);
    runtime.stop(run, StopScope::Subtree).unwrap();
    runtime.wait(run).await.unwrap();
    for child in children {
        runtime.wait(child).await.unwrap();
    }
}
fn stopped(runtime: &AgentRuntime) -> bool {
    runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .handle
        .lane_delivery_destination("stop-pending".into())
        .unwrap()
        .unwrap()
        .stopped
}

#[tokio::test]
async fn resume_race_admission_save_failure_does_not_resume_delivery() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let (run, mut events) = waiting_chat(runtime).await;
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();
    let old = runtime.execution_snapshot(run).unwrap();
    let release = Arc::new(Barrier::new(2));
    let (arrived, waiting) = tokio::sync::oneshot::channel();
    let gate = release.clone();
    runtime.on_execution(HookPoint::RootRestoreValidated, &old, move || {
        arrived.send(()).unwrap();
        gate.wait();
    });
    let resuming = runtime.clone();
    let dispatch =
        tokio::task::spawn_blocking(move || resuming.resume_chat(run, RunConfig::default()));
    waiting.await.unwrap();
    let sql = rusqlite::Connection::open(fixture._dir.path().join("goal.sqlite3")).unwrap();
    sql.execute_batch("CREATE TRIGGER reject_new_resume_incarnation BEFORE UPDATE ON resume_intents WHEN NEW.incarnation != OLD.incarnation BEGIN SELECT RAISE(ABORT, 'new resume incarnation rejected'); END;").unwrap();
    release.wait();
    let result = dispatch.await.unwrap();
    runtime.wait(run).await.unwrap();
    let held = stopped(runtime);
    let pending = events.drain_pending_snapshot();
    let failed = pending.iter().any(|event| matches!(&event.kind, event_bus::EventKind::Diagnostic(d) if d.code == "chat_resume_failed"));
    let resumed = pending.iter().any(|event| matches!(&event.kind, event_bus::EventKind::Diagnostic(d) if d.code == "lane_delivery_resumed"));
    sql.execute_batch("DROP TRIGGER reject_new_resume_incarnation;")
        .unwrap();
    cleanup(runtime, run).await;
    assert_eq!(
        (result.is_err(), held, failed, resumed),
        (true, true, true, false),
        "a diagnostic incarnation is not admitted: {result:?}"
    );
}

#[tokio::test]
async fn resume_race_later_stop_keeps_delivery_stopped_after_registration() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let (run, _) = waiting_chat(runtime).await;
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();
    let old = runtime.execution_snapshot(run).unwrap();
    let release = Arc::new(Barrier::new(2));
    let (arrived, waiting) = tokio::sync::oneshot::channel();
    let (at_gate, disposition) = tokio::sync::oneshot::channel();
    let at_gate = Arc::new(Mutex::new(Some(at_gate)));
    let gate = release.clone();
    let hooked = runtime.clone();
    runtime.on_execution(HookPoint::ResumeRegistered, &old, move || {
        let new = hooked.execution_snapshot(run).unwrap();
        for (point, blocked) in [
            (HookPoint::StopGateAcquired, false),
            (HookPoint::StopGateBlocked, true),
        ] {
            let at_gate = at_gate.clone();
            hooked.on_execution(point, &new, move || {
                if let Some(sender) = at_gate.lock().unwrap().take() {
                    sender.send(blocked).unwrap();
                }
            });
        }
        arrived.send(()).unwrap();
        gate.wait();
    });
    let resuming = runtime.clone();
    let dispatch =
        tokio::task::spawn_blocking(move || resuming.resume_chat(run, RunConfig::default()));
    waiting.await.unwrap();
    let stopping = runtime.clone();
    let stop = tokio::task::spawn_blocking(move || stopping.stop(run, StopScope::SelfOnly));
    // The gate itself acknowledges either acquisition or contention. No time
    // observation decides whether Stop reached the registration/commit boundary.
    if disposition.await.unwrap() {
        release.wait();
        stop.await.unwrap().unwrap();
    } else {
        stop.await.unwrap().unwrap();
        release.wait();
    }
    let result = dispatch.await.unwrap();
    runtime.wait(run).await.unwrap();
    let held = stopped(runtime);
    cleanup(runtime, run).await;
    assert!(result.is_ok(), "resume admission itself failed: {result:?}");
    assert!(held, "an older Continue overwrote the later Stop ledger");
}

#[tokio::test]
async fn resume_race_fresh_continue_after_pending_continue_cancel_and_terminal_is_accepted() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let (run, mut events) = waiting_chat(runtime).await;
    let old = runtime.execution_snapshot(run).unwrap();
    // No await before Cancel: the first Continue is accepted while Stop is pending.
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    runtime.resume_chat(run, RunConfig::default()).unwrap();
    runtime.cancel(run).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    assert!(Arc::ptr_eq(&old, &runtime.execution_snapshot(run).unwrap()));
    runtime.resume_chat(run, RunConfig::default()).unwrap();
    assert!(!Arc::ptr_eq(
        &old,
        &runtime.execution_snapshot(run).unwrap()
    ));
    loop {
        if matches!(events.recv().await.unwrap().kind, event_bus::EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {run_id, to: AgentRunPhase::Waiting, ..}) if run_id == run.to_string())
        {
            break;
        }
    }
    cleanup(runtime, run).await;
}
