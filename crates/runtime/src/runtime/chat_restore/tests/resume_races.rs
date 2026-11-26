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
    for admission in [false, true] {
        resume_race_admission_save_failure_does_not_resume_delivery_with_admission(admission).await;
    }
}

async fn resume_race_admission_save_failure_does_not_resume_delivery_with_admission(
    admission: bool,
) {
    let fixture = Fixture::for_s2(admission);
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

#[tokio::test]
async fn resume_race_m4_later_subtree_cancel_fences_already_cancelled_observation() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let (run, mut events) = waiting_chat(runtime).await;
    runtime.cancel(run).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    let old = runtime.execution_snapshot(run).unwrap();
    let original = runtime
        .resume_intents()
        .unwrap()
        .into_iter()
        .find(|i| i.run_id == run)
        .unwrap()
        .incarnation;
    let restore_release = Arc::new(Barrier::new(2));
    let cancel_release = Arc::new(Barrier::new(2));
    let (validated, validation) = tokio::sync::oneshot::channel();
    let (collected, collection) = tokio::sync::oneshot::channel();
    let release = restore_release.clone();
    runtime.on_execution(HookPoint::RootRestoreValidated, &old, move || {
        validated.send(()).unwrap();
        release.wait();
    });
    let release = cancel_release.clone();
    runtime.on_execution(HookPoint::SubtreeCollected, &old, move || {
        collected.send(()).unwrap();
        release.wait();
    });
    let resuming = runtime.clone();
    let dispatch =
        tokio::task::spawn_blocking(move || resuming.resume_chat(run, RunConfig::default()));
    validation.await.unwrap();
    let cancelling = runtime.clone();
    let cancel = tokio::task::spawn_blocking(move || cancelling.cancel_subtree(run));
    collection.await.unwrap();
    // I has recorded the later cancellation, but notification still targets the
    // old execution. Its watch value was already Cancel before this request.
    restore_release.wait();
    let result = dispatch.await.unwrap();
    let unchanged = Arc::ptr_eq(&old, &runtime.execution_snapshot(run).unwrap())
        && runtime
            .resume_intents()
            .unwrap()
            .into_iter()
            .find(|i| i.run_id == run)
            .unwrap()
            .incarnation
            == original;
    let failed = events.drain_pending_snapshot().iter().any(|e| matches!(&e.kind, event_bus::EventKind::Diagnostic(d) if d.code == "chat_resume_failed"));
    cancel_release.wait();
    cancel.await.unwrap().unwrap();
    cleanup(runtime, run).await;
    assert_eq!(
        (result.is_err(), unchanged, failed),
        (true, true, true),
        "a later inherited cancellation must reject the old observation: {result:?}"
    );
}

#[tokio::test]
async fn resume_race_cancel_after_registration_prevents_lane_resume_commit() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let (run, mut events) = waiting_chat(runtime).await;
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();
    let old = runtime.execution_snapshot(run).unwrap();
    let release = Arc::new(Barrier::new(2));
    let (arrived, registered) = tokio::sync::oneshot::channel();
    let gate = release.clone();
    runtime.on_execution(HookPoint::ResumeRegistered, &old, move || {
        arrived.send(()).unwrap();
        gate.wait();
    });
    let resuming = runtime.clone();
    let dispatch =
        tokio::task::spawn_blocking(move || resuming.resume_chat(run, RunConfig::default()));
    registered.await.unwrap();
    let new = runtime.execution_snapshot(run).unwrap();
    assert!(!Arc::ptr_eq(&old, &new));
    runtime.cancel(run).unwrap();
    release.wait();
    let result = dispatch.await.unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    let held = stopped(runtime);
    let pending = events.drain_pending_snapshot();
    let failed = pending.iter().any(|e| matches!(&e.kind, event_bus::EventKind::Diagnostic(d) if d.code == "chat_resume_failed"));
    let resumed = pending.iter().any(|e| matches!(&e.kind, event_bus::EventKind::Diagnostic(d) if d.code == "lane_delivery_resumed"));
    cleanup(runtime, run).await;
    assert_eq!(
        (result.is_err(), new.cancelled(), held, failed, resumed),
        (true, true, true, true, false),
        "Cancel won before lane commit: {result:?}"
    );
}

// A real Continue holds stop_restore_gate before taking the inbox serializer.
// Pause retention inside that serializer, then let Continue reach its serializer
// acquisition. Retention must yield instead of forming a lock-order cycle.
#[tokio::test]
async fn retention_idle_stop_yields_to_ordinary_continue_holding_restore_gate() {
    struct PauseStop {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Arc<Barrier>,
    }
    impl crate::quiesce::StopBoundary for PauseStop {
        fn stop(&self, delivery: crate::quiesce::StopDelivery<'_>) -> Result<(), RuntimeError> {
            let entered = self.entered.lock().unwrap().take();
            if let Some(entered) = entered {
                entered.send(()).unwrap();
                self.release.wait();
            }
            delivery.deliver()
        }
    }
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let mut events = runtime.shared.bus.subscribe();
    let mut to_persist = runtime.shared.bus.subscribe();
    let run = runtime
        .delegate_chat(
            "retention-race",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    loop {
        if matches!(events.recv().await.unwrap().kind,
            event_bus::EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id, to: AgentRunPhase::Waiting, ..
            }) if run_id == run.to_string())
        {
            break;
        }
    }
    runtime.bind_thread_root("retention-race", run).unwrap();
    // This chat fixture has no workbench event pump. Persist and ACK its initial
    // bus output before taking the retention snapshot, just as the host does.
    for event in to_persist.drain_pending_snapshot() {
        if !matches!(event.kind, event_bus::EventKind::Usage(_)) {
            fixture.storage.handle().append_event(None, &event).unwrap();
            runtime
                .shared
                .bus
                .persistence_notifications()
                .notify(&event, event_bus::PersistenceStage::Committed);
        }
    }
    fixture.storage.handle().barrier().unwrap();
    let old = runtime.execution_snapshot(run).unwrap();
    let inbox = runtime.entry(run).unwrap().user_inbox.clone();
    let pause = inbox.pause_next_send(crate::runtime::user_inbox::SendPausePoint::BeforeLock);
    let release = Arc::new(Barrier::new(2));
    let (entered, prepared) = tokio::sync::oneshot::channel();
    runtime
        .install_stop_boundary(Arc::new(PauseStop {
            entered: Mutex::new(Some(entered)),
            release: release.clone(),
        }))
        .unwrap();
    let scope = format!("r:{run}");
    let before = vec![(scope.clone(), runtime.shared.retention_gate.stamp(&scope))];
    let retaining = runtime.clone();
    assert_eq!(before[0].1.pending, 0);
    let mut stop = tokio::task::spawn_blocking(move || retaining.stop_retention_idle(run, &before));
    tokio::select! {
        result = prepared => result.unwrap(),
        result = &mut stop => panic!("retention did not reach its boundary: {result:?}"),
    }
    let continuing = runtime.clone();
    let send = tokio::task::spawn_blocking(move || {
        continuing.continue_goal(run, "ordinary followup".into(), RunConfig::default())
    });
    let pause = tokio::task::spawn_blocking(move || {
        pause.reached.recv().unwrap();
        pause
    })
    .await
    .unwrap();
    // Continue now owns stop_restore_gate. Both paths may proceed; under the
    // old blocking Stop they would wait for each other's lock indefinitely.
    pause.resume.send(()).unwrap();
    release.wait();
    assert!(matches!(
        stop.await.unwrap(),
        Err(RuntimeError::RestartBusy)
    ));
    assert_eq!(send.await.unwrap().unwrap(), run);
    loop {
        if matches!(events.recv().await.unwrap().kind,
            event_bus::EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id, to: AgentRunPhase::Waiting, ..
            }) if run_id == run.to_string())
        {
            break;
        }
    }
    assert!(Arc::ptr_eq(&old, &runtime.execution_snapshot(run).unwrap()));
    assert!(!runtime.operator_stopped(run));
    assert_eq!(inbox.status().pending, 0);
    cleanup(runtime, run).await;
}
