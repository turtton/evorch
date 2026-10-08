//! A tool call whose streamed arguments were not valid JSON (assembled as `null`)
//! is not run; the model is told why instead of getting a schema mismatch.

mod support;

use std::sync::Arc;

use event_bus::{AgentRunPhase, EventKind, ToolEvent};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use support::{ScriptedModel, drain_events, text_response, tool_response};

#[tokio::test]
async fn null_arguments_are_rejected_without_running_the_tool() {
    // Given: a model whose first call has unparsable (null) arguments.
    let bus = Arc::new(event_bus::EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let dir = tempfile::tempdir().unwrap();
    let config = config::Config::default();
    let model = Arc::new(ScriptedModel::new(
        [
            tool_response("call-1", "read", serde_json::Value::Null),
            text_response("done", FinishReason::Stop),
        ]
        .into_iter()
        .map(Ok),
    ));
    let composed = compose_runtime(RuntimeComposition {
        user_config_dir: Some(dir.path().join("empty-user-config")),
        config: &config,
        executor: Arc::new(tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )),
        bus,
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(dir.path()).unwrap(),
        ),
        env: Arc::new(routing::MapEnv::default()),
        model_source: ModelSource::Fixed(model.clone()),
        workspace: None,
    })
    .unwrap();
    // When: the run consumes the script.
    let run = composed.runtime.delegate_background(
        Role::Worker,
        "malformed".into(),
        RunConfig::default(),
    );
    let phase = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        composed.runtime.wait(run),
    )
    .await
    .unwrap()
    .unwrap();
    // Then: the run continues, the tool never started, and the model sees the cause.
    assert_eq!(phase, AgentRunPhase::Done);
    let events = drain_events(&mut receiver).await;
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "read"
    )));
    let second = model.observed().await.remove(1);
    let result = second
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == "call-1" => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("tool result for the malformed call");
    assert!(result.1);
    assert!(matches!(
        &result.0[..],
        [ToolResultContent::Text { text }] if text.contains("not valid JSON")
    ));
}
