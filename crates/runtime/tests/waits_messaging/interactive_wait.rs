use crate::support;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig, RunId};
use serde_json::{Value, json};
use std::sync::Arc;
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

async fn exercise(wait: Option<Value>) {
    let model = Arc::new(ScriptedModel::new([]));
    let background = wait.is_some();
    let mut args = json!({"target":{"role":"worker"},"interactive":true,"prompt":"CHILD"});
    if background {
        args["background"] = json!(true);
    }
    let mut steps = vec![Ok(tool_response("delegate", "delegate", args))];
    if let Some(wait) = wait {
        steps.push(Ok(tool_response("wait", "wait", wait)));
    }
    steps.push(Ok(tool_response(
        "output",
        "run_output",
        json!({"run_id":"run-2"}),
    )));
    steps.push(Ok(tool_response(
        "finish",
        "finish",
        json!({"result":"parent done"}),
    )));
    model.add_keyed("PARENT", steps).await;
    model
        .add_keyed(
            "CHILD",
            [Ok(text_response("review complete", FinishReason::Stop))],
        )
        .await;
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_sequential_run_ids();
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
    let observed = model.observed().await;
    let last = observed.last().unwrap();
    let result = |id: &str| -> Value {
        last.iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } if tool_call_id == id => {
                    let ToolResultContent::Text { text } = content.first().unwrap() else {
                        panic!("expected text")
                    };
                    assert!(!is_error, "{text}");
                    Some(serde_json::from_str(text).unwrap())
                }
                _ => None,
            })
            .unwrap()
    };
    let snapshot = result(if background { "wait" } else { "delegate" });
    let turn = if background {
        assert_eq!(snapshot["completed_run_ids"], json!(["run-2"]));
        assert_eq!(snapshot["attention_run_ids"], json!([]));
        assert_eq!(snapshot["messages"], json!([]));
        &snapshot["runs"][0]
    } else {
        &snapshot
    };
    assert_eq!(turn["run_id"], "run-2");
    assert_eq!(turn["phase"], "Waiting");
    assert_eq!(turn["status"], "turn_completed");
    assert_eq!(turn["output"], "review complete");
    assert!(turn["turn_generation"].as_u64().is_some());
    let output = result("output");
    assert_eq!(output["output"], "review complete");
    assert_eq!(output["turn_generation"], turn["turn_generation"]);
    assert_eq!(
        runtime.inspect_agent(RunId::new(2)).unwrap().phase,
        AgentRunPhase::Waiting
    );
    runtime.cancel(RunId::new(2)).unwrap();
    runtime.wait(RunId::new(2)).await.unwrap();
}

#[tokio::test]
async fn background_interactive_child_returns_output_from_legacy_wait() {
    exercise(Some(json!({"run_id":"run-2"}))).await;
}

#[tokio::test]
async fn interactive_child_returns_output_from_run_ids_all() {
    exercise(Some(json!({"run_ids":["run-2"],"mode":"all"}))).await;
}

#[tokio::test]
async fn foreground_interactive_delegate_returns_turn_output() {
    exercise(None).await;
}
