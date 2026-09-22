mod support;
use event_bus::{EventBus, EventKind, LifecycleEvent, RunActivity};
use providers::FinishReason;
use runtime::{AgentRuntime, Role, RunConfig};
use std::sync::Arc;
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;
#[tokio::test]
async fn latest_context_breakdown_separates_schema_history_and_tool_payload() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("large.txt");
    std::fs::write(&file, "tool output line\n".repeat(150)).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let executor = ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    );
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "read",
            "read",
            serde_json::json!({"path":file}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model);
    let run = runtime.delegate_background(
        Role::Worker,
        "Inspect the file".into(),
        RunConfig::default(),
    );
    runtime.wait(run).await.unwrap();
    let mut estimates = vec![];
    let mut activities = vec![];
    for event in support::drain_events(&mut events).await {
        if let EventKind::Lifecycle(LifecycleEvent::RunProgress {
            activity, context, ..
        }) = event.kind
        {
            activities.push(activity);
            if let Some(c) = context {
                estimates.push(c);
            }
        }
    }
    assert_eq!(estimates.len(), 2);
    assert!(estimates[0].tool_definitions > 0);
    assert_eq!(estimates[0].tool_outputs, 0);
    assert!(estimates[1].tool_outputs > 100);
    assert!(estimates[1].projected_tokens > estimates[0].projected_tokens);
    assert!(estimates[1].projected_tokens >= estimates[1].tool_definitions);
    assert!(activities.contains(&RunActivity::Tools));
    assert_eq!(activities.last(), Some(&RunActivity::Idle));
}
