use crate::support;

use std::sync::Arc;

use agents::Role;
use event_bus::EventBus;
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig};
use serde_json::{Value, json};
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

#[tokio::test]
async fn model_cannot_delegate_to_an_orchestrator_under_any_spelling() {
    // Given: a root Orchestrator whose model asks to delegate to an Orchestrator,
    // in the foreground and background, and with each casing.
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "lower",
            "delegate",
            json!({"target":{"role":"orchestrator"}, "prompt":"X"}),
        )),
        Ok(tool_response(
            "upper",
            "delegate",
            json!({"target":{"role":"ORCHESTRATOR"}, "prompt":"X", "background":true}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone());

    // When: the root runs to completion.
    let root = runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    runtime.wait(root).await.unwrap();

    // Then: both calls fail with the dedicated code, and no child was created.
    let observed = model.observed().await;
    let results: Vec<(String, bool, Value)> = observed
        .last()
        .unwrap()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                let ToolResultContent::Text { text } = content.first()? else {
                    return None;
                };
                Some((
                    tool_call_id.clone(),
                    *is_error,
                    serde_json::from_str(text).unwrap_or(Value::Null),
                ))
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "{results:?}");
    for (call, is_error, value) in results {
        assert!(is_error, "{call}");
        assert_eq!(value["code"], "role_not_delegable", "{call}: {value}");
    }
    assert_eq!(runtime.list_agents().len(), 1);
}
