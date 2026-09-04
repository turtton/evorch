mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

use support::{ScriptedModel, collect_events, text_response, tool_response};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_runs_keep_independent_histories_and_run_ids() {
    // Given
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed("short", [Ok(text_response("done", FinishReason::Stop))])
        .await;
    model
        .add_keyed(
            "tool",
            [
                Ok(tool_response(
                    "grep-1",
                    "grep",
                    json!({ "pattern": "missing", "path": "." }),
                )),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);
    let mut events = bus.subscribe();

    // When
    let short_id =
        runtime.delegate_background(Role::Reviewer, "short".to_string(), RunConfig::default());
    let tool_id =
        runtime.delegate_background(Role::Explorer, "tool".to_string(), RunConfig::default());
    let (short_phase, tool_phase) = tokio::join!(runtime.wait(short_id), runtime.wait(tool_id));

    // Then
    assert_eq!(short_phase, Ok(AgentRunPhase::Done));
    assert_eq!(tool_phase, Ok(AgentRunPhase::Done));
    assert_eq!(
        runtime
            .inspect_agent(short_id)
            .expect("short exists")
            .message_count,
        2
    );
    assert_eq!(
        runtime
            .inspect_agent(tool_id)
            .expect("tool exists")
            .message_count,
        4
    );
    let events = collect_events(&mut events, 12).await;
    let changed_ids: Vec<&str> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, .. }) => {
                Some(run_id.as_str())
            }
            EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_) => None,
        })
        .collect();
    let short_text = short_id.to_string();
    let tool_text = tool_id.to_string();
    assert!(changed_ids.contains(&short_text.as_str()));
    assert!(changed_ids.contains(&tool_text.as_str()));
}
