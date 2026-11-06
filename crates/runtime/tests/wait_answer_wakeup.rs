//! A run blocked in `wait_reply` or `wait` returns as soon as one of its own
//! questions is answered, instead of lingering until the reply, a target event
//! or a timeout. The answer text still reaches the model exactly once, through
//! the agent loop's own flush after the tool result.

mod support;

use std::sync::Arc;

use event_bus::{AgentMessageKind, AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, Message, ToolResultContent};
use runtime::{AgentRuntime, Role, RunConfig, RunId, RunStore};
use serde_json::{Value, json};
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response, tool_response, tool_responses};
use tokio::sync::Notify;
use tools::ToolExecutor;

struct Fixture {
    bus: Arc<EventBus>,
    runtime: AgentRuntime,
    model: Arc<ScriptedModel>,
    _storage: Storage,
    _dir: tempfile::TempDir,
}

fn fixture(model: Arc<ScriptedModel>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("questions.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus.clone())),
        model.clone(),
    )
    .with_sequential_run_ids()
    .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    Fixture {
        bus,
        runtime,
        model,
        _storage: storage,
        _dir: dir,
    }
}

fn tool_result(messages: &[Message], call_id: &str) -> Option<(String, bool)> {
    messages.iter().find_map(|message| {
        message.content.iter().find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == call_id => content.first().map(|item| match item {
                ToolResultContent::Text { text } => (text.clone(), *is_error),
                ToolResultContent::Image { .. } => panic!("unexpected image result"),
            }),
            _ => None,
        })
    })
}

fn occurrences(messages: &[Message], needle: &str) -> usize {
    serde_json::to_string(messages)
        .unwrap()
        .matches(needle)
        .count()
}

async fn until_waiting(fixture: &Fixture, run: RunId) {
    let mut events = fixture.bus.subscribe();
    while fixture.runtime.inspect_agent(run).unwrap().phase != AgentRunPhase::Waiting {
        events.recv().await.unwrap();
    }
}

fn tool_json(messages: &[Message], call_id: &str) -> Value {
    let (text, is_error) = tool_result(messages, call_id).expect("tool result present");
    assert!(!is_error, "{call_id} failed: {text}");
    serde_json::from_str(&text).expect("JSON tool result")
}

/// The parent (run-1) never answers on its own; the child (run-2) sends it a
/// message, asks a nonblocking question and waits for a reply to the message.
async fn wait_reply_scenario(answer_before_wait_starts: bool) {
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::new([]));
    model.gate_key("PARENT", Arc::new(Notify::new())).await;
    model.gate_key("CHILD", gate.clone()).await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(tool_response(
                    "ask-parent",
                    "send",
                    json!({"run_id": "run-1", "message": "need a decision"}),
                )),
                Ok(tool_response(
                    "question",
                    "ask_user",
                    json!({"title": "Which implementation?", "blocking": false}),
                )),
                Ok(tool_response(
                    "first-wait",
                    "wait_reply",
                    json!({"message_id": "msg-1", "timeout_ms": 600_000}),
                )),
                Ok(tool_response(
                    "second-wait",
                    "wait_reply",
                    json!({"message_id": "msg-1", "timeout_ms": 600_000}),
                )),
                Ok(text_response("child done", FinishReason::Stop)),
            ],
        )
        .await;
    let fixture = fixture(model);
    let parent = fixture.runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".into(),
        RunConfig::default(),
    );
    fixture.model.wait_for_request(0).await;
    let child = fixture
        .runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    // Requests 1 and 2 of the child run to completion; the third is the one
    // that issues the first wait_reply.
    for index in 1..=2 {
        fixture.model.wait_for_request(index).await;
        gate.notify_one();
    }
    fixture.model.wait_for_request(3).await;
    let question = fixture.runtime.user_answers(child).unwrap().remove(0);
    if answer_before_wait_starts {
        // The answer lands after the child's model call but before the tool
        // runs, so the loop has not injected it: the wait must not start idle.
        fixture
            .runtime
            .answer_subagent_question(parent, &question.id, "Use A")
            .await
            .unwrap();
        gate.notify_one();
    } else {
        gate.notify_one();
        until_waiting(&fixture, child).await;
        fixture
            .runtime
            .answer_subagent_question(parent, &question.id, "Use A")
            .await
            .unwrap();
    }

    // The answer ends the first wait_reply without a timeout error, and the
    // answer is injected once, after the tool result.
    let after_answer = fixture.model.wait_for_request(4).await;
    let first = tool_json(&after_answer, "first-wait");
    assert_eq!(first["status"], "answer_ready");
    assert_eq!(first["question_ids"], json!([question.id]));
    assert_eq!(first["message_id"], "msg-1");
    assert_eq!(occurrences(&after_answer, "[parent-answer id="), 1);
    assert!(!first.to_string().contains("Use A"));

    // The same message_id can be waited on again; this time it blocks until
    // the reply arrives, because the answer is already delivered.
    gate.notify_one();
    until_waiting(&fixture, child).await;
    fixture
        .runtime
        .send_agent_message(
            parent,
            child,
            AgentMessageKind::Reply,
            "late reply",
            Some("msg-1".to_string()),
        )
        .unwrap();
    let after_reply = fixture.model.wait_for_request(5).await;
    let second = tool_json(&after_reply, "second-wait");
    assert_eq!(second["kind"], "reply");
    assert_eq!(second["content"], "late reply");
    assert_eq!(second["reply_to"], "msg-1");
    assert_eq!(occurrences(&after_reply, "[parent-answer id="), 1);

    gate.notify_one();
    assert_eq!(
        fixture.runtime.wait(child).await.unwrap(),
        AgentRunPhase::Done
    );
    fixture.runtime.cancel(parent).unwrap();
    fixture.runtime.wait(parent).await.unwrap();
}

#[tokio::test]
async fn wait_reply_returns_when_its_own_question_is_answered_mid_wait() {
    wait_reply_scenario(false).await;
}

#[tokio::test]
async fn wait_reply_does_not_start_idle_when_an_answer_is_already_pending() {
    wait_reply_scenario(true).await;
}

#[tokio::test]
async fn wait_returns_when_its_own_question_is_answered() {
    let model = Arc::new(ScriptedModel::new([]));
    model.gate_key("CHILD", Arc::new(Notify::new())).await;
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "spawn",
                    "delegate",
                    json!({"target": {"role": "worker"}, "background": true, "prompt": "CHILD"}),
                )),
                Ok(tool_response(
                    "question",
                    "ask_user",
                    json!({"title": "Which implementation?", "blocking": false}),
                )),
                Ok(tool_response("observe", "wait", json!({"run_id": "run-2"}))),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({"result": "applied the answer"}),
                )),
            ],
        )
        .await;
    let fixture = fixture(model);
    let parent = fixture.runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".into(),
        RunConfig::default(),
    );
    until_waiting(&fixture, parent).await;
    let question = fixture.runtime.user_answers(parent).unwrap().remove(0);
    fixture
        .runtime
        .answer_user_question(&question.id, "Use A")
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(parent).await.unwrap(),
        AgentRunPhase::Done
    );

    let observed = fixture.model.observed().await;
    let parent_requests = observed
        .iter()
        .filter(|messages| {
            messages.first().is_some_and(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == "PARENT"))
            })
        })
        .collect::<Vec<_>>();
    let after_wait = parent_requests.last().expect("parent requests");
    let wait = tool_json(after_wait, "observe");
    assert_eq!(wait["answer_ready"], true);
    assert_eq!(wait["question_ids"], json!([question.id]));
    assert_eq!(wait["completed_run_ids"], json!([]));
    assert_eq!(occurrences(after_wait, "Use A"), 1);

    fixture.runtime.cancel(RunId::new(2)).unwrap();
    fixture.runtime.wait(RunId::new(2)).await.unwrap();
}

/// The child (run-2) sends run-1 a message and then waits for its reply with
/// the given script tail; every child model request waits on the returned gate.
async fn waiting_child(
    tail: Vec<Result<providers::ChatResponse, runtime::RuntimeError>>,
) -> (Fixture, Arc<Notify>, RunId, RunId) {
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::new([]));
    model.gate_key("PARENT", Arc::new(Notify::new())).await;
    model.gate_key("CHILD", gate.clone()).await;
    let mut script = vec![Ok(tool_response(
        "ask-parent",
        "send",
        json!({"run_id": "run-1", "message": "need a decision"}),
    ))];
    script.extend(tail);
    model.add_keyed("CHILD", script).await;
    let fixture = fixture(model);
    let parent = fixture.runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".into(),
        RunConfig::default(),
    );
    fixture.model.wait_for_request(0).await;
    let child = fixture
        .runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    fixture.model.wait_for_request(1).await;
    gate.notify_one();
    fixture.model.wait_for_request(2).await;
    gate.notify_one();
    until_waiting(&fixture, child).await;
    (fixture, gate, parent, child)
}

#[tokio::test]
async fn cancel_ends_wait_reply_without_waiting_for_the_timeout() {
    let (fixture, _gate, parent, child) = waiting_child(vec![Ok(tool_response(
        "wait",
        "wait_reply",
        json!({"message_id": "msg-1", "timeout_ms": 600_000}),
    ))])
    .await;
    fixture.runtime.cancel(child).unwrap();
    assert_eq!(
        fixture.runtime.wait(child).await.unwrap(),
        AgentRunPhase::Error
    );
    // The cancelled child made no further model request.
    assert_eq!(fixture.model.observed().await.len(), 3);
    fixture.runtime.cancel(parent).unwrap();
    fixture.runtime.wait(parent).await.unwrap();
}

#[tokio::test]
async fn host_input_ends_wait_reply_and_is_injected_once() {
    let (fixture, gate, parent, child) = waiting_child(vec![
        Ok(tool_response(
            "first-wait",
            "wait_reply",
            json!({"message_id": "msg-1", "timeout_ms": 600_000}),
        )),
        Ok(tool_response(
            "second-wait",
            "wait_reply",
            json!({"message_id": "msg-1", "timeout_ms": 600_000}),
        )),
        Ok(text_response("child done", FinishReason::Stop)),
    ])
    .await;
    fixture
        .runtime
        .send_message(child, "host says hi".into())
        .unwrap();
    let after_input = fixture.model.wait_for_request(3).await;
    let first = tool_json(&after_input, "first-wait");
    assert_eq!(first["status"], "user_input_ready");
    assert_eq!(first["message_id"], "msg-1");
    assert_eq!(occurrences(&after_input, "host says hi"), 1);

    // The message_id is still waitable and the input is not injected again.
    gate.notify_one();
    until_waiting(&fixture, child).await;
    fixture
        .runtime
        .send_agent_message(
            parent,
            child,
            AgentMessageKind::Reply,
            "late reply",
            Some("msg-1".to_string()),
        )
        .unwrap();
    let after_reply = fixture.model.wait_for_request(4).await;
    assert_eq!(
        tool_json(&after_reply, "second-wait")["content"],
        "late reply"
    );
    assert_eq!(occurrences(&after_reply, "host says hi"), 1);
    gate.notify_one();
    assert_eq!(
        fixture.runtime.wait(child).await.unwrap(),
        AgentRunPhase::Done
    );
    fixture.runtime.cancel(parent).unwrap();
    fixture.runtime.wait(parent).await.unwrap();
}

#[tokio::test]
async fn pending_host_input_does_not_hide_an_unknown_message_id_in_the_same_batch() {
    let (fixture, gate, parent, child) = waiting_child(vec![
        Ok(tool_responses([
            (
                "first-wait",
                "wait_reply",
                json!({"message_id": "msg-1", "timeout_ms": 600_000}),
            ),
            (
                "bad-wait",
                "wait_reply",
                json!({"message_id": "msg-999", "timeout_ms": 600_000}),
            ),
        ])),
        Ok(text_response("child done", FinishReason::Stop)),
    ])
    .await;
    fixture
        .runtime
        .send_message(child, "host says hi".into())
        .unwrap();
    let after_batch = fixture.model.wait_for_request(3).await;
    assert_eq!(
        tool_json(&after_batch, "first-wait")["status"],
        "user_input_ready"
    );
    let (text, is_error) = tool_result(&after_batch, "bad-wait").expect("second result");
    assert!(is_error, "{text}");
    assert!(text.contains("msg-999"), "{text}");
    assert_eq!(occurrences(&after_batch, "host says hi"), 1);
    gate.notify_one();
    assert_eq!(
        fixture.runtime.wait(child).await.unwrap(),
        AgentRunPhase::Done
    );
    fixture.runtime.cancel(parent).unwrap();
    fixture.runtime.wait(parent).await.unwrap();
}
