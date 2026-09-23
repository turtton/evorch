mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig};
use serde_json::{Value, json};
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

async fn execute_wait(arguments: Value) -> (String, bool) {
    execute_wait_with_message(arguments, false).await
}

async fn execute_wait_with_message(arguments: Value, send_message: bool) -> (String, bool) {
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "first",
                    "delegate",
                    json!({"prompt":"CHILD", "background":true}),
                )),
                Ok(tool_response(
                    "second",
                    "delegate",
                    json!({"prompt":"CHILD2", "background":true}),
                )),
                Ok(tool_response("wait-result", "wait", arguments)),
                Ok(tool_response("finish", "finish", json!({"result":"done"}))),
            ],
        )
        .await;
    let mut child_responses = Vec::new();
    if send_message {
        child_responses.push(Ok(tool_response(
            "message",
            "send",
            json!({"run_id":"run-1", "message":"Review this finding"}),
        )));
    }
    child_responses.push(Ok(text_response("first completed", FinishReason::Stop)));
    model.add_keyed("CHILD", child_responses).await;
    model
        .add_keyed(
            "CHILD2",
            [Ok(text_response("second completed", FinishReason::Stop))],
        )
        .await;
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
    for run in runtime.list_agents() {
        runtime.wait(run.run_id).await.unwrap();
    }
    model
        .observed()
        .await
        .iter()
        .rev()
        .flat_map(|messages| messages.iter())
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == "wait-result" => {
                let ToolResultContent::Text { text } = content.first().unwrap();
                Some((text.clone(), *is_error))
            }
            _ => None,
        })
        .expect("wait tool result reached model")
}

#[tokio::test]
async fn legacy_single_run_wait_preserves_phase_result() {
    let (result, failed) = execute_wait(json!({"run_id":"run-2"})).await;
    assert!(!failed, "{result}");
    assert_eq!(result, "Done");
}

#[tokio::test]
async fn multi_run_wait_delivers_one_structured_completion_batch_to_model() {
    let (result, failed) = execute_wait(json!({"run_ids":["run-2","run-3"],"mode":"all"})).await;
    assert!(!failed, "{result}");
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["completed_run_ids"], json!(["run-2", "run-3"]));
    assert_eq!(result["runs"][0]["output"], "first completed");
    assert_eq!(result["runs"][1]["output"], "second completed");
}

#[tokio::test]
async fn invalid_self_wait_returns_error_and_keeps_parent_running() {
    let (result, failed) = execute_wait(json!({"run_id":"run-1"})).await;
    assert!(failed);
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap()["code"],
        "wait_denied"
    );
}

#[tokio::test]
async fn legacy_wait_returns_a_structured_snapshot_for_an_inbox_message() {
    let (result, failed) = execute_wait_with_message(json!({"run_id":"run-2"}), true).await;
    assert!(!failed, "{result}");
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["inbox_ready"], true);
}
