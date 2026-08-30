mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, ToolEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

use support::{ScriptedModel, collect_events, text_response, tool_response};

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
async fn orchestrator_edit_is_denied_without_tool_started() {
    // Given
    let (runtime, bus) = runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "edit-1",
            "edit",
            json!({ "path": "ignored", "new_string": "x" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "coordinate".to_string(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = collect_events(&mut events, 5).await;

    // Then
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "edit"
    )));
    assert_eq!(
        runtime
            .inspect_agent(run_id)
            .expect("run exists")
            .message_count,
        4
    );
}

#[tokio::test]
async fn worker_edit_emits_started_and_completed() {
    // Given
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("worker.txt");
    let (runtime, bus) = runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "edit-2",
            "edit",
            json!({ "path": path, "new_string": "written" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id =
        runtime.delegate_background(Role::Worker, "edit".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = collect_events(&mut events, 7).await;

    // Then
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id }) if tool_name == "edit" && call_id == "edit-2")));
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolCompleted { tool_name, call_id, is_error: false }) if tool_name == "edit" && call_id == "edit-2")));
}

#[tokio::test]
async fn explorer_shell_is_denied_without_execution() {
    // Given
    let (runtime, bus) = runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "shell-1",
            "shell",
            json!({ "command": "false" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id =
        runtime.delegate_background(Role::Explorer, "inspect".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = collect_events(&mut events, 5).await;

    // Then
    assert!(!events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "shell")));
}
