//! A tool call whose streamed arguments were not valid JSON (assembled as `null`)
//! is not run; the model is told why instead of getting a schema mismatch.

use crate::support;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use event_bus::{AgentRunPhase, EventKind, ToolEvent};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use support::{ScriptedModel, drain_events, text_response, tool_response};

struct EmptyRead(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl tools::Tool for EmptyRead {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn permissions(&self) -> tools::Permissions {
        tools::Permissions::read_only()
    }
    async fn execute(&self, _: serde_json::Value) -> Result<tools::ToolResult, tools::ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(tools::ToolResult::success("executed"))
    }
}

#[tokio::test]
async fn non_object_arguments_are_saved_as_objects_but_never_executed() {
    for input in [
        serde_json::Value::Null,
        serde_json::json!([]),
        serde_json::json!(42),
        serde_json::json!("partial"),
        serde_json::json!(false),
    ] {
        for name in ["read", "inbox"] {
            check_arguments(input.clone(), false, name).await;
        }
    }
}

#[tokio::test]
async fn valid_empty_object_arguments_can_execute() {
    for name in ["read", "inbox"] {
        check_arguments(serde_json::json!({}), true, name).await;
    }
}

async fn check_arguments(input: serde_json::Value, executable: bool, name: &str) {
    // Given: the execution input and its raw replay block share the same envelope.
    let bus = Arc::new(event_bus::EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let dir = tempfile::tempdir().unwrap();
    let config = config::Config::default();
    let starts = Arc::new(AtomicUsize::new(0));
    let mut executor = tools::ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(EmptyRead(starts.clone())))
        .unwrap();
    let mut call = tool_response("call-1", name, input.clone());
    call.message.content.insert(
        0,
        ContentBlock::Anthropic {
            block: serde_json::json!({
                "type":"tool_use","id":"call-1","name":name,
                "input":input,"extra":{"preserved":true}
            }),
            prefix_hash: None,
        },
    );
    let model = Arc::new(ScriptedModel::new(
        [call, text_response("done", FinishReason::Stop)]
            .into_iter()
            .map(Ok),
    ));
    let composed = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(dir.path().join("empty-user-config")),
        config: &config,
        executor: Arc::new(executor),
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
    let phase = composed.runtime.wait(run).await.unwrap();
    // Then: the run continues; only an original object envelope can start the tool.
    assert_eq!(phase, AgentRunPhase::Done);
    let events = drain_events(&mut receiver).await;
    assert_eq!(
        events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == name
        )),
        executable
    );
    assert_eq!(
        starts.load(Ordering::SeqCst),
        usize::from(executable && name == "read")
    );
    let second = model.observed().await.remove(1);
    let saved = second
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| {
            matches!(
                block,
                ContentBlock::ToolUse { .. } | ContentBlock::Anthropic { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(saved.len(), 2);
    assert!(
        matches!(saved[0], ContentBlock::Anthropic { block, .. } if block["input"] == serde_json::json!({}) && block["extra"]["preserved"] == true)
    );
    assert!(
        matches!(saved[1], ContentBlock::ToolUse { input, .. } if *input == serde_json::json!({}))
    );
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
    assert_eq!(result.1, !executable);
    if executable {
        return;
    }
    assert!(matches!(
        &result.0[..],
        [ToolResultContent::Text { text }] if text.contains("not valid JSON")
    ));
}
