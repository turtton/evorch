use std::collections::BTreeSet;
use std::sync::Arc;

use event_bus::{
    AgentMessageEvent, AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent, ProviderEvent,
};
use gui::model::demo::DemoScriptModel;
use runtime::{AgentRuntime, Role, RunConfig, RunId};
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

#[tokio::test]
async fn demo_script_drives_three_done_runs_with_messages_and_telemetry() {
    // Given: the production demo model behind the real runtime and meta-tool executor.
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::new(Arc::clone(&bus)));
    let model = Arc::new(DemoScriptModel::new(Arc::clone(&bus)));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);
    let mut receiver = bus.subscribe();

    // When: the orchestrator executes the complete deterministic demo script.
    let orchestrator = runtime.delegate_background(
        Role::Orchestrator,
        "DEMO-ORCH".to_string(),
        RunConfig::default(),
    );
    assert_eq!(
        runtime.wait(orchestrator).await,
        Ok(AgentRunPhase::Done),
        "orchestrator inspection: {:?}",
        runtime.inspect_agent(orchestrator)
    );
    for run_id in [RunId::new(2), RunId::new(3)] {
        assert_eq!(
            timeout(Duration::from_secs(2), runtime.wait(run_id)).await,
            Ok(Ok(AgentRunPhase::Done))
        );
    }
    let events = collect_until_completed(&mut receiver).await;

    // Then: all runs are Done, all four directed messages were delivered, and every
    // run emitted correlated provider completion telemetry.
    assert!(
        runtime
            .list_agents()
            .iter()
            .all(|run| run.phase == AgentRunPhase::Done)
    );
    assert_eq!(runtime.list_agents().len(), 3);
    let deliveries = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) => Some((
                message.sender_run_id.as_str(),
                message.recipient_run_id.as_str(),
            )),
            EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::Compaction(_) => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        deliveries,
        BTreeSet::from([
            ("run-1", "run-2"),
            ("run-1", "run-3"),
            ("run-2", "run-1"),
            ("run-3", "run-1"),
        ])
    );
    let completed_runs = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id: Some(run_id),
                ..
            }) => Some(run_id.as_str()),
            EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_) => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(completed_runs, BTreeSet::from(["run-1", "run-2", "run-3"]));
}

async fn collect_until_completed(receiver: &mut event_bus::EventReceiver) -> Vec<Event> {
    let mut completed = BTreeSet::new();
    let mut events = Vec::new();
    while completed.len() < 3 {
        let event = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("demo event timeout")
            .expect("demo event bus remains open");
        if let EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { task_id }) =
            &event.kind
        {
            completed.insert(task_id.clone());
        }
        events.push(event);
    }
    events
}
