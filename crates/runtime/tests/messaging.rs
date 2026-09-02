mod support;

use std::sync::Arc;
use std::time::Duration;

use agents::Role;
use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, EventBus, EventKind, LifecycleEvent,
};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig, RunId, RuntimeError};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tokio::time::{Duration as TokioDuration, Instant as TokioInstant, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, collect_events, text_response};

async fn wait_for_phase(runtime: &AgentRuntime, run_id: RunId, target: event_bus::AgentRunPhase) {
    let deadline = TokioInstant::now() + TokioDuration::from_secs(3);
    while runtime.inspect_agent(run_id).map(|info| info.phase) != Ok(target) {
        if TokioInstant::now() > deadline {
            panic!(
                "run {run_id} did not reach phase {target:?} within 3s (current {:?})",
                runtime.inspect_agent(run_id)
            );
        }
        tokio::time::sleep(TokioDuration::from_millis(5)).await;
    }
}

fn runtime_with(model: ScriptedModel) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (
        AgentRuntime::new(Arc::clone(&bus), executor, Arc::new(model)),
        bus,
    )
}

#[tokio::test]
async fn send_returns_immediately_while_recipient_running() {
    // Given: 子 run がモデル応答ゲートで Running のままになる親子関係
    let gate = Arc::new(Notify::new());
    let (runtime, _bus) = runtime_with(ScriptedModel::gated(
        [Ok(text_response("done", FinishReason::Stop))],
        Arc::clone(&gate),
    ));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD".to_string(),
            RunConfig::default(),
        )
        .expect("子 run を生成できる");
    wait_for_phase(&runtime, parent, event_bus::AgentRunPhase::Running).await;

    // When: 親から子へメッセージを送る
    let result = runtime.send_agent_message(
        parent,
        child,
        AgentMessageKind::Send,
        "hello while running",
        None,
    );

    // Then: 即座に message_id が返り、子は Running のまま
    assert_eq!(result, Ok("msg-1".to_string()));
    assert_eq!(
        runtime.inspect_agent(child).expect("子 exists").phase,
        event_bus::AgentRunPhase::Running
    );
    gate.notify_waiters();
    let _ = tokio::time::timeout(TokioDuration::from_secs(2), runtime.wait(child)).await;
}

#[tokio::test]
async fn wait_reply_returns_matching_reply_and_leaves_unrelated_unread() {
    // Given: 親子 run と、子から親への返信＋無関係 Send イベント
    let (runtime, _bus) = runtime_with(ScriptedModel::gated(
        [Ok(text_response("done", FinishReason::Stop))],
        Arc::new(Notify::new()),
    ));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD".to_string(),
            RunConfig::default(),
        )
        .expect("子 run を生成できる");
    wait_for_phase(&runtime, parent, event_bus::AgentRunPhase::Running).await;
    wait_for_phase(&runtime, child, event_bus::AgentRunPhase::Running).await;
    let id = runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "ask", None)
        .expect("送信成功");
    runtime
        .send_agent_message(
            child,
            parent,
            AgentMessageKind::Reply,
            "answer",
            Some(id.clone()),
        )
        .expect("返信成功");
    runtime
        .send_agent_message(child, parent, AgentMessageKind::Send, "unrelated", None)
        .expect("無関係メッセージ成功");

    // When: 親で待ち受ける
    let reply = timeout(
        TokioDuration::from_millis(500),
        runtime.wait_reply(parent, &id, Duration::from_millis(400)),
    )
    .await
    .expect("待機がタイムアウトしない")
    .expect("返信を受け取る");

    // Then: 一致する返信だけが返り、無関係な Send は inbox に残る
    assert_eq!(reply.content, "answer".to_string());
    assert_eq!(reply.kind, AgentMessageKind::Reply);
    assert_eq!(reply.reply_to, Some(id));
    let inbox = runtime.take_inbox(parent).expect("inbox を取得できる");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].content, "unrelated".to_string());
    assert_eq!(inbox[0].kind, AgentMessageKind::Send);
}

#[tokio::test]
async fn wait_reply_times_out_with_typed_error() {
    // Given: 親子 run を作り、子は即座に終端しないゲート付き
    let gate = Arc::new(Notify::new());
    let (runtime, _bus) = runtime_with(ScriptedModel::gated(
        [Ok(text_response("done", FinishReason::Stop))],
        Arc::clone(&gate),
    ));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD".to_string(),
            RunConfig::default(),
        )
        .expect("子 run を生成できる");
    let id = runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "ask", None)
        .expect("送信成功");

    // When: 返信なしで 50ms 待つ
    let start = TokioInstant::now();
    let result = runtime
        .wait_reply(parent, &id, Duration::from_millis(50))
        .await;
    let elapsed = start.elapsed();

    // Then: ReplyTimeout 型付きエラーで迅速に返る（100ms 未満）
    assert!(elapsed < TokioDuration::from_millis(100));
    assert_eq!(
        result,
        Err(RuntimeError::ReplyTimeout {
            message_id: id.clone(),
        })
    );
    gate.notify_waiters();
    let _ = tokio::time::timeout(TokioDuration::from_secs(2), runtime.wait(child)).await;
}

#[tokio::test]
async fn wait_reply_returns_run_terminated_when_recipient_finishes_without_reply() {
    // Given: 子 run が即座に終了するスクリプト
    let (runtime, _bus) = runtime_with(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD".to_string(),
            RunConfig::default(),
        )
        .expect("子 run を生成できる");
    let id = runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "ask", None)
        .expect("送信成功");

    // When: 子が終端してから返信を待つ
    runtime.wait(child).await.expect("子が終端する");
    let result = runtime
        .wait_reply(parent, &id, Duration::from_millis(100))
        .await;

    // Then: 相手が終端しているため RunTerminated
    assert_eq!(
        result,
        Err(RuntimeError::RunTerminated {
            run_id: child.to_string(),
        })
    );
}

#[tokio::test]
async fn take_inbox_returns_delivery_order_and_never_rereturns() {
    // Given: 親子 run で子はゲート付き Running 維持
    let gate = Arc::new(Notify::new());
    let (runtime, _bus) = runtime_with(ScriptedModel::gated(
        [Ok(text_response("done", FinishReason::Stop))],
        Arc::clone(&gate),
    ));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD".to_string(),
            RunConfig::default(),
        )
        .expect("子 run を生成できる");
    for content in ["first", "second", "third"] {
        runtime
            .send_agent_message(parent, child, AgentMessageKind::Send, content, None)
            .expect("送信成功");
    }

    // When: 子 inbox を二回取得する
    let first = runtime.take_inbox(child).expect("inbox を取得できる");
    let second = runtime.take_inbox(child).expect("inbox を取得できる");

    // Then: 一回目は FIFO 全件、二回目は空
    assert_eq!(first.len(), 3);
    assert_eq!(first[0].content, "first".to_string());
    assert_eq!(first[1].content, "second".to_string());
    assert_eq!(first[2].content, "third".to_string());
    assert!(second.is_empty());

    gate.notify_waiters();
    let _ = tokio::time::timeout(TokioDuration::from_secs(2), runtime.wait(child)).await;
}

#[tokio::test]
async fn addressing_matrix_rejects_non_parent_child_and_self() {
    // Given: 根 P、子 C1/C2、無関係 U
    let (runtime, _bus) = runtime_with(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let parent =
        runtime.delegate_background(Role::Orchestrator, "P".to_string(), RunConfig::default());
    let child1 = runtime
        .delegate_background_as_child(parent, Role::Worker, "C1".to_string(), RunConfig::default())
        .expect("子 run 生成");
    let child2 = runtime
        .delegate_background_as_child(parent, Role::Worker, "C2".to_string(), RunConfig::default())
        .expect("子 run 生成");
    let unrelated =
        runtime.delegate_background(Role::Explorer, "U".to_string(), RunConfig::default());
    let missing = RunId::new(999);
    let sent_id = runtime
        .send_agent_message(parent, child1, AgentMessageKind::Send, "p->c1", None)
        .expect("P→C1 は許可");

    // When/Then: 許可・拒否行列を検証
    assert_eq!(
        runtime.send_agent_message(child1, parent, AgentMessageKind::Send, "c1->p", None),
        Ok("msg-2".to_string())
    );
    assert!(matches!(
        runtime.send_agent_message(child1, child2, AgentMessageKind::Send, "sibling", None),
        Err(RuntimeError::MessageDenied { sender, recipient, .. })
        if sender == child1 && recipient == child2
    ));
    assert!(matches!(
        runtime.send_agent_message(unrelated, child1, AgentMessageKind::Send, "unrelated", None),
        Err(RuntimeError::MessageDenied { sender, recipient, .. })
        if sender == unrelated && recipient == child1
    ));
    assert!(matches!(
        runtime.send_agent_message(parent, parent, AgentMessageKind::Send, "self", None),
        Err(RuntimeError::MessageDenied { sender, recipient, .. })
        if sender == parent && recipient == parent
    ));
    assert_eq!(
        runtime.send_agent_message(parent, missing, AgentMessageKind::Send, "unknown", None),
        Err(RuntimeError::UnknownRun {
            run_id: missing.to_string()
        })
    );
    assert!(matches!(
        runtime.send_agent_message(child1, parent, AgentMessageKind::Steering, "steer", None),
        Err(RuntimeError::MessageDenied { sender, recipient, .. })
        if sender == child1 && recipient == parent
    ));
    assert!(matches!(
        runtime.send_agent_message(
            child2,
            child1,
            AgentMessageKind::Reply,
            "wrong reply",
            Some(sent_id.clone())
        ),
        Err(RuntimeError::UnknownMessage { message_id }) if message_id == sent_id
    ));
    assert!(matches!(
        runtime.send_agent_message(
            child1,
            parent,
            AgentMessageKind::Reply,
            "no reply_to",
            None
        ),
        Err(RuntimeError::MessageDenied { sender, recipient, .. })
        if sender == child1 && recipient == parent
    ));
    assert!(matches!(
        runtime.send_agent_message(
            parent,
            child1,
            AgentMessageKind::Reply,
            "unknown reply_to",
            Some("msg-missing".to_string())
        ),
        Err(RuntimeError::UnknownMessage { message_id }) if message_id == "msg-missing"
    ));
    let _ = runtime.wait(parent).await;
    let _ = runtime.wait(unrelated).await;
}

#[tokio::test]
async fn send_to_terminal_recipient_returns_run_terminated() {
    // Given: 即座に終了する子 run
    let (runtime, _bus) = runtime_with(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD".to_string(),
            RunConfig::default(),
        )
        .expect("子 run を生成できる");
    runtime.wait(child).await.expect("子が終端する");

    // When: 終端後に送る
    let result =
        runtime.send_agent_message(parent, child, AgentMessageKind::Send, "too late", None);

    // Then: RunTerminated
    assert_eq!(
        result,
        Err(RuntimeError::RunTerminated {
            run_id: child.to_string()
        })
    );
    let _ = runtime.wait(parent).await;
}

#[tokio::test]
async fn send_fails_when_mailbox_full() {
    // Given: 子 run をゲート付きで Running 維持、mailboxes を未ドレイン
    let gate = Arc::new(Notify::new());
    let (runtime, _bus) = runtime_with(ScriptedModel::gated(
        [Ok(text_response("done", FinishReason::Stop))],
        Arc::clone(&gate),
    ));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD".to_string(),
            RunConfig::default(),
        )
        .expect("子 run を生成できる");

    // When: 子 mailbox 容量（64 件）を超えるまで送る
    for i in 0..64 {
        assert_eq!(
            runtime.send_agent_message(
                parent,
                child,
                AgentMessageKind::Send,
                format!("fill-{i}"),
                None
            ),
            Ok(format!("msg-{}", i + 1))
        );
    }
    let result =
        runtime.send_agent_message(parent, child, AgentMessageKind::Send, "overflow", None);

    // Then: 65 件目は MailboxFull
    assert_eq!(
        result,
        Err(RuntimeError::MailboxFull {
            run_id: child.to_string()
        })
    );
    gate.notify_waiters();
    let _ = tokio::time::timeout(TokioDuration::from_secs(2), runtime.wait(child)).await;
}

#[tokio::test]
async fn message_delivery_emits_agent_message_event_and_no_lifecycle_completion() {
    // Given: 子 run をゲート付きで Running 維持、購読者を確保
    let gate = Arc::new(Notify::new());
    let (runtime, bus) = runtime_with(ScriptedModel::gated(
        [Ok(text_response("done", FinishReason::Stop))],
        Arc::clone(&gate),
    ));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD".to_string(),
            RunConfig::default(),
        )
        .expect("子 run を生成できる");
    wait_for_phase(&runtime, child, event_bus::AgentRunPhase::Running).await;
    let mut receiver = bus.subscribe();

    // When: send + reply を行う（子は Running のまま）
    let id = runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "ping", None)
        .expect("送信成功");
    runtime
        .send_agent_message(
            child,
            parent,
            AgentMessageKind::Reply,
            "pong",
            Some(id.clone()),
        )
        .expect("返信成功");

    // Then: AgentMessage Delivered イベントが最後の購読位置以降に現れる
    let events = collect_events(&mut receiver, 2).await;
    let delivered: Vec<&AgentMessage> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) => Some(message),
            EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_) => None,
        })
        .collect();
    assert_eq!(delivered.len(), 2);
    assert_eq!(delivered[0].message_id, "msg-1");
    assert_eq!(delivered[0].sender_run_id, parent.to_string());
    assert_eq!(delivered[0].recipient_run_id, child.to_string());
    assert_eq!(delivered[0].kind, AgentMessageKind::Send);
    assert_eq!(delivered[0].content, "ping".to_string());
    assert_eq!(delivered[0].reply_to, None);
    assert_eq!(delivered[1].message_id, "msg-2");
    assert_eq!(delivered[1].sender_run_id, child.to_string());
    assert_eq!(delivered[1].recipient_run_id, parent.to_string());
    assert_eq!(delivered[1].kind, AgentMessageKind::Reply);
    assert_eq!(delivered[1].content, "pong".to_string());
    assert_eq!(delivered[1].reply_to, Some(id));
    // 子の BackgroundTaskCompleted / Cancelled はこの exchange では発生しない
    let has_terminal = events.iter().any(|event| {
        matches!(
            &event.kind,
            EventKind::Lifecycle(
                LifecycleEvent::BackgroundTaskCompleted { task_id }
                | LifecycleEvent::BackgroundTaskCancelled { task_id }
            ) if task_id == &child.to_string()
        )
    });
    assert!(!has_terminal);

    gate.notify_waiters();
    let _ = tokio::time::timeout(TokioDuration::from_secs(2), runtime.wait(child)).await;
}
