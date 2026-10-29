//! Output-bearing observations must not create another turn at a natural Stop.
mod support;

use std::sync::Arc;

use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason};
use runtime::{AgentRuntime, Role, RunConfig, RunId};
use serde_json::json;
use support::{ScriptedModel, text_response, tool_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

async fn observed_output_stops(tool: &str, child_output: &str) {
    let parent_gate = Arc::new(Notify::new());
    let child_gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::new([]));
    model.gate_key("PARENT", parent_gate.clone()).await;
    model.gate_key("CHILD", child_gate.clone()).await;
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "delegate",
                    "delegate",
                    json!({"target":{"role":"worker"}, "prompt":"CHILD", "background":true}),
                )),
                Ok(tool_response(
                    "observe",
                    tool,
                    if tool == "wait" {
                        json!({"run_ids":["run-2"]})
                    } else {
                        json!({"run_id":"run-2"})
                    },
                )),
                Ok(text_response(
                    "I have the child output.",
                    FinishReason::Stop,
                )),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [Ok(text_response(child_output, FinishReason::Stop))],
        )
        .await;
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_sequential_run_ids();
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    model.wait_for_request(0).await;
    parent_gate.notify_one();
    // Both the delegate child and the parent's second request have started.
    model.wait_for_request(2).await;
    child_gate.notify_one();
    assert_eq!(runtime.wait(RunId::new(2)).await, Ok(AgentRunPhase::Done));
    parent_gate.notify_one();
    let last = model.wait_for_request(3).await;
    assert!(last.iter().flat_map(|message| &message.content).any(|block| {
        matches!(block, ContentBlock::ToolResult { content, .. } if serde_json::to_string(content).unwrap().contains("child result"))
    }));
    if tool == "wait" && child_output.len() > 4096 {
        let result = last
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| {
                if let ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } = block
                    && tool_call_id == "observe"
                {
                    return Some(
                        serde_json::from_str::<serde_json::Value>(&content[0].text_or_image_note())
                            .unwrap(),
                    );
                }
                None
            })
            .unwrap();
        assert_eq!(result["runs"][0]["output_truncated"], true);
        assert!(result["runs"][0]["output"].as_str().unwrap().len() <= 4096);
    }
    parent_gate.notify_one();
    // Completion or a forbidden extra request are explicit signals; no wall clock.
    tokio::select! {
        phase = runtime.wait(parent) => assert_eq!(phase, Ok(AgentRunPhase::Done)),
        extra = model.wait_for_request(4) => panic!("redundant request after Stop: {extra:?}"),
    }
    let observed = model.observed().await;
    assert_eq!(observed.len(), 4, "three parent requests plus one child");
    let parent_requests: Vec<_> = observed
        .iter()
        .filter(|messages| {
            messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == "PARENT"))
            })
        })
        .collect();
    assert_eq!(parent_requests.len(), 3);
    for pair in parent_requests.windows(2) {
        // Independent append-only prefix assertion: no sent history is rewritten.
        assert_eq!(&pair[1][..pair[0].len()], pair[0].as_slice());
    }
    assert!(
        !last
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| {
                matches!(block, ContentBlock::Text { text } if text.contains("[agent-message"))
            })
    );
}

#[tokio::test]
async fn wait_output_then_natural_stop_needs_no_completion_turn() {
    observed_output_stops("wait", "child result").await;
}

#[tokio::test]
async fn run_output_then_natural_stop_needs_no_completion_turn() {
    observed_output_stops("run_output", "child result").await;
}

#[tokio::test]
async fn truncated_wait_output_still_acknowledges_the_completion() {
    observed_output_stops("wait", &format!("child result {}", "検証済み".repeat(2000))).await;
}
