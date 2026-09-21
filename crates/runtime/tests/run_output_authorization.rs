mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use serde_json::{Value, json};
use support::{ScriptedModel, text_response, tool_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

async fn read_related(siblings: bool) -> (Value, bool) {
    let model = Arc::new(ScriptedModel::new([]));
    model.gate_key("ROOT", Arc::new(Notify::new())).await;
    model
        .add_keyed(
            "SECRET",
            [Ok(text_response("private final text", FinishReason::Stop))],
        )
        .await;
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, model.clone());
    let root = runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    let target = runtime
        .delegate_background_as_child(root, Role::Worker, "SECRET", RunConfig::default())
        .unwrap();
    runtime.wait(target).await.unwrap();
    model
        .add_keyed(
            "READER",
            [
                Ok(tool_response(
                    "output",
                    "run_output",
                    json!({"run_id":target.to_string()}),
                )),
                Ok(tool_response("finish", "finish", json!({"result":"done"}))),
            ],
        )
        .await;
    let reader = if siblings {
        runtime
            .delegate_background_as_child(root, Role::Orchestrator, "READER", RunConfig::default())
            .unwrap()
    } else {
        runtime.delegate_background(Role::Orchestrator, "READER".into(), RunConfig::default())
    };
    assert_eq!(runtime.wait(reader).await.unwrap(), AgentRunPhase::Done);
    runtime.cancel(root).unwrap();
    runtime.wait(root).await.unwrap();
    let observed = model.observed().await;
    observed
        .iter()
        .rev()
        .flat_map(|messages| messages.iter())
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == "output" => {
                let ToolResultContent::Text { text } = content.first().unwrap();
                assert!(!text.contains("private final text"));
                Some((
                    serde_json::from_str(text).expect("structured error"),
                    *is_error,
                ))
            }
            _ => None,
        })
        .expect("tool result")
}

#[tokio::test]
async fn denied_when_target_is_foreign_run() {
    // Given: two unrelated run trees with a completed private result.
    // When: a foreign reader calls the real meta-tool.
    let (result, is_error) = read_related(false).await;
    // Then: no final text is disclosed.
    assert!(is_error);
    assert_eq!(result["code"], "run_output_denied");
}

#[tokio::test]
async fn denied_when_target_is_sibling_run() {
    // Given: a reader sharing the target's parent, but not its direct relationship.
    // When: the reader calls the real meta-tool.
    let (result, is_error) = read_related(true).await;
    // Then: sibling outputs remain private, like send_message.
    assert!(is_error);
    assert_eq!(result["code"], "run_output_denied");
}
