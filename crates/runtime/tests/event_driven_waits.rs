//! An Orchestrator's `wait` lasts until something happens, so waiting on a long
//! child neither costs model turns nor trips the loop guards, while a wait that
//! keeps returning nothing new is still stopped.
mod support;

use std::sync::Arc;
use std::time::Duration;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use runtime::{AgentRuntime, Role, RunConfig, RunId, RunStore};
use serde_json::json;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, tool_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

fn delegate() -> Result<providers::ChatResponse, runtime::RuntimeError> {
    Ok(tool_response(
        "delegate",
        "delegate",
        json!({"target": {"role": "worker"}, "background": true, "prompt": "CHILD"}),
    ))
}

/// A wait as models wrote it under the old ten-minute deadline; the agent loop
/// drops the timeout before validation.
fn wait(id: &str) -> Result<providers::ChatResponse, runtime::RuntimeError> {
    Ok(tool_response(
        id,
        "wait",
        json!({"run_id": "run-2", "timeout_ms": 600000}),
    ))
}

fn finish() -> Result<providers::ChatResponse, runtime::RuntimeError> {
    Ok(tool_response("finish", "finish", json!({"result": "done"})))
}

fn runtime(model: &Arc<ScriptedModel>, bus: &Arc<EventBus>) -> AgentRuntime {
    AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus.clone())),
        model.clone(),
    )
    .with_sequential_run_ids()
}

/// Model requests whose first user text starts with `marker`.
async fn requests(model: &ScriptedModel, marker: &str) -> usize {
    model
        .observed()
        .await
        .iter()
        .filter(|messages| {
            messages
                .iter()
                .find(|message| message.role == providers::Role::User)
                .and_then(|message| message.content.first())
                .is_some_and(|block| {
                    matches!(block, providers::ContentBlock::Text { text } if text.starts_with(marker))
                })
        })
        .count()
}

/// Resolves when `run` has entered Waiting `count` times.
async fn waiting(receiver: &mut event_bus::EventReceiver, run: RunId, count: usize) {
    let mut seen = 0;
    while seen < count {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            receiver.recv().await.expect("event").kind
            && run_id == run.to_string()
            && to == AgentRunPhase::Waiting
        {
            seen += 1;
        }
    }
}

/// The reason of `run`'s terminal Error transition, if any.
fn error_reason(events: &[event_bus::Event], run: RunId) -> Option<String> {
    events.iter().find_map(|event| match &event.kind {
        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Error,
            reason,
            ..
        }) if *run_id == run.to_string() => reason.clone(),
        _ => None,
    })
}

#[tokio::test(start_paused = true)]
async fn one_wait_covers_hours_of_child_work_without_repeated_turns() {
    // Given: an Orchestrator waiting on a child that works for hours. Before,
    // each ten-minute wait cost a turn and nine identical waits stopped it.
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed("PARENT", [delegate(), wait("wait"), finish()])
        .await;
    let child_gate = Arc::new(Notify::new());
    model.gate_key("CHILD", Arc::clone(&child_gate)).await;
    model
        .add_keyed(
            "CHILD",
            [Ok(support::text_response(
                "child done",
                providers::FinishReason::Stop,
            ))],
        )
        .await;
    let bus = Arc::new(EventBus::new(4096));
    let mut phases = bus.subscribe();
    let runtime = runtime(&model, &bus);
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    waiting(&mut phases, parent, 1).await;
    // When: hours pass before the child finishes.
    tokio::time::advance(Duration::from_secs(6 * 60 * 60)).await;
    child_gate.notify_one();
    // Then: the single wait returned the completion and the run finished.
    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(requests(&model, "PARENT").await, 3);
}

#[tokio::test]
async fn a_reported_nonblocking_question_does_not_turn_waits_into_a_busy_loop() {
    // Given: a child that asks a nonblocking question and keeps working.
    let directory = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: directory.path().join("questions.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "PARENT",
            [delegate(), wait("wait-1"), wait("wait-2"), finish()],
        )
        .await;
    let child_gate = Arc::new(Notify::new());
    child_gate.notify_one();
    model.gate_key("CHILD", Arc::clone(&child_gate)).await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(tool_response(
                    "ask",
                    "ask_user",
                    json!({"title": "Prefer A or B? I'll keep going meanwhile.", "blocking": false}),
                )),
                Ok(support::text_response(
                    "child done",
                    providers::FinishReason::Stop,
                )),
            ],
        )
        .await;
    let bus = Arc::new(EventBus::new(4096));
    let mut phases = bus.subscribe();
    let runtime =
        runtime(&model, &bus).with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    // When: the second, identical wait is suspended before the child finishes.
    waiting(&mut phases, parent, 2).await;
    child_gate.notify_one();
    // Then: the question ended only the first wait; the second lasted until the
    // child finished, so the parent made no extra requests and was not stopped.
    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(requests(&model, "PARENT").await, 4);
    let observed = model.observed().await;
    let results: Vec<_> = observed
        .last()
        .unwrap()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            providers::ContentBlock::ToolResult {
                tool_call_id,
                content,
                ..
            } if tool_call_id.starts_with("wait-") => match content.first() {
                Some(providers::ToolResultContent::Text { text }) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let first: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
    assert_eq!(first["attention_run_ids"], json!(["run-2"]));
    assert_eq!(first["runs"][0]["status"], "still_running");
    // With the legacy timeout dropped, a single-run wait that only saw the
    // completion keeps its plain phase result.
    assert_eq!(results[1], "Done");
}

#[tokio::test]
async fn identical_waits_on_a_finished_child_are_still_stopped() {
    // Given: an Orchestrator that keeps waiting on a child it already saw finish.
    let model = Arc::new(ScriptedModel::new([]));
    let mut script = vec![delegate()];
    script.extend((0..12).map(|index| wait(&format!("wait-{index}"))));
    script.push(finish());
    model.add_keyed("PARENT", script).await;
    model
        .add_keyed(
            "CHILD",
            [Ok(support::text_response(
                "child done",
                providers::FinishReason::Stop,
            ))],
        )
        .await;
    let bus = Arc::new(EventBus::new(4096));
    let mut receiver = bus.subscribe();
    let runtime = runtime(&model, &bus);
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    // When: every wait after the first returns at once with nothing new.
    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Error);
    // Then: the identical-call guard stops the loop.
    let events = support::drain_events(&mut receiver).await;
    let reason = error_reason(&events, parent).unwrap();
    assert!(
        reason.starts_with("IdenticalToolCalls: tool=wait; "),
        "{reason}"
    );
    assert!(reason.contains("without progress"), "{reason}");
}
