mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig, RuntimeError};
use sandbox::DirectSandbox;
use serde_json::{Value, json};
use support::{ScriptedModel, text_response, tool_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

async fn exercise(
    steps: Vec<providers::ChatResponse>,
    child: Option<Result<providers::ChatResponse, RuntimeError>>,
) -> (String, bool) {
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "PARENT",
            steps.into_iter().map(Ok).chain([Ok(tool_response(
                "finish",
                "finish",
                json!({"result":"done"}),
            ))]),
        )
        .await;
    match child {
        Some(response) => model.add_keyed("CHILD", [response]).await,
        None => model.gate_key("CHILD", Arc::new(Notify::new())).await,
    }
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, model.clone());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
    for run in runtime.list_agents() {
        if matches!(
            run.phase,
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
        ) {
            runtime.cancel(run.run_id).unwrap();
            runtime.wait(run.run_id).await.unwrap();
        }
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
            } if tool_call_id == "output" => {
                let ToolResultContent::Text { text } = content.first().unwrap();
                Some((text.clone(), *is_error))
            }
            _ => None,
        })
        .expect("run_output response reaches the model")
}

fn spawn() -> providers::ChatResponse {
    tool_response(
        "spawn",
        "delegate",
        json!({"background":true, "role":"worker", "prompt":"CHILD"}),
    )
}

fn output(run: &str) -> providers::ChatResponse {
    tool_response("output", "run_output", json!({"run_id":run}))
}

fn decoded(result: (String, bool)) -> Value {
    assert!(!result.1, "unexpected tool error: {}", result.0);
    serde_json::from_str(&result.0).expect("structured output")
}

#[tokio::test]
async fn final_text_when_background_child_completed() {
    // Given: a background child with a known final answer.
    let steps = vec![
        spawn(),
        tool_response("wait", "wait", json!({"run_id":"run-2"})),
        output("run-2"),
    ];
    // When: the parent retrieves the completed child's output.
    let result = decoded(
        exercise(
            steps,
            Some(Ok(text_response("child final answer", FinishReason::Stop))),
        )
        .await,
    );
    // Then: the final output is returned, independent of completion inbox delivery.
    assert_eq!(
        result,
        json!({"run_id":"run-2", "phase":"Done", "status":"completed", "output":"child final answer", "reason":null})
    );
}

#[tokio::test]
async fn still_running_without_text_when_child_in_flight() {
    // Given: the child's model never completes until cancelled by cleanup.
    let steps = vec![spawn(), output("run-2")];
    // When: output is requested without waiting.
    let result = decoded(exercise(steps, None).await);
    // Then: this is an explicit nonterminal snapshot, not an empty success.
    assert_eq!(result["status"], "still_running");
    assert!(matches!(
        result["phase"].as_str(),
        Some("Pending" | "Running")
    ));
    assert_eq!(result["output"], Value::Null);
    assert_eq!(result["reason"], Value::Null);
}

#[tokio::test]
async fn cancelled_status_and_reason_when_child_cancelled() {
    // Given: a blocked child that the parent cancels and awaits.
    let steps = vec![
        spawn(),
        tool_response("cancel", "cancel", json!({"run_id":"run-2"})),
        tool_response("wait", "wait", json!({"run_id":"run-2"})),
        output("run-2"),
    ];
    // When: output is requested after cancellation.
    let result = decoded(exercise(steps, None).await);
    // Then: cancellation is distinguishable from other failures.
    assert_eq!(
        result,
        json!({"run_id":"run-2", "phase":"Error", "status":"cancelled", "output":null, "reason":"cancelled"})
    );
}

#[tokio::test]
async fn error_reason_when_child_failed() {
    // Given: a model failure rather than cancellation.
    let steps = vec![
        spawn(),
        tool_response("wait", "wait", json!({"run_id":"run-2"})),
        output("run-2"),
    ];
    let failure = RuntimeError::Model {
        reason: "provider unavailable".into(),
    };
    let expected = failure.to_string();
    // When: the failed child's output is requested.
    let result = decoded(exercise(steps, Some(Err(failure))).await);
    // Then: the original error survives terminal publication.
    assert_eq!(result["status"], "failed");
    assert_eq!(result["reason"], expected);
    assert_eq!(result["output"], Value::Null);
}

#[tokio::test]
async fn structured_error_when_run_unknown() {
    // Given: an ID with no registered run.
    // When: the parent attempts to retrieve it.
    let (text, is_error) = exercise(vec![output("run-999")], None).await;
    // Then: an unknown-run error is surfaced rather than data.
    assert!(is_error);
    let result: Value = serde_json::from_str(&text).expect("structured error");
    assert_eq!(result["code"], "unknown_run");
}

#[tokio::test]
async fn structured_error_when_reading_self() {
    // Given: the caller's own ID.
    // When: it attempts to read itself.
    let (text, is_error) = exercise(vec![output("run-1")], None).await;
    // Then: the same self-recipient restriction as messaging applies.
    assert!(is_error);
    let result: Value = serde_json::from_str(&text).expect("structured error");
    assert_eq!(result["code"], "run_output_denied");
}
