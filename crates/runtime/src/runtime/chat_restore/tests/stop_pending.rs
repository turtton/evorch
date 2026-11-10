use super::*;

async fn waiting_chat(runtime: &AgentRuntime) -> (RunId, event_bus::EventReceiver) {
    let mut events = runtime.shared.bus.subscribe();
    let run = runtime
        .delegate_chat(
            "stop-pending",
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
            event_bus::EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. }) if run_id == run.to_string())
        {
            break;
        }
    }
    runtime.bind_thread_root("stop-pending", run).unwrap();
    runtime
        .open_lane(
            run,
            "stop-lane",
            event_bus::LaneBrief {
                verification_requirements: Default::default(),
                objective: "independent deliverable".into(),
                inputs: vec![],
                deliverables: vec!["artifact".into()],
                acceptance_criteria: vec!["review".into()],
                constraints: vec![],
                workspace: Default::default(),
                report_policy: event_bus::LanePolicy::Attached,
            },
        )
        .unwrap();
    (run, events)
}

#[tokio::test]
async fn continue_after_stop_diagnostic_starts_a_new_incarnation() {
    for revoke in ["continue", "stop", "cancel", "late-cancel"] {
        let fixture = Fixture::new();
        let runtime = &fixture.runtime;
        let (run, mut events) = waiting_chat(runtime).await;
        let old = runtime.execution_snapshot(run).unwrap();
        if revoke == "late-cancel" {
            let cancelling = runtime.clone();
            runtime.on_execution(
                crate::runtime::cancellation::HookPoint::RootRestoreValidated,
                &old,
                move || cancelling.cancel(run).unwrap(),
            );
        }
        // No await between Stop, its synchronous ledger diagnostic and Continue:
        // this executor cannot finish the old run in the interval.
        runtime.stop(run, StopScope::SelfOnly).unwrap();
        assert!(
            events
                .drain_pending_snapshot()
                .iter()
                .any(|event| matches!(&event.kind,
            event_bus::EventKind::Diagnostic(d) if d.code == "lane_delivery_stopped"))
        );
        assert_eq!(
            runtime.inspect_agent(run).unwrap().phase,
            AgentRunPhase::Waiting
        );
        runtime.resume_chat(run, RunConfig::default()).unwrap();
        // Duplicate scheduling requests coalesce into one new admission.
        runtime.resume_chat(run, RunConfig::default()).unwrap();
        assert!(
            runtime
                .resume_intents()
                .unwrap()
                .iter()
                .find(|intent| intent.run_id == run)
                .unwrap()
                .resume_requested
        );
        match revoke {
            "stop" => runtime.stop(run, StopScope::SelfOnly).unwrap(),
            "cancel" => runtime.cancel(run).unwrap(),
            _ => {}
        }
        assert_eq!(
            runtime.wait(run).await.unwrap(),
            if revoke == "cancel" {
                AgentRunPhase::Error
            } else {
                AgentRunPhase::Stopped
            }
        );
        let new = runtime.execution_snapshot(run).unwrap();
        assert_eq!(
            !Arc::ptr_eq(&old, &new),
            revoke == "continue",
            "Continue was discarded or a revoked request restarted: {revoke}"
        );
        assert!(
            !runtime
                .resume_intents()
                .unwrap()
                .iter()
                .find(|intent| intent.run_id == run)
                .unwrap()
                .resume_requested
        );
        if revoke == "late-cancel" {
            let pending = events.drain_pending_snapshot();
            assert!(pending.iter().any(|event| matches!(&event.kind,
                event_bus::EventKind::Diagnostic(d) if d.code == "chat_resume_failed")));
            assert!(!pending.iter().any(|event| matches!(&event.kind,
                event_bus::EventKind::Diagnostic(d) if d.code == "lane_delivery_resumed")));
        }
        if revoke == "continue" {
            loop {
                if matches!(events.recv().await.unwrap().kind,
                    event_bus::EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. }) if run_id == run.to_string())
                {
                    break;
                }
            }
        }
        let descendants = runtime.live_descendants(run);
        runtime.stop(run, StopScope::Subtree).unwrap();
        runtime.wait(run).await.unwrap();
        for child in descendants {
            runtime.wait(child).await.unwrap();
        }
    }
}

#[tokio::test]
async fn deferred_continue_save_failure_is_not_accepted() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let (run, _) = waiting_chat(runtime).await;
    let old = runtime.execution_snapshot(run).unwrap();
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    rusqlite::Connection::open(fixture._dir.path().join("goal.sqlite3")).unwrap().execute_batch(
        "CREATE TRIGGER reject_deferred_resume BEFORE UPDATE ON resume_intents WHEN json_extract(NEW.payload, '$.resume_requested')=1 BEGIN SELECT RAISE(ABORT, 'deferred resume rejected'); END;"
    ).unwrap();
    assert!(runtime.resume_chat(run, RunConfig::default()).is_err());
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Stopped);
    assert!(Arc::ptr_eq(&old, &runtime.execution_snapshot(run).unwrap()));
    let descendants = runtime.live_descendants(run);
    runtime.stop(run, StopScope::Subtree).unwrap();
    for child in descendants {
        runtime.wait(child).await.unwrap();
    }
}

#[tokio::test]
async fn continue_during_hard_cancel_does_not_resume_lane_delivery() {
    let fixture = Fixture::new();
    let runtime = &fixture.runtime;
    let (run, mut events) = waiting_chat(runtime).await;
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    assert!(runtime.graceful_stop_requested(run));
    runtime.cancel(run).unwrap();
    assert!(!runtime.graceful_stop_requested(run));
    assert!(matches!(
        runtime.resume_chat(run, RunConfig::default()),
        Err(RuntimeError::RunTerminated { .. })
    ));
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    assert!(!events.drain_pending_snapshot().iter().any(|event| matches!(&event.kind, event_bus::EventKind::Diagnostic(d) if d.code == "lane_delivery_resumed")));
    let descendants = runtime.live_descendants(run);
    runtime.stop(run, StopScope::Subtree).unwrap();
    for child in descendants {
        runtime.wait(child).await.unwrap();
    }
}
