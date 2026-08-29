mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig, RuntimeError};
use tools::ToolExecutor;

use support::{ScriptedModel, collect_events, text_response};

fn runtime_with(model: ScriptedModel) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(Arc::clone(&bus)));
    (
        AgentRuntime::new(Arc::clone(&bus), executor, Arc::new(model)),
        bus,
    )
}

#[tokio::test]
async fn run_emits_pending_running_done_in_order() {
    // Given
    let (runtime, bus) = runtime_with(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let mut events = bus.subscribe();

    // When
    let run_id =
        runtime.delegate_background(Role::Worker, "work".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = collect_events(&mut events, 4).await;

    // Then
    let lifecycle: Vec<&LifecycleEvent> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Lifecycle(event) => Some(event),
            EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_) => None,
        })
        .collect();
    assert!(matches!(
        lifecycle[0],
        LifecycleEvent::AgentRunStateChanged {
            from: AgentRunPhase::Pending,
            to: AgentRunPhase::Pending,
            ..
        }
    ));
    assert!(
        matches!(lifecycle[1], LifecycleEvent::BackgroundTaskStarted { task_id } if task_id == &run_id.to_string())
    );
    assert!(matches!(
        lifecycle[2],
        LifecycleEvent::AgentRunStateChanged {
            from: AgentRunPhase::Pending,
            to: AgentRunPhase::Running,
            reason: None,
            ..
        }
    ));
    assert!(matches!(
        lifecycle[3],
        LifecycleEvent::AgentRunStateChanged {
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
            ..
        }
    ));
}

#[tokio::test]
async fn model_error_transitions_run_to_error_with_reason() {
    // Given
    let error = RuntimeError::Model {
        reason: "offline".to_string(),
    };
    let (runtime, bus) = runtime_with(ScriptedModel::new([Err(error)]));
    let mut events = bus.subscribe();

    // When
    let run_id =
        runtime.delegate_background(Role::Explorer, "inspect".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Error));
    let events = collect_events(&mut events, 4).await;

    // Then
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id: event_run_id,
            to: AgentRunPhase::Error,
            reason: Some(reason),
            ..
        }) if event_run_id == &run_id.to_string() && reason.contains("offline")
    )));
    assert!(!events.iter().any(|event| matches!(
        event.kind,
        EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { .. })
    )));
}

#[tokio::test]
async fn interactive_run_waits_for_message_then_completes() {
    // Given
    let (runtime, bus) = runtime_with(ScriptedModel::new([
        Ok(text_response("question", FinishReason::Stop)),
        Ok(text_response("answer", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();
    let config = RunConfig { interactive: true };

    // When
    let run_id = runtime.delegate_background(Role::Reviewer, "review".to_string(), config);
    let mut observed_waiting = false;
    while !observed_waiting {
        let event = support::collect_events(&mut events, 1).await.remove(0);
        observed_waiting = matches!(
            event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                to: AgentRunPhase::Waiting,
                ..
            })
        );
    }
    assert_eq!(runtime.send_message(run_id, "continue".to_string()), Ok(()));

    // Then
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let inspection = runtime.inspect_agent(run_id).expect("run exists");
    assert_eq!(inspection.message_count, 4);
}
