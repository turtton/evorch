mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{
    AgentMessageEvent, AgentMessageKind, AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent,
};
use providers::FinishReason;
use runtime::{AgentRuntime, CoordinationTopology, RunConfig, RunId};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

async fn fixture() -> (AgentRuntime, Arc<EventBus>, Arc<ScriptedModel>) {
    let model = Arc::new(ScriptedModel::new([]));
    model.gate_key("PARENT", Arc::new(Notify::new())).await;
    model.gate_key("PENDING", Arc::new(Notify::new())).await;
    model
        .add_keyed(
            "CHILD",
            [Ok(text_response("fixed child result", FinishReason::Stop))],
        )
        .await;
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (
        AgentRuntime::new(Arc::clone(&bus), executor, model.clone()),
        bus,
        model,
    )
}

async fn running(receiver: &mut event_bus::EventReceiver, run: RunId) {
    loop {
        let event = receiver.recv().await.expect("bus open");
        if matches!(event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id, to: AgentRunPhase::Running, ..
        }) if run_id == run.to_string())
        {
            return;
        }
    }
}

fn notice(runtime: &AgentRuntime, parent: RunId, child: RunId) -> String {
    let messages = runtime.take_inbox(parent).expect("parent exists");
    assert_eq!(messages.len(), 1);
    let message = &messages[0];
    assert_eq!(message.sender_run_id, child.to_string());
    assert_eq!(message.recipient_run_id, parent.to_string());
    assert_eq!(message.kind, AgentMessageKind::Send);
    assert_eq!(message.reply_to, None);
    assert!(!message.message_id.is_empty());
    assert!(runtime.take_inbox(parent).unwrap().is_empty());
    message.content.clone()
}

#[tokio::test]
async fn background_child_completion_reaches_parent_inbox_once() {
    // Given: an in-flight parent and an immediately completing child.
    let (runtime, bus, _model) = fixture().await;
    let mut receiver = bus.subscribe();
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    running(&mut receiver, parent).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    // When: the child finishes.
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));
    // Then: exactly one correctly addressed final result can be drained.
    assert_eq!(notice(&runtime, parent, child), "fixed child result");
    runtime.cancel(parent).unwrap();
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Error));
}

#[tokio::test]
async fn cancelled_child_relays_metadata_only_terminal_notice() {
    // Given: both runs are blocked in their model requests.
    let (runtime, bus, _model) = fixture().await;
    let mut receiver = bus.subscribe();
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    running(&mut receiver, parent).await;
    let prompt = "PENDING confidential task input";
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, prompt, RunConfig::default())
        .unwrap();
    running(&mut receiver, child).await;
    // When: cancellation terminates the child.
    runtime.cancel(child).unwrap();
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Error));
    // Then: only cancellation metadata is delivered, never the task input.
    let content = notice(&runtime, parent, child);
    assert!(content.contains("cancelled"));
    assert!(!content.contains(prompt));
    runtime.cancel(parent).unwrap();
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Error));
}

#[tokio::test]
async fn relay_skipped_when_parent_terminal_and_never_restored() {
    // Given: a completed parent remains registered.
    let (runtime, bus, model) = fixture().await;
    model
        .add_keyed(
            "DONE",
            [Ok(text_response("parent result", FinishReason::Stop))],
        )
        .await;
    let mut receiver = bus.subscribe();
    let parent =
        runtime.delegate_background(Role::Orchestrator, "DONE".into(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    // When: its child terminates after the parent.
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));
    bus.emit(Event::new(LifecycleEvent::BackgroundTaskCompleted {
        task_id: "probe-end".into(),
    }));
    // Then: a bounded event prefix contains neither delivery nor restoration.
    loop {
        let event = receiver.recv().await.unwrap();
        assert!(
            !matches!(&event.kind, EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) if message.recipient_run_id == parent.to_string())
        );
        assert!(
            !matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunRestored { run_id, .. }) if run_id == &parent.to_string())
        );
        if matches!(event.kind, EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { task_id }) if task_id == "probe-end")
        {
            break;
        }
    }
    let agents = runtime.list_agents();
    assert_eq!(agents.len(), 2);
    assert_eq!(
        agents
            .iter()
            .find(|agent| agent.run_id == parent)
            .unwrap()
            .phase,
        AgentRunPhase::Done
    );
    assert!(runtime.take_inbox(parent).unwrap().is_empty());
}

#[tokio::test]
async fn spawn_failure_relays_metadata_only_terminal_notice() {
    // Given: the team's single worker slot is occupied.
    let (runtime, bus, _model) = fixture().await;
    let root = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: root.path().join("team.db"),
        ..Default::default()
    };
    let writer = storage::Storage::open(config.clone()).unwrap();
    let store = runtime::team_context::TeamStore {
        config,
        writer: writer.handle(),
        id: "relay-test".into(),
    };
    let mut receiver = bus.subscribe();
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".into(),
        RunConfig {
            topology: CoordinationTopology::DynamicTeam { max_workers: 1 },
            team_store: Some(store),
            delegation_value: Some("independent work".into()),
            ..Default::default()
        },
    );
    running(&mut receiver, parent).await;
    let occupied = runtime
        .delegate_background_as_child(parent, Role::Worker, "PENDING", RunConfig::default())
        .unwrap();
    running(&mut receiver, occupied).await;
    // When: registration rejects another worker before the agent loop starts.
    let prompt = "confidential rejected task";
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, prompt, RunConfig::default())
        .unwrap();
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Error));
    // Then: the bypass path delivers exactly one metadata-only failure.
    let content = notice(&runtime, parent, child);
    assert!(content.starts_with("failed:"));
    assert!(!content.contains(prompt));
    runtime.cancel(parent).unwrap();
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Error));
    runtime.cancel(occupied).unwrap();
    assert_eq!(runtime.wait(occupied).await, Ok(AgentRunPhase::Error));
}
