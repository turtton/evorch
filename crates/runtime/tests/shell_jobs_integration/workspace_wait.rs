use super::*;
use event_bus::{EventKind, EventReceiver, LifecycleEvent, ToolEvent, WorkspaceWait};

async fn wait_change(events: &mut EventReceiver, run: RunId, call: &str) -> Option<WorkspaceWait> {
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::WorkspaceWaitChanged {
            run_id,
            call_id,
            waiting,
        }) = events.recv().await.unwrap().kind
            && run_id == run.to_string()
            && call_id == call
        {
            return waiting;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yielded_shell_retains_observed_owner_and_runtime_cancellation_clears_wait() {
    let (temp, root) = init_git_repo();
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let executor = executor(&bus, &root);
    let snapshots = snapshots(&root, temp.path());
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, executor.clone(), model).with_snapshots(snapshots.clone());
    let owner = runtime.delegate_background(Role::Worker, "owner".into(), RunConfig::default());
    calls.recv().await.unwrap().respond(tool_response(
        "start",
        "shell",
        json!({"command":"read value", "yield_ms":0}),
    ));
    let owner_call = calls.recv().await.unwrap();
    assert!(!owner_call.result("start").1);
    assert!(executor.has_running_shell_jobs(&owner.to_string()));
    // Model continuation establishes that ToolCompleted has been emitted, while
    // the actual child is still blocked on stdin and owns the retained guard.
    assert!(events.drain_pending_snapshot().iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolCompleted {
            run_id: Some(run_id), call_id, is_error: false, ..
        }) if *run_id == owner.to_string() && call_id == "start"
    )));

    let cancelled = runtime.delegate_background(
        Role::Worker,
        "cancelled waiter".into(),
        RunConfig::default(),
    );
    calls.recv().await.unwrap().respond(tool_response(
        "cancelled-shell",
        "shell",
        json!({"command":"printf should-not-run", "yield_ms":0}),
    ));
    let waiting = wait_change(&mut events, cancelled, "cancelled-shell")
        .await
        .unwrap();
    assert_eq!(waiting.workspace_root, root);
    assert_eq!(waiting.command.as_deref(), Some("printf should-not-run"));
    let holder = waiting.holder.unwrap();
    assert_eq!(holder.run_id, owner.to_string());
    assert_eq!(holder.call_id, "start");
    assert_eq!(holder.tool_name, "shell");
    assert_eq!(holder.command.as_deref(), Some("read value"));

    runtime.cancel(cancelled).unwrap();
    assert_eq!(runtime.wait(cancelled).await.unwrap(), AgentRunPhase::Error);
    assert!(
        wait_change(&mut events, cancelled, "cancelled-shell")
            .await
            .is_none()
    );
    assert!(executor.has_running_shell_jobs(&owner.to_string()));

    let successor =
        runtime.delegate_background(Role::Worker, "successor".into(), RunConfig::default());
    calls.recv().await.unwrap().respond(tool_response(
        "write-after",
        "write",
        json!({"path":"after.txt", "content":"unblocked"}),
    ));
    let waiting = wait_change(&mut events, successor, "write-after")
        .await
        .unwrap();
    assert_eq!(waiting.holder.unwrap().run_id, owner.to_string());
    assert!(waiting.command.is_none());

    runtime.cancel(owner).unwrap();
    assert_eq!(runtime.wait(owner).await.unwrap(), AgentRunPhase::Error);
    // A holder-clear update may precede acquisition. Only None ends this call's wait.
    while wait_change(&mut events, successor, "write-after")
        .await
        .is_some()
    {}
    let call = calls.recv().await.unwrap();
    assert!(!call.result("write-after").1);
    assert_eq!(
        std::fs::read_to_string(root.join("after.txt")).unwrap(),
        "unblocked"
    );
    call.respond(text_response("done", FinishReason::Stop));
    assert_eq!(runtime.wait(successor).await.unwrap(), AgentRunPhase::Done);
    drop(snapshots.lock(None).await.unwrap());
    drop(owner_call);
}
