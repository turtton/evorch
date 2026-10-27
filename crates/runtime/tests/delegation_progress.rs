//! Delegation-only Orchestrators must make progress through child runs, while
//! polling loops over the same children are still detected as NoProgress.

mod support;

use std::sync::Arc;
use std::time::Duration;

use event_bus::{
    AgentRunPhase, DiagnosticSeverity, Event, EventBus, EventKind, LifecycleEvent,
    event::diagnostic_codes,
};
use providers::{ChatResponse, FinishReason};
use runtime::{AgentRuntime, Role, RunConfig, RunId};
use serde_json::json;
use support::{ScriptedModel, text_response, tool_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

const LIMIT: u32 = 2;

struct Outcome {
    phase: AgentRunPhase,
    no_progress: usize,
    error_reason: Option<String>,
}

impl Outcome {
    fn assert_done(&self) {
        assert_eq!(self.phase, AgentRunPhase::Done, "{:?}", self.error_reason);
        assert_eq!(self.no_progress, 0);
    }

    fn assert_no_progress(&self) {
        assert_eq!(self.phase, AgentRunPhase::Error);
        assert_eq!(self.no_progress, 1);
        assert!(
            self.error_reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with(diagnostic_codes::NO_PROGRESS)),
            "{:?}",
            self.error_reason
        );
    }
}

fn config() -> RunConfig {
    let mut config = RunConfig::default();
    config.budget.max_no_progress_rounds = LIMIT;
    // Isolate the round-based detector from the identical-call guard.
    config.budget.max_identical_tool_call_repeats = 100;
    config
}

fn finish() -> Result<ChatResponse, runtime::RuntimeError> {
    Ok(tool_response("finish", "finish", json!({"result":"done"})))
}

fn child(index: usize) -> String {
    format!("CHILD-{index:02}")
}

/// Sequential IDs: the parent is run-1 and the n-th delegated child is run-(n+2).
fn child_id(index: usize) -> String {
    format!("run-{}", index + 2)
}

async fn harness(
    parent: Vec<Result<ChatResponse, runtime::RuntimeError>>,
    children: usize,
) -> (Arc<ScriptedModel>, AgentRuntime, Arc<EventBus>) {
    let model = Arc::new(ScriptedModel::new([]));
    model.add_keyed("PARENT", parent).await;
    for index in 0..children {
        model
            .add_keyed(
                &child(index),
                [Ok(text_response("child done", FinishReason::Stop))],
            )
            .await;
    }
    let bus = Arc::new(EventBus::new(4096));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus.clone())),
        model.clone(),
    )
    .with_sequential_run_ids();
    (model, runtime, bus)
}

async fn run_parent(runtime: &AgentRuntime, bus: &EventBus) -> Outcome {
    let mut receiver = bus.subscribe();
    let parent = runtime.delegate_background(Role::Orchestrator, "PARENT".into(), config());
    collect(runtime, parent, &mut receiver).await
}

async fn collect(
    runtime: &AgentRuntime,
    parent: RunId,
    receiver: &mut event_bus::EventReceiver,
) -> Outcome {
    let phase = runtime.wait(parent).await.expect("parent wait");
    let parent = parent.to_string();
    let mut events: Vec<Event> = Vec::new();
    let mut error_reason = None;
    loop {
        let event = receiver.recv().await.expect("event");
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id, to, reason, ..
        }) = &event.kind
            && *run_id == parent
            && matches!(to, AgentRunPhase::Done | AgentRunPhase::Error)
        {
            error_reason.clone_from(reason);
            break;
        }
        events.push(event);
    }
    let no_progress = events
        .iter()
        .filter(|event| {
            matches!(&event.kind, EventKind::Diagnostic(d)
                if d.source == "budget_tracker"
                    && d.code == diagnostic_codes::NO_PROGRESS
                    && d.severity == DiagnosticSeverity::Error
                    && d.run_id.as_deref() == Some(parent.as_str()))
        })
        .count();
    Outcome {
        phase,
        no_progress,
        error_reason,
    }
}

fn delegate(index: usize, background: bool) -> Result<ChatResponse, runtime::RuntimeError> {
    Ok(tool_response(
        &format!("delegate-{index}"),
        "delegate",
        json!({"target": {"role": "worker"}, "prompt": child(index), "background": background}),
    ))
}

fn wait(call: &str, run_id: &str) -> Result<ChatResponse, runtime::RuntimeError> {
    Ok(tool_response(call, "wait", json!({"run_ids": [run_id]})))
}

fn run_output(index: usize, run_id: &str) -> Result<ChatResponse, runtime::RuntimeError> {
    Ok(tool_response(
        &format!("output-{index}"),
        "run_output",
        json!({"run_id": run_id}),
    ))
}

#[tokio::test]
async fn background_delegate_then_wait_rounds_exceeding_limit_finish_done() {
    // Given: 5 cycles of background delegate + completed wait (10 rounds > LIMIT).
    let cycles = 5;
    let mut script = Vec::new();
    for index in 0..cycles {
        script.push(delegate(index, true));
        script.push(wait(&format!("wait-{index}"), &child_id(index)));
    }
    script.push(finish());
    let (_model, runtime, bus) = harness(script, cycles).await;
    // When: the Orchestrator only delegates and receives child results.
    let outcome = run_parent(&runtime, &bus).await;
    // Then: spawning and newly received completions are progress.
    outcome.assert_done();
}

#[tokio::test]
async fn awaited_delegate_rounds_exceeding_limit_finish_done() {
    // Given: 5 awaited delegates, more rounds than LIMIT.
    let cycles = 5;
    let mut script: Vec<_> = (0..cycles).map(|index| delegate(index, false)).collect();
    script.push(finish());
    let (_model, runtime, bus) = harness(script, cycles).await;
    // When: the Orchestrator only runs awaited delegates.
    let outcome = run_parent(&runtime, &bus).await;
    // Then: each awaited delegation counts as progress.
    outcome.assert_done();
}

#[tokio::test]
async fn awaited_delegate_followed_by_waits_on_the_same_child_finishes_done() {
    // Given: each awaited delegate is followed by one redundant wait (streak 1 <= LIMIT).
    let cycles = 4;
    let mut script = Vec::new();
    for index in 0..cycles {
        script.push(delegate(index, false));
        script.push(wait(&format!("wait-{index}"), &child_id(index)));
    }
    script.push(finish());
    let (_model, runtime, bus) = harness(script, cycles).await;
    // When/Then: the re-observed completion is not progress, but the next spawn is.
    run_parent(&runtime, &bus).await.assert_done();
}

/// Holds the parent's second model request until the background child is
/// terminal, so the first `run_output` deterministically observes completion.
async fn run_output_after_child_completed(outputs: usize) -> Outcome {
    let mut script = vec![delegate(0, true)];
    script.extend((0..outputs).map(|index| run_output(index, &child_id(0))));
    script.push(finish());
    let (model, runtime, bus) = harness(script, 1).await;
    let gate = Arc::new(Notify::new());
    model.gate_key("PARENT", Arc::clone(&gate)).await;
    let mut receiver = bus.subscribe();
    let parent = runtime.delegate_background(Role::Orchestrator, "PARENT".into(), config());
    gate.notify_one();
    let child = child_id(0).parse::<RunId>().expect("run id");
    loop {
        if runtime.list_agents().iter().any(|run| run.run_id == child) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        runtime.wait(child).await.expect("child wait"),
        AgentRunPhase::Done
    );
    let releaser = tokio::spawn(async move {
        loop {
            gate.notify_one();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    let outcome = collect(&runtime, parent, &mut receiver).await;
    releaser.abort();
    outcome
}

#[tokio::test]
async fn first_run_output_of_completed_child_is_progress() {
    // Given: delegate, then 1 + LIMIT run_output calls for the same finished child.
    let outputs = 1 + usize::try_from(LIMIT).unwrap();
    // When: only the repeated observations after the first are non-progress.
    let outcome = run_output_after_child_completed(outputs).await;
    // Then: the streak equals LIMIT and the run still finishes.
    outcome.assert_done();
}

#[tokio::test]
async fn repeated_run_output_of_completed_child_reaches_no_progress() {
    // Given: one more repeated run_output than the allowed streak.
    let outputs = 2 + usize::try_from(LIMIT).unwrap();
    // When: the same finished child is polled again and again.
    let outcome = run_output_after_child_completed(outputs).await;
    // Then: second and later observations are not progress.
    outcome.assert_no_progress();
}

#[tokio::test]
async fn run_output_after_counted_wait_is_not_progress() {
    // Given: the child completion was already received through wait.
    let mut script = vec![delegate(0, true), wait("wait", &child_id(0))];
    let repeats = usize::try_from(LIMIT).unwrap() + 1;
    script.extend((0..repeats).map(|index| run_output(index, &child_id(0))));
    script.push(finish());
    let (_model, runtime, bus) = harness(script, 1).await;
    // When: run_output re-reads the same child.
    let outcome = run_parent(&runtime, &bus).await;
    // Then: each child completion is counted once across wait and run_output.
    outcome.assert_no_progress();
}

#[tokio::test]
async fn repeated_list_agents_reaches_no_progress() {
    // Given: list_agents polling for more rounds than LIMIT.
    let repeats = usize::try_from(LIMIT).unwrap() + 1;
    let mut script: Vec<_> = (0..repeats)
        .map(|index| {
            Ok(tool_response(
                &format!("list-{index}"),
                "list_agents",
                json!({}),
            ))
        })
        .collect();
    script.push(finish());
    let (_model, runtime, bus) = harness(script, 0).await;
    // When/Then: inspection alone is not progress.
    run_parent(&runtime, &bus).await.assert_no_progress();
}

#[tokio::test]
async fn repeated_waits_on_an_already_observed_child_reach_no_progress() {
    // Given: a child whose completion the first wait already received.
    let repeats = usize::try_from(LIMIT).unwrap() + 1;
    let mut script = vec![delegate(0, true), wait("wait-first", &child_id(0))];
    script.extend((0..repeats).map(|index| wait(&format!("wait-{index}"), &child_id(0))));
    script.push(finish());
    let (_model, runtime, bus) = harness(script, 1).await;
    // When: later waits return at once with the same finished child.
    let outcome = run_parent(&runtime, &bus).await;
    // Then: re-observing a completion is not progress.
    outcome.assert_no_progress();
}

fn wait_any(call: &str, run_ids: &[String]) -> Result<ChatResponse, runtime::RuntimeError> {
    Ok(tool_response(
        call,
        "wait",
        json!({"run_ids": run_ids, "mode": "any"}),
    ))
}

#[tokio::test]
async fn any_waits_reobserving_the_same_completion_reach_no_progress() {
    // Given: child 0 finishes and is received once; child 1 never finishes.
    let ids = vec![child_id(0), child_id(1)];
    let repeats = usize::try_from(LIMIT).unwrap() + 1;
    let mut script = vec![
        delegate(0, true),
        delegate(1, true),
        wait("wait-first", &child_id(0)),
    ];
    script.extend((0..repeats).map(|index| wait_any(&format!("wait-any-{index}"), &ids)));
    script.push(finish());
    let (model, runtime, bus) = harness(script, 2).await;
    model.gate_key(&child(1), Arc::new(Notify::new())).await;
    // When: any-mode waits return at once, reporting only the received child.
    let outcome = run_parent(&runtime, &bus).await;
    // Then: re-observed completions are not progress.
    outcome.assert_no_progress();
    // The breaching round ends the run before its result reaches the model.
    for index in 0..repeats - 1 {
        let result = tool_result(&model, &format!("wait-any-{index}")).await;
        assert_eq!(
            result["completed_run_ids"],
            json!([child_id(0)]),
            "{result}"
        );
    }
}

/// Mirrors `budget_tracker::MAX_UNOBSERVED_SPAWN_CREDITS`: consecutive spawns
/// without a newly observed child completion that still count as progress.
const SPAWN_CREDITS: usize = 20;

fn background_only_script(delegates: usize) -> Vec<Result<ChatResponse, runtime::RuntimeError>> {
    let mut script: Vec<_> = (0..delegates).map(|index| delegate(index, true)).collect();
    script.push(finish());
    script
}

#[tokio::test]
async fn background_delegates_without_collecting_results_within_credits_finish_done() {
    // Given: more background delegate rounds than LIMIT, never waiting or reading output.
    let delegates = 2 * usize::try_from(LIMIT).unwrap() + 1;
    let (_model, runtime, bus) = harness(background_only_script(delegates), delegates).await;
    // When: the Orchestrator only spawns children.
    let outcome = run_parent(&runtime, &bus).await;
    // Then: each spawn within the credit limit is progress on its own.
    outcome.assert_done();
}

#[tokio::test]
async fn background_delegates_up_to_the_credit_boundary_finish_done() {
    // Given: every spawn credit is used, followed by exactly LIMIT uncredited spawns.
    let delegates = SPAWN_CREDITS + usize::try_from(LIMIT).unwrap();
    let (_model, runtime, bus) = harness(background_only_script(delegates), delegates).await;
    // When/Then: the no-progress streak equals LIMIT, so the run still finishes.
    run_parent(&runtime, &bus).await.assert_done();
}

#[tokio::test]
async fn endless_background_delegates_without_collecting_results_reach_no_progress() {
    // Given: distinct background delegates past the credit limit, with no wait or run_output.
    let delegates = SPAWN_CREDITS + usize::try_from(LIMIT).unwrap() + 1;
    let (_model, runtime, bus) = harness(background_only_script(delegates), delegates).await;
    // When: the Orchestrator keeps spawning without ever observing a completion.
    let outcome = run_parent(&runtime, &bus).await;
    // Then: spawns beyond the credits are not progress and the loop is stopped.
    outcome.assert_no_progress();
}

#[tokio::test]
async fn observed_completion_restores_spawn_credits() {
    // Given: all credits used, one observed completion, then a full set of credits again.
    let mut script: Vec<_> = (0..SPAWN_CREDITS)
        .map(|index| delegate(index, true))
        .collect();
    script.push(wait("wait-first", &child_id(0)));
    script.extend((SPAWN_CREDITS..2 * SPAWN_CREDITS).map(|index| delegate(index, true)));
    script.push(finish());
    let (_model, runtime, bus) = harness(script, 2 * SPAWN_CREDITS).await;
    // When/Then: the newly received completion resets the spawn credits.
    run_parent(&runtime, &bus).await.assert_done();
}

/// Finds a JSON tool result the parent's model received for `call`.
async fn tool_result(model: &ScriptedModel, call: &str) -> serde_json::Value {
    model
        .observed()
        .await
        .iter()
        .rev()
        .flat_map(|messages| messages.iter())
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            providers::ContentBlock::ToolResult {
                tool_call_id,
                content,
                ..
            } if tool_call_id == call => {
                let providers::ToolResultContent::Text { text } = content.first()? else {
                    return None;
                };
                serde_json::from_str(text).ok()
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("{call} result reached the model"))
}

fn interactive_delegate(index: usize) -> Result<ChatResponse, runtime::RuntimeError> {
    Ok(tool_response(
        &format!("delegate-{index}"),
        "delegate",
        json!({"target": {"role": "worker"}, "prompt": child(index), "background": true, "interactive": true}),
    ))
}

#[tokio::test]
async fn a_wait_that_delivers_a_message_is_progress() {
    // Given: an interactive child that messages the parent and then idles, and
    // a parent that inspects LIMIT more rounds after receiving the message.
    let mut script = vec![interactive_delegate(0), wait("wait-message", &child_id(0))];
    script.extend((0..LIMIT).map(|index| {
        Ok(tool_response(
            &format!("list-{index}"),
            "list_agents",
            json!({}),
        ))
    }));
    script.push(finish());
    let (model, runtime, bus) = harness(script, 0).await;
    model
        .add_keyed(
            &child(0),
            [
                Ok(tool_response(
                    "message",
                    "send",
                    json!({"run_id": "run-1", "message": "status update"}),
                )),
                Ok(text_response("idle", FinishReason::Stop)),
            ],
        )
        .await;
    // When: the wait returns with the message.
    let outcome = run_parent(&runtime, &bus).await;
    // Then: the message reset the no-progress streak, and arrived in the result.
    outcome.assert_done();
    let result = tool_result(&model, "wait-message").await;
    assert_eq!(
        result["messages"][0]["content"], "status update",
        "{result}"
    );
    assert_eq!(result["completed_run_ids"], json!([]), "{result}");
    runtime.cancel(child_id(0).parse().unwrap()).unwrap();
}

#[tokio::test]
async fn a_reported_question_does_not_end_the_next_wait() {
    // Given: a child that asks a required question and finishes once answered.
    let dir = tempfile::tempdir().unwrap();
    let storage_config = storage::StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let script = vec![
        delegate(0, true),
        wait("wait-question", &child_id(0)),
        wait("wait-answer", &child_id(0)),
        finish(),
    ];
    let model = Arc::new(ScriptedModel::new([]));
    model.add_keyed("PARENT", script).await;
    model
        .add_keyed(
            &child(0),
            [
                Ok(tool_response(
                    "ask",
                    "ask_user",
                    json!({"title": "Which scope?", "blocking": true}),
                )),
                Ok(text_response("answered", FinishReason::Stop)),
            ],
        )
        .await;
    let bus = Arc::new(EventBus::new(4096));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus.clone())),
        model.clone(),
    )
    .with_sequential_run_ids()
    .with_run_store(runtime::RunStore::open(&storage_config, storage.handle()).unwrap());
    let mut receiver = bus.subscribe();
    let mut phases = bus.subscribe();
    let parent = runtime.delegate_background(Role::Orchestrator, "PARENT".into(), config());
    // When: the second wait is suspended before the question is answered.
    let mut waits = 0;
    while waits < 2 {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            phases.recv().await.expect("event").kind
            && run_id == parent.to_string()
            && to == AgentRunPhase::Waiting
        {
            waits += 1;
        }
    }
    let child = child_id(0).parse::<RunId>().unwrap();
    let question = runtime.user_answers(child).unwrap().remove(0);
    runtime.answer_user_question(&question.id, "All").unwrap();
    let outcome = collect(&runtime, parent, &mut receiver).await;
    // Then: the first wait reported the question; the second, instead of
    // returning at once for it again, lasted until the child finished.
    outcome.assert_done();
    let first = tool_result(&model, "wait-question").await;
    assert_eq!(first["attention_run_ids"], json!([child_id(0)]), "{first}");
    let second = tool_result(&model, "wait-answer").await;
    assert_eq!(second["attention_run_ids"], json!([]), "{second}");
    assert_eq!(
        second["completed_run_ids"],
        json!([child_id(0)]),
        "{second}"
    );
}

#[tokio::test]
async fn non_terminal_run_output_reaches_no_progress() {
    // Given: a background child that never finishes and repeated run_output calls.
    let repeats = usize::try_from(LIMIT).unwrap() + 1;
    let mut script = vec![delegate(0, true)];
    script.extend((0..repeats).map(|index| run_output(index, &child_id(0))));
    script.push(finish());
    let (model, runtime, bus) = harness(script, 1).await;
    model.gate_key(&child(0), Arc::new(Notify::new())).await;
    // When: every snapshot is still running.
    let outcome = run_parent(&runtime, &bus).await;
    // Then: non-terminal output snapshots are not progress.
    outcome.assert_no_progress();
    // The breaching round ends the run before its result reaches the model.
    for index in 0..repeats - 1 {
        let result = tool_result(&model, &format!("output-{index}")).await;
        assert_eq!(result["status"], "still_running", "{result}");
    }
}
