mod support;

// allow: SIZE_OK — 6 個の T4 シナリオが共有する同期 helper を同一 integration suite に置く。

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentMessageKind, AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, FinishReason, Message, ToolResultContent};
use runtime::{AgentRuntime, RunConfig, RunId};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response};

fn runtime_with(model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
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
            }),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
    })
}

async fn wait_for_observed(model: &ScriptedModel, count: usize) -> Vec<Vec<Message>> {
    timeout(Duration::from_secs(2), async {
        loop {
            let observed = model.observed().await;
            if observed.len() >= count {
                return observed;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("model observation timeout")
}

async fn events_until_terminal(
    receiver: &mut event_bus::EventReceiver,
    run_id: RunId,
) -> Vec<Event> {
    let run_id = run_id.to_string();
    let mut events = Vec::new();
    loop {
        let event = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("event timeout")
            .expect("event receiver remains open");
        let terminal = matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id: event_run_id,
                to: AgentRunPhase::Done | AgentRunPhase::Error,
                ..
            }) if event_run_id == &run_id
        );
        events.push(event);
        if terminal {
            return events;
        }
    }
}

fn messages_for_marker(observed: &[Vec<Message>], marker: &str) -> Vec<Vec<Message>> {
    observed
        .iter()
        .filter(|messages| {
            messages.first().is_some_and(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == marker))
            })
        })
        .cloned()
        .collect()
}

fn contains_agent_message(messages: &[Message], header: &str, content: &str) -> bool {
    messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text.starts_with(header) && text.contains(content)))
    })
}

fn has_phase_transition(
    events: &[Event],
    run_id: &str,
    from: AgentRunPhase,
    to: AgentRunPhase,
) -> bool {
    events.iter().any(|event| {
        matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id: event_run_id,
                from: event_from,
                to: event_to,
                ..
            }) if event_run_id == run_id && *event_from == from && *event_to == to
        )
    })
}

/// gate 待ちのモデル要求が指定個数に達するのを待ってから gate を一斉に開く。
///
/// notify_waiters は待機中の全要求を同時に解放するため、待ち合わせ対象外の run
/// (子の no-op 応答など) は capability 拒否される `wait` 要求で消費を吸収し、
/// 再び gate で待ち戻る。
async fn step(model: &ScriptedModel, gate: &Notify, total_requests: usize) {
    wait_for_observed(model, total_requests).await;
    gate.notify_waiters();
}

#[tokio::test]
async fn send_message_meta_op_is_fire_and_forget_alias() {
    // Given: 子を生成して send_message → finish する Orchestrator と、
    //        gate で blocked され続け決して終端しない子 run
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "spawn-child",
                    "delegate_background",
                    json!({ "role": "worker", "prompt": "CHILD" }),
                )),
                Ok(tool_response(
                    "fire-and-forget",
                    "send_message",
                    json!({ "run_id": "run-2", "message": "先に進めてください" }),
                )),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({ "result": "done" }),
                )),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(tool_response(
                    "noop-1",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
                Ok(tool_response(
                    "noop-2",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
                Ok(tool_response(
                    "noop-3",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
            ],
        )
        .await;
    let (runtime, _bus) = runtime_with(Arc::clone(&model));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    // 親が run-1 なので子は決定論的に run-2 になる
    let child = RunId::new(2);

    // When: gate を段階的に開いて親を finish まで進める
    step(&model, &gate, 1).await;
    step(&model, &gate, 3).await;
    step(&model, &gate, 5).await;
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(parent)).await,
        Ok(Ok(AgentRunPhase::Done))
    );

    // Then: 親は子の終端を待たずに Done になり、send_message の結果は
    //       message_id を含む成功 ToolResult (旧実装なら子の終端待ちで block する)
    let observed = model.observed().await;
    let parent_turns = messages_for_marker(&observed, "PARENT");
    assert_eq!(
        tool_result(&parent_turns[2], "fire-and-forget"),
        Some(("msg-1".to_string(), false))
    );
    assert_eq!(
        runtime
            .inspect_agent(child)
            .expect("child run exists")
            .phase,
        AgentRunPhase::Running
    );

    // gate 待ちで残留する子をキャンセルして終端させる
    assert_eq!(runtime.cancel(child), Ok(()));
    let _ = timeout(Duration::from_secs(2), runtime.wait(child)).await;
}

#[tokio::test]
async fn send_meta_op_returns_message_id() {
    // Given: send meta-op で子へメッセージを送る Orchestrator
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "spawn-child",
                    "delegate_background",
                    json!({ "role": "worker", "prompt": "CHILD" }),
                )),
                Ok(tool_response(
                    "ping",
                    "send",
                    json!({ "run_id": "run-2", "message": "hello" }),
                )),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({ "result": "sent" }),
                )),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(tool_response(
                    "noop-1",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
                Ok(tool_response(
                    "noop-2",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
                Ok(tool_response(
                    "noop-3",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
            ],
        )
        .await;
    let (runtime, _bus) = runtime_with(Arc::clone(&model));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = RunId::new(2);

    // When: gate を段階的に開いて親を finish まで進める
    step(&model, &gate, 1).await;
    step(&model, &gate, 3).await;
    step(&model, &gate, 5).await;
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(parent)).await,
        Ok(Ok(AgentRunPhase::Done))
    );

    // Then: send の ToolResult は成功で決定論的な message_id "msg-1" を含む
    let observed = model.observed().await;
    let parent_turns = messages_for_marker(&observed, "PARENT");
    assert_eq!(
        tool_result(&parent_turns[2], "ping"),
        Some(("msg-1".to_string(), false))
    );

    // gate 待ちで残留する子をキャンセルして終端させる
    assert_eq!(runtime.cancel(child), Ok(()));
    let _ = timeout(Duration::from_secs(2), runtime.wait(child)).await;
}

#[tokio::test]
async fn wait_reply_meta_op_returns_reply_and_transitions_waiting() {
    // Given: 子へ質問を送り wait_reply で返信を待つ Orchestrator と、
    //        注入された [agent-message を受けて kind=reply で返信する Worker スクリプト
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "spawn-child",
                    "delegate_background",
                    json!({ "role": "worker", "prompt": "CHILD" }),
                )),
                Ok(tool_response(
                    "await-reply",
                    "wait_reply",
                    json!({ "message_id": "msg-1", "timeout_ms": 2000 }),
                )),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({ "result": "replied" }),
                )),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(tool_response(
                    "noop-1",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
                Ok(tool_response(
                    "answer",
                    "send",
                    json!({
                        "run_id": "run-1",
                        "message": "answer content",
                        "kind": "reply",
                        "reply_to": "msg-1"
                    }),
                )),
                Ok(text_response("child done", FinishReason::Stop)),
            ],
        )
        .await;
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let mut receiver = bus.subscribe();
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = RunId::new(2);

    // When: 子を生成した後、子の返信ターン解放前に質問を配送し、
    //       注入マーカーを確認してから返信を解放する
    step(&model, &gate, 1).await;
    wait_for_observed(&model, 3).await;
    assert_eq!(
        runtime.send_agent_message(
            parent,
            child,
            AgentMessageKind::Send,
            "question content",
            None,
        ),
        Ok("msg-1".to_string())
    );
    gate.notify_waiters();
    // 子の次リクエストに注入された agent-message マーカーが現れるまで待つ
    timeout(Duration::from_secs(2), async {
        loop {
            let observed = model.observed().await;
            let child_turns = messages_for_marker(&observed, "CHILD");
            if child_turns.len() >= 2
                && contains_agent_message(
                    &child_turns[1],
                    "[agent-message id=msg-1",
                    "question content",
                )
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("injected agent-message marker timeout");
    gate.notify_waiters();
    // 親の wait_reply が返信を受け取り、両者の次リクエストが観測されるまで待つ
    wait_for_observed(&model, 6).await;
    gate.notify_waiters();
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(parent)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(child)).await,
        Ok(Ok(AgentRunPhase::Done))
    );

    // Then: wait_reply の ToolResult は返信内容と返信メタデータを運び、
    //       ライフサイクルには呼び出し元の Waiting 遷移と Running 復帰が現れる
    let observed = model.observed().await;
    let parent_turns = messages_for_marker(&observed, "PARENT");
    let (reply_json, is_error) =
        tool_result(&parent_turns[2], "await-reply").expect("wait_reply result");
    assert!(!is_error);
    let reply: serde_json::Value = serde_json::from_str(&reply_json).expect("reply JSON");
    assert_eq!(reply["message_id"], json!("msg-2"));
    assert_eq!(reply["sender_run_id"], json!("run-2"));
    assert_eq!(reply["recipient_run_id"], json!("run-1"));
    assert_eq!(reply["kind"], json!("reply"));
    assert_eq!(reply["content"], json!("answer content"));
    assert_eq!(reply["reply_to"], json!("msg-1"));
    let events = events_until_terminal(&mut receiver, parent).await;
    assert!(has_phase_transition(
        &events,
        &parent.to_string(),
        AgentRunPhase::Running,
        AgentRunPhase::Waiting
    ));
    assert!(has_phase_transition(
        &events,
        &parent.to_string(),
        AgentRunPhase::Waiting,
        AgentRunPhase::Running
    ));
}

#[tokio::test]
async fn wait_reply_meta_op_times_out_with_error_result() {
    // Given: 返信せず gate で blocked され続ける子と、50ms の wait_reply を要求する親
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "spawn-child",
                    "delegate_background",
                    json!({ "role": "worker", "prompt": "CHILD" }),
                )),
                Ok(tool_response(
                    "ask",
                    "send",
                    json!({ "run_id": "run-2", "message": "question" }),
                )),
                Ok(tool_response(
                    "await-reply",
                    "wait_reply",
                    json!({ "message_id": "msg-1", "timeout_ms": 50 }),
                )),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({ "result": "gave up" }),
                )),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(tool_response(
                    "noop-1",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
                Ok(tool_response(
                    "noop-2",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
                Ok(tool_response(
                    "noop-3",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
                Ok(tool_response(
                    "noop-4",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
            ],
        )
        .await;
    let (runtime, _bus) = runtime_with(Arc::clone(&model));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = RunId::new(2);

    // When: gate を段階的に開いて wait_reply を実行させる
    step(&model, &gate, 1).await;
    step(&model, &gate, 3).await;
    step(&model, &gate, 5).await;
    step(&model, &gate, 7).await;
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(parent)).await,
        Ok(Ok(AgentRunPhase::Done))
    );

    // Then: wait_reply の ToolResult は timeout を示す error になり、親は Done まで復帰する
    let observed = model.observed().await;
    let parent_turns = messages_for_marker(&observed, "PARENT");
    let (text, is_error) = tool_result(&parent_turns[3], "await-reply").expect("wait_reply result");
    assert!(is_error);
    assert!(
        text.contains("タイムアウト") && text.contains("msg-1"),
        "timeout を示す error 本文が必要: {text}"
    );

    // gate 待ちで残留する子をキャンセルして終端させる
    assert_eq!(runtime.cancel(child), Ok(()));
    let _ = timeout(Duration::from_secs(2), runtime.wait(child)).await;
}

#[tokio::test]
async fn inbox_meta_op_returns_unread_once() {
    // Given: 何もしない親 run と、inbox を2回呼んでから完了する Worker 子 run
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response("noop-1", "list_agents", json!({}))),
                Ok(tool_response("noop-2", "list_agents", json!({}))),
                Ok(tool_response("noop-3", "list_agents", json!({}))),
                Ok(tool_response("noop-4", "list_agents", json!({}))),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(tool_response("inbox-1", "inbox", json!({}))),
                Ok(tool_response("inbox-2", "inbox", json!({}))),
                Ok(text_response("child done", FinishReason::Stop)),
            ],
        )
        .await;
    let (runtime, _bus) = runtime_with(Arc::clone(&model));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .expect("child run を生成できる");

    // When: 子の最初の inbox 要求が gate 待ちの間に親から2件配送し、
    //       その後 inbox の結果確認と完了を段階的に解放する
    wait_for_observed(&model, 2).await;
    assert_eq!(
        runtime.send_agent_message(parent, child, AgentMessageKind::Send, "first", None),
        Ok("msg-1".to_string())
    );
    assert_eq!(
        runtime.send_agent_message(parent, child, AgentMessageKind::Send, "second", None),
        Ok("msg-2".to_string())
    );
    gate.notify_waiters();
    step(&model, &gate, 4).await;
    step(&model, &gate, 6).await;
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(child)).await,
        Ok(Ok(AgentRunPhase::Done))
    );

    // Then: 1回目の inbox は FIFO 全件の JSON 配列、2回目は空配列になる
    let observed = model.observed().await;
    let child_turns = messages_for_marker(&observed, "CHILD");
    let (first, first_error) = tool_result(&child_turns[1], "inbox-1").expect("first inbox");
    assert!(!first_error);
    let messages: serde_json::Value = serde_json::from_str(&first).expect("inbox JSON");
    let array = messages.as_array().expect("JSON array");
    assert_eq!(array.len(), 2);
    assert_eq!(array[0]["message_id"], json!("msg-1"));
    assert_eq!(array[0]["sender_run_id"], json!("run-1"));
    assert_eq!(array[0]["recipient_run_id"], json!("run-2"));
    assert_eq!(array[0]["kind"], json!("send"));
    assert_eq!(array[0]["content"], json!("first"));
    assert!(array[0]["reply_to"].is_null());
    assert_eq!(array[1]["message_id"], json!("msg-2"));
    assert_eq!(array[1]["content"], json!("second"));
    let (second, second_error) = tool_result(&child_turns[2], "inbox-2").expect("second inbox");
    assert!(!second_error);
    assert_eq!(second, "[]");

    // gate 待ちで残留する親をキャンセルして終端させる
    assert_eq!(runtime.cancel(parent), Ok(()));
    let _ = timeout(Duration::from_secs(2), runtime.wait(parent)).await;
}

#[tokio::test]
async fn send_meta_op_rejects_unrelated_recipient() {
    // Given: 親子関係のない Explorer run と、それ宛てに send する Orchestrator
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "deny-send",
                    "send",
                    json!({ "run_id": "run-2", "message": "hello" }),
                )),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({ "result": "done" }),
                )),
            ],
        )
        .await;
    model
        .add_keyed(
            "UNRELATED",
            [
                Ok(tool_response(
                    "noop-1",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
                Ok(tool_response(
                    "noop-2",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
                Ok(tool_response(
                    "noop-3",
                    "wait",
                    json!({ "run_id": "run-1" }),
                )),
            ],
        )
        .await;
    let (runtime, _bus) = runtime_with(Arc::clone(&model));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let unrelated = runtime.delegate_background(
        Role::Explorer,
        "UNRELATED".to_string(),
        RunConfig::default(),
    );

    // When: gate を段階的に開いて send と finish を実行させる
    step(&model, &gate, 2).await;
    step(&model, &gate, 4).await;
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(parent)).await,
        Ok(Ok(AgentRunPhase::Done))
    );

    // Then: send は親子関係のない宛先として MessageDenied の error ToolResult になる
    let observed = model.observed().await;
    let parent_turns = messages_for_marker(&observed, "PARENT");
    let (text, is_error) = tool_result(&parent_turns[1], "deny-send").expect("send result");
    assert!(is_error);
    assert!(
        text.contains("拒否されました") && text.contains("run-1") && text.contains("run-2"),
        "MessageDenied の本文が必要: {text}"
    );

    // gate 待ちで残留する無関係 run をキャンセルして終端させる
    assert_eq!(runtime.cancel(unrelated), Ok(()));
    let _ = timeout(Duration::from_secs(2), runtime.wait(unrelated)).await;
}
