mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, RunConfig, RunId, RunStore, RuntimeError};
use sandbox::DirectSandbox;
use serde_json::{Value, json};
use storage::{Storage, StorageConfig};
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
    let runtime = AgentRuntime::new(bus, executor, model.clone()).with_sequential_run_ids();
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
                let ToolResultContent::Text { text } = content.first().unwrap() else {
                    panic!("expected text result");
                };
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
        json!({"target": {"role": "worker"},"background":true, "prompt":"CHILD"}),
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
        json!({"run_id":"run-2", "phase":"Done", "status":"completed", "output":"child final answer", "reason":null,
            "continuable":false, "continuation_blocked":runtime::RunRestoreFailure::StorageNotConfigured.to_string()})
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

async fn observe_waiting_child(question: Option<bool>) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let model = Arc::new(ScriptedModel::new([]));
    let parent_gate = Arc::new(Notify::new());
    model.gate_key("PARENT", parent_gate.clone()).await;
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "spawn",
                    "delegate",
                    json!({"target":{"role":"worker"}, "background":true, "interactive":true, "prompt":"CHILD"}),
                )),
                Ok(output("run-2")),
                Ok(tool_response(
                    "wait-snapshot",
                    "wait",
                    json!({"run_ids":["run-2"], "timeout_ms":0}),
                )),
                Ok(tool_response("finish", "finish", json!({"result":"done"}))),
            ],
        )
        .await;
    let mut child = Vec::new();
    if let Some(blocking) = question {
        child.push(Ok(tool_response(
            "ask",
            "ask_user",
            json!({"title":"Which scope?", "blocking":blocking}),
        )));
    }
    child.push(Ok(text_response("Waiting for input", FinishReason::Stop)));
    model.add_keyed("CHILD", child).await;
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_sequential_run_ids()
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    parent_gate.notify_one();

    // Observe the child's actual idle boundary before allowing either snapshot.
    let child = RunId::new(2);
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Waiting,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == child.to_string()
        {
            break;
        }
    }
    let first_parent_follow_up = if question.is_some() { 3 } else { 2 };
    for index in first_parent_follow_up..first_parent_follow_up + 2 {
        model.wait_for_request(index).await;
        parent_gate.notify_one();
    }
    model.wait_for_request(first_parent_follow_up + 2).await;
    runtime.cancel(child).unwrap();
    runtime.wait(child).await.unwrap();
    parent_gate.notify_one();
    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);

    let observed = model.observed().await;
    let last = observed.last().unwrap();
    let result = |id: &str| {
        last.iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } if tool_call_id == id => {
                    let ToolResultContent::Text { text } = content.first().unwrap() else {
                        panic!("expected text result");
                    };
                    assert!(!is_error, "{text}");
                    Some(serde_json::from_str::<Value>(text).unwrap())
                }
                _ => None,
            })
            .unwrap()
    };
    let output = result("output");
    let wait = result("wait-snapshot");
    for snapshot in [&output, &wait["runs"][0]] {
        assert_eq!(snapshot["phase"], "Waiting");
        assert_eq!(snapshot["status"], "waiting");
        assert_eq!(snapshot["output"], Value::Null);
        assert_eq!(snapshot["reason"], Value::Null);
    }
    assert_eq!(wait["completed_run_ids"], json!([]));
    assert_eq!(wait["runs"][0]["needs_user_input"], question == Some(true));
    assert_eq!(wait["runs"][0]["has_pending_question"], question.is_some());
    assert_eq!(wait["timed_out"], question.is_none());
    assert_eq!(
        wait["attention_run_ids"],
        if question.is_some() {
            json!(["run-2"])
        } else {
            json!([])
        }
    );
}

#[tokio::test]
async fn waiting_child_is_distinct_from_running_without_question_attention() {
    observe_waiting_child(None).await;
}

#[tokio::test]
async fn waiting_child_preserves_optional_question_attention() {
    observe_waiting_child(Some(false)).await;
}

#[tokio::test]
async fn waiting_child_preserves_required_question_attention() {
    observe_waiting_child(Some(true)).await;
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
        json!({"run_id":"run-2", "phase":"Error", "status":"cancelled", "output":null, "reason":"cancelled",
            "continuable":false, "continuation_blocked":runtime::RunRestoreFailure::Cancelled.to_string()})
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
