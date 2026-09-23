use super::*;
use event_bus::{EventKind, LifecycleEvent};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_notification_waits_for_drain_and_precedes_terminal_error() {
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let (started, mut draining) = mpsc::unbounded_channel();
    let shell = Arc::new(GatedShell {
        shell: tools::tools::Shell::new(Arc::new(sandbox::DirectSandbox::new_unchecked())),
        drains: AtomicUsize::new(0),
        started,
        gate: tokio::sync::Semaphore::new(0),
    });
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(shell.clone()).unwrap();
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model);
    let run = runtime.delegate_background(
        Role::Worker,
        "cancel during model invocation".into(),
        RunConfig::default(),
    );
    let call = next(&mut calls).await;
    runtime.cancel(run).unwrap();
    timeout(DEADLINE, draining.recv()).await.unwrap().unwrap();
    assert_eq!(
        runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Running
    );
    assert!(runtime.wait(run).now_or_never().is_none());
    for event in support::drain_events(&mut events).await {
        assert!(!matches!(
            event.kind,
            EventKind::Lifecycle(
                LifecycleEvent::BackgroundTaskCancelled { .. }
                    | LifecycleEvent::AgentRunStateChanged {
                        to: AgentRunPhase::Error | AgentRunPhase::Done,
                        ..
                    }
            )
        ));
    }
    shell.gate.add_permits(1);
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Error);
    let events = support::drain_events(&mut events).await;
    let cancelled: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(&event.kind,
                EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCancelled { task_id })
                if task_id == &run.to_string()
            )
            .then_some(index)
        })
        .collect();
    assert_eq!(cancelled.len(), 1, "cancel notification is emitted once");
    let terminal = events
        .iter()
        .position(|event| {
            matches!(&event.kind,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id, to: AgentRunPhase::Error, reason: Some(reason), ..
                }) if run_id == &run.to_string() && reason == "cancelled"
            )
        })
        .expect("terminal error is published");
    assert!(
        cancelled[0] < terminal,
        "terminal observers must see cancellation first"
    );
    drop(call);
}
