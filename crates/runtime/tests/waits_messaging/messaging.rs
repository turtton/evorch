use crate::support;

use std::sync::Arc;

use agents::Role;
use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, EventBus, EventKind, LifecycleEvent,
};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig, RunId, RuntimeError};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tools::ToolExecutor;

use support::{ScriptedModel, collect_events, text_response};

async fn wait_for_phase(
    runtime: &AgentRuntime,
    bus: &EventBus,
    run_id: RunId,
    target: event_bus::AgentRunPhase,
) {
    let mut events = bus.subscribe();
    while runtime.inspect_agent(run_id).map(|info| info.phase) != Ok(target) {
        events.recv().await.expect("runtime phase event");
    }
}

fn runtime_with(model: impl Into<Arc<ScriptedModel>>) -> (AgentRuntime, Arc<EventBus>) {
    let model = model.into();
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
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
    wait_for_phase(&runtime, &_bus, parent, event_bus::AgentRunPhase::Running).await;

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
    runtime.cancel(child).ok();
    runtime.wait(child).await.unwrap();
}

#[tokio::test]
async fn consuming_a_reply_preserves_correlation_for_a_second_reply() {
    let (runtime, _bus) = runtime_with(ScriptedModel::gated([], Arc::new(Notify::new())));
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    let id = runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "ask", None)
        .unwrap();
    for content in ["first answer", "second answer"] {
        runtime
            .send_agent_message(
                child,
                parent,
                AgentMessageKind::Reply,
                content,
                Some(id.clone()),
            )
            .unwrap();
        let inbox = runtime.take_inbox(parent).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].kind, AgentMessageKind::Reply);
        assert_eq!(inbox[0].reply_to.as_deref(), Some(id.as_str()));
        assert_eq!(inbox[0].content, content);
        assert!(runtime.take_inbox(parent).unwrap().is_empty());
    }
    runtime.cancel(child).unwrap();
    runtime.cancel(parent).unwrap();
    runtime.wait(child).await.unwrap();
    runtime.wait(parent).await.unwrap();
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
    runtime.cancel(child).ok();
    runtime.wait(child).await.unwrap();
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
        runtime.send_agent_message(child1, parent, AgentMessageKind::Steering, "steer c1->p", None),
        Err(RuntimeError::MessageDenied { sender, recipient, detail })
        if sender == child1 && recipient == parent && detail.contains("steering")
    ));
    assert!(matches!(
        runtime.send_agent_message(child2, child1, AgentMessageKind::Steering, "steer c2->c1", None),
        Err(RuntimeError::MessageDenied { sender, recipient, detail })
        if sender == child2 && recipient == child1 && detail.contains("steering")
    ));
    assert!(matches!(
        runtime.send_agent_message(unrelated, child1, AgentMessageKind::Steering, "steer u->c1", None),
        Err(RuntimeError::MessageDenied { sender, recipient, detail })
        if sender == unrelated && recipient == child1 && detail.contains("steering")
    ));
    assert!(matches!(
        runtime.send_agent_message(parent, parent, AgentMessageKind::Steering, "steer self", None),
        Err(RuntimeError::MessageDenied { sender, recipient, .. })
        if sender == parent && recipient == parent
    ));
    // relation チェックが correlation 参照より先なので、無関係 run からの Reply は MessageDenied になる。
    assert!(matches!(
        runtime.send_agent_message(
            unrelated,
            parent,
            AgentMessageKind::Reply,
            "reply u->p",
            Some(sent_id.clone())
        ),
        Err(RuntimeError::MessageDenied { sender, recipient, .. })
        if sender == unrelated && recipient == parent
    ));
    // relation チェックが先に self を検出するため、同一 run への Reply も MessageDenied になる。
    assert!(matches!(
        runtime.send_agent_message(
            child1,
            child1,
            AgentMessageKind::Reply,
            "reply self",
            Some(sent_id.clone())
        ),
        Err(RuntimeError::MessageDenied { sender, recipient, .. })
        if sender == child1 && recipient == child1
    ));
    assert!(matches!(
        runtime.send_agent_message(
            child2,
            child1,
            AgentMessageKind::Reply,
            "wrong reply",
            Some(sent_id.clone())
        ),
        Err(RuntimeError::MessageDenied { sender, recipient, .. })
        if sender == child2 && recipient == child1
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
    // Steering は P→C1 のみ許可される（C1→P は上で拒否済み）。
    assert_eq!(
        runtime.send_agent_message(
            parent,
            child1,
            AgentMessageKind::Steering,
            "parent steer",
            None
        ),
        Ok("msg-3".to_string())
    );
    // Reply はもともとの送受信関係と逆方向のみ許可される。
    let reply_id = runtime
        .send_agent_message(
            child1,
            parent,
            AgentMessageKind::Reply,
            "ok",
            Some(sent_id.clone()),
        )
        .expect("C1→P の Reply は許可");
    assert_eq!(reply_id, "msg-4");
    // 別の有効な record (P→C2) に対して、関係は正しいが record の recipient が sender(C1) でないため UnknownMessage。
    let sent_b = runtime
        .send_agent_message(parent, child2, AgentMessageKind::Send, "p->c2", None)
        .expect("P→C2 は許可");
    assert_eq!(
        runtime.send_agent_message(
            child1,
            parent,
            AgentMessageKind::Reply,
            "mismatched correlation",
            Some(sent_b.clone())
        ),
        Err(RuntimeError::UnknownMessage {
            message_id: sent_b.clone()
        })
    );
    let _ = runtime.wait(parent).await;
    let _ = runtime.wait(unrelated).await;
}

#[tokio::test]
async fn restored_delivery_consumes_one_id_before_next_live_delivery() {
    // Given: 親は対話モードで入力待ち、子1 は即座に終了、子2 も対話モードで待機
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "child1",
        FinishReason::Stop,
    ))]));
    model
        .add_keyed("PARENT", [Ok(text_response("parent", FinishReason::Stop))])
        .await;
    model
        .add_keyed("CHILD2", [Ok(text_response("child2", FinishReason::Stop))])
        .await;
    let (runtime, _bus) = runtime_with(Arc::clone(&model));
    let directory = tempfile::tempdir().unwrap();
    let storage_config = storage::StorageConfig {
        db_path: directory.path().join("restore.sqlite3"),
        ..storage::StorageConfig::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let runtime =
        runtime.with_run_store(runtime::RunStore::open(&storage_config, storage.handle()).unwrap());
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, &_bus, parent, event_bus::AgentRunPhase::Waiting).await;
    let child1 = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD1".to_string(),
            RunConfig::default(),
        )
        .expect("子1 run を生成できる");
    runtime.wait(child1).await.expect("子1 が終端する");
    let child2 = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD2".to_string(),
            RunConfig {
                interactive: true,
                ..RunConfig::default()
            },
        )
        .expect("子2 run を生成できる");
    wait_for_phase(&runtime, &_bus, child2, event_bus::AgentRunPhase::Waiting).await;

    // When: restore the terminal child.
    let failed =
        runtime.send_agent_message(parent, child1, AgentMessageKind::Send, "too late", None);
    assert_eq!(failed, Ok("msg-2".to_string()));

    // Then: the next live delivery consumes the next ID.
    assert_eq!(
        runtime.send_agent_message(parent, child2, AgentMessageKind::Send, "ok", None),
        Ok("msg-3".to_string())
    );

    let _ = runtime.cancel(parent);
    runtime.cancel(parent).ok();
    runtime.wait(parent).await.unwrap();
    let _ = runtime.cancel(child2);
    runtime.cancel(child2).ok();
    runtime.wait(child2).await.unwrap();
}

#[tokio::test]
async fn send_to_terminal_recipient_without_store_returns_typed_error() {
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

    // Then: storage configuration is required, without a fresh-session fallback.
    assert_eq!(
        result,
        Err(RuntimeError::RunRestoreFailed {
            run_id: child.to_string(),
            reason: runtime::RunRestoreFailure::StorageNotConfigured,
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
    runtime.cancel(child).ok();
    runtime.wait(child).await.unwrap();
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
    wait_for_phase(&runtime, &bus, child, event_bus::AgentRunPhase::Running).await;
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
            | EventKind::Ledger(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_)
            | EventKind::Diagnostic(_)
            | EventKind::Ownership(_)
            | EventKind::ConversationPlan(_)
            | EventKind::PullRequest(_) => None,
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
    runtime.cancel(child).ok();
    runtime.wait(child).await.unwrap();
}
