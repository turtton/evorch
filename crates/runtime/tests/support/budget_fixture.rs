use super::*;

pub(super) async fn run_calls(count: u32, config: RunConfig) -> Vec<Event> {
    run_calls_in_batches(count, config, false).await
}

pub(super) async fn run_calls_in_batches(count: u32, config: RunConfig, batch: bool) -> Vec<Event> {
    let file = tempfile::NamedTempFile::new().expect("file");
    std::fs::write(file.path(), "budget fixture").expect("write");
    let mut script = Vec::new();
    for index in 0..count {
        let mut response = tool_response(
            &index.to_string(),
            "read",
            serde_json::json!({"path": file.path()}),
        );
        response.usage.input_tokens = 3;
        response.usage.output_tokens = 2;
        script.push(Ok(response));
    }
    if batch {
        let content = script
            .iter()
            .flat_map(|response| response.as_ref().expect("response").message.content.clone())
            .collect();
        let mut response = text_response("", FinishReason::ToolUse);
        response.message.content = content;
        script = vec![Ok(response)];
    }
    script.push(Ok(text_response("done", FinishReason::Stop)));
    let bus = Arc::new(EventBus::new(4096));
    let mut receiver = bus.subscribe();
    let executor = tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    );
    let model = Arc::new(ScriptedModel::new(script));
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone());
    let run = runtime.delegate_background(Role::Worker, "budget".into(), config);
    let phase = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.wait(run))
        .await
        .expect("completion deadline")
        .expect("wait");
    let mut events = Vec::new();
    loop {
        let event = receiver.recv().await.expect("event");
        let terminal = matches!(
            &event.kind,
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                to: AgentRunPhase::Done | AgentRunPhase::Error,
                ..
            })
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    assert_eq!(
        phase,
        if events.iter().any(
            |event| matches!(&event.kind, EventKind::Diagnostic(d) if d.source == "budget_tracker")
        ) {
            AgentRunPhase::Error
        } else {
            AgentRunPhase::Done
        }
    );
    let tool_limit = events.iter().any(|event| matches!(&event.kind, EventKind::Diagnostic(d) if d.source == "budget_tracker" && d.detail.contains("max_tool_calls=5")));
    if tool_limit {
        assert_eq!(model.observed().await.len(), if batch { 1 } else { 5 });
    }
    events
}
