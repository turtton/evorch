use super::*;

struct PendingModel;

#[async_trait::async_trait]
impl AgentModel for PendingModel {
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pending".into()
    }
}

#[tokio::test]
async fn terminal_replay_relays_once_without_final_output() {
    // Given: registered runs that have not produced any output.
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, Arc::new(PendingModel));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "private parent".into(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "private child", RunConfig::default())
        .unwrap();
    let terminal = LifecycleEvent::AgentRunStateChanged {
        run_id: child.to_string(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Done,
        reason: None,
    };
    // When: the terminal publication is replayed.
    runtime.publish_terminal(child, terminal.clone());
    runtime.publish_terminal(child, terminal);
    // Then: one metadata-only message survives both publications.
    let messages = runtime.take_inbox(parent).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "completed without final output");
    assert_eq!(messages[0].sender_run_id, child.to_string());
    assert_eq!(messages[0].recipient_run_id, parent.to_string());
    assert_eq!(messages[0].kind, AgentMessageKind::Send);
    assert_eq!(messages[0].reply_to, None);
}

fn fixture() -> AgentRuntime {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(bus, executor, Arc::new(PendingModel))
}

fn publish(runtime: &AgentRuntime, run_id: RunId, to: AgentRunPhase) {
    runtime.publish_terminal(
        run_id,
        LifecycleEvent::AgentRunStateChanged {
            run_id: run_id.to_string(),
            from: AgentRunPhase::Running,
            to,
            reason: Some("operator stopped".into()),
        },
    );
}

#[tokio::test]
async fn stopped_child_publishes_terminal_output_and_relays_once_without_legacy_events() {
    let runtime = fixture();
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    let mut events = runtime.shared.bus.subscribe();

    publish(&runtime, child, AgentRunPhase::Stopped);
    publish(&runtime, child, AgentRunPhase::Stopped);
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Stopped);
    let output = serde_json::to_value(runtime.run_output(parent, child).unwrap()).unwrap();
    assert_eq!(output["phase"], "Stopped");
    assert_eq!(output["status"], "stopped");
    assert!(output["output"].is_null());
    assert_eq!(output["reason"], "operator stopped");
    assert!(matches!(
        runtime.send_message(child, "not a restore".into()),
        Err(RuntimeError::RunTerminated { .. })
    ));
    let messages = runtime.take_inbox(parent).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "stopped");

    // A marker bounds the synchronous publication without polling or yielding to runs.
    runtime.shared.bus.emit(Event::new(LifecycleEvent::Started {
        session_id: "marker".into(),
    }));
    let mut terminals = 0;
    let mut deliveries = 0;
    loop {
        match events.recv().await.unwrap().kind {
            EventKind::Lifecycle(LifecycleEvent::Started { session_id })
                if session_id == "marker" =>
            {
                break;
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { to, .. }) => {
                assert_eq!(to, AgentRunPhase::Stopped);
                terminals += 1;
            }
            EventKind::AgentMessage(AgentMessageEvent::Delivered { .. }) => deliveries += 1,
            EventKind::Lifecycle(
                LifecycleEvent::BackgroundTaskCancelled { .. }
                | LifecycleEvent::BackgroundTaskCompleted { .. },
            ) => panic!("stopping must not emit a legacy terminal event"),
            _ => {}
        }
    }
    assert_eq!(terminals, 2);
    assert_eq!(deliveries, 1);
}

#[tokio::test]
async fn terminal_parents_drop_child_completion_without_mailbox_wake() {
    for phase in [
        AgentRunPhase::Stopped,
        AgentRunPhase::Done,
        AgentRunPhase::Error,
    ] {
        let runtime = fixture();
        let parent =
            runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
        let child = runtime
            .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
            .unwrap();
        publish(&runtime, parent, phase);
        let version = runtime.run_mailbox(parent).unwrap().subscribe_version();
        publish(&runtime, child, AgentRunPhase::Stopped);
        assert!(!version.has_changed().unwrap());
        assert!(runtime.take_inbox(parent).unwrap().is_empty());
        assert_eq!(runtime.inspect_agent(parent).unwrap().phase, phase);
    }
}

#[tokio::test]
async fn stop_scopes_follow_intents_and_registered_parents_without_hard_cancel() {
    let runtime = fixture();
    let root = runtime.delegate_background(Role::Orchestrator, "root".into(), RunConfig::default());
    let child = runtime
        .delegate_background_as_child(root, Role::Worker, "child", RunConfig::default())
        .unwrap();
    let grandchild = runtime
        .delegate_background_as_child(child, Role::Worker, "grandchild", RunConfig::default())
        .unwrap();
    let unrelated =
        runtime.delegate_background(Role::Worker, "unrelated".into(), RunConfig::default());
    // Traverse the root -> child edge only in runs and child -> grandchild only in intents.
    runtime.shared.spawn_intents.lock().unwrap().remove(&child);
    lock_runs(&runtime.shared.runs)
        .get_mut(&grandchild)
        .unwrap()
        .parent = None;
    publish(&runtime, child, AgentRunPhase::Done);
    assert_eq!(runtime.live_descendants(root), vec![grandchild]);
    assert_eq!(
        *runtime.entry(root).unwrap().cancel_tx.borrow(),
        RunInterrupt::None
    );

    runtime.stop(root, StopScope::SelfOnly).unwrap();
    assert_eq!(
        *runtime.entry(root).unwrap().cancel_tx.borrow(),
        RunInterrupt::Stop
    );
    assert_eq!(
        *runtime.entry(grandchild).unwrap().cancel_tx.borrow(),
        RunInterrupt::None
    );
    runtime.stop(root, StopScope::Subtree).unwrap();
    for id in [root, child, grandchild] {
        assert_eq!(
            *runtime.entry(id).unwrap().cancel_tx.borrow(),
            RunInterrupt::Stop
        );
    }
    assert_eq!(
        *runtime.entry(unrelated).unwrap().cancel_tx.borrow(),
        RunInterrupt::None
    );
    assert!(!runtime.spawn_cancelled(root));
    runtime.cancel(root).unwrap();
    assert_eq!(
        *runtime.entry(root).unwrap().cancel_tx.borrow(),
        RunInterrupt::Cancel
    );
    assert!(matches!(
        runtime.stop(RunId::new(u64::MAX), StopScope::Subtree),
        Err(RuntimeError::UnknownRun { .. })
    ));
}

#[tokio::test]
async fn stopping_registered_pending_run_is_terminal_and_distinct_from_cancel() {
    let runtime = fixture();
    for (interrupt, expected) in [
        (RunInterrupt::Stop, AgentRunPhase::Stopped),
        (RunInterrupt::Cancel, AgentRunPhase::Error),
    ] {
        let run = runtime.delegate_background(Role::Worker, "prompt".into(), RunConfig::default());
        assert_eq!(
            runtime.inspect_agent(run).unwrap().phase,
            AgentRunPhase::Pending
        );
        match interrupt {
            RunInterrupt::Stop => runtime.stop(run, StopScope::SelfOnly).unwrap(),
            RunInterrupt::Cancel => runtime.cancel(run).unwrap(),
            RunInterrupt::None => unreachable!(),
        }
        let phase = tokio::time::timeout(Duration::from_secs(2), runtime.wait(run))
            .await
            .expect("interrupted run must terminate")
            .unwrap();
        assert_eq!(phase, expected);
        assert_eq!(runtime.run_result(run).unwrap(), None);
    }
}

#[tokio::test]
async fn stop_cancels_pending_admission_for_self_and_subtree() {
    for scope in [StopScope::SelfOnly, StopScope::Subtree] {
        let runtime = fixture();
        let root =
            runtime.delegate_background(Role::Orchestrator, "root".into(), RunConfig::default());
        let pending = runtime.reserve_child_run_id(root).unwrap();
        runtime.track_goal_run(pending, &root.to_string());
        runtime.admit_run(
            pending,
            Some(root),
            Role::Worker,
            "pending".into(),
            RunConfig::default(),
            RunContinuation::Fresh,
        );
        assert_eq!(runtime.live_descendants(root), vec![pending]);
        let target = if scope == StopScope::SelfOnly {
            pending
        } else {
            root
        };
        runtime.stop(target, scope).unwrap();
        assert!(
            runtime
                .shared
                .admissions
                .lock()
                .unwrap()
                .get(&pending)
                .unwrap()
                .cancelled
        );
        assert!(matches!(
            runtime.wait_admission(pending).await,
            Err(RuntimeError::RunTerminated { .. })
        ));
        assert!(runtime.live_descendants(root).is_empty());
        assert!(runtime.entry(pending).is_err());
    }
}

#[tokio::test]
async fn publish_terminal_emits_event_when_run_missing_from_registry() {
    let runtime = fixture();
    let mut events = runtime.shared.bus.subscribe();
    let missing = RunId::new(u64::MAX);
    for phase in [
        AgentRunPhase::Done,
        AgentRunPhase::Error,
        AgentRunPhase::Stopped,
    ] {
        publish(&runtime, missing, phase);
        assert!(matches!(events.recv().await.unwrap().kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { to, .. }) if to == phase));
        // No legacy completion/cancel or parent mailbox delivery event.
        runtime.shared.bus.emit(Event::new(LifecycleEvent::Started {
            session_id: "marker".into(),
        }));
        assert!(matches!(events.recv().await.unwrap().kind,
            EventKind::Lifecycle(LifecycleEvent::Started { session_id }) if session_id == "marker"));
    }
    for phase in [
        AgentRunPhase::Pending,
        AgentRunPhase::Running,
        AgentRunPhase::Waiting,
    ] {
        publish(&runtime, missing, phase);
        runtime.shared.bus.emit(Event::new(LifecycleEvent::Started {
            session_id: "marker".into(),
        }));
        assert!(matches!(events.recv().await.unwrap().kind,
            EventKind::Lifecycle(LifecycleEvent::Started { session_id }) if session_id == "marker"));
    }
}
