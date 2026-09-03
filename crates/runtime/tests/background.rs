mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig, RunId, RuntimeError};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tools::ToolExecutor;

use support::{ScriptedModel, collect_events, text_response};

fn runtime_with(model: ScriptedModel) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (
        AgentRuntime::new(Arc::clone(&bus), executor, Arc::new(model)),
        bus,
    )
}

#[tokio::test]
async fn background_start_is_observable_before_wait_and_completion_is_success_only() {
    // Given
    let (runtime, bus) = runtime_with(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let mut events = bus.subscribe();

    // When
    let run_id =
        runtime.delegate_background(Role::Worker, "work".to_string(), RunConfig::default());
    let first_three = collect_events(&mut events, 3).await;

    // Then
    assert!(first_three.iter().any(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::BackgroundTaskStarted { task_id }) if task_id == &run_id.to_string())));
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let remaining = collect_events(&mut events, 3).await;
    assert!(remaining.iter().any(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { task_id }) if task_id == &run_id.to_string())));
}

#[tokio::test]
async fn cancel_mid_model_turn_emits_cancelled_and_error() {
    // Given
    let gate = Arc::new(Notify::new());
    let (runtime, bus) = runtime_with(ScriptedModel::gated(
        [Ok(text_response("unused", FinishReason::Stop))],
        Arc::clone(&gate),
    ));
    let mut events = bus.subscribe();
    let run_id =
        runtime.delegate_background(Role::Worker, "blocked".to_string(), RunConfig::default());
    let _started = collect_events(&mut events, 4).await;

    // When
    assert_eq!(runtime.cancel(run_id), Ok(()));

    // Then
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Error));
    let events = collect_events(&mut events, 2).await;
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCancelled { task_id }) if task_id == &run_id.to_string())));
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { to: AgentRunPhase::Error, reason: Some(reason), .. }) if reason == "cancelled")));
}

#[tokio::test]
async fn send_message_to_unknown_run_returns_unknown_run() {
    // Given
    let (runtime, _bus) = runtime_with(ScriptedModel::new([]));
    let missing = RunId::new(999);

    // When
    let result = runtime.send_message(missing, "hello".to_string());

    // Then
    assert_eq!(
        result,
        Err(RuntimeError::UnknownRun {
            run_id: missing.to_string()
        })
    );
}
