mod support;

// allow: SIZE_OK — AC7/AC9 の cross-crate scenarios が同じ bridge と deadline helpers を共有する。

use std::sync::Arc;

use event_bus::{AgentMessageKind, AgentRunPhase, DeliveryDisposition, EventBus, RecvError};
use providers::{ContentBlock, FinishReason, Role as MessageRole};
use runtime::{AgentRuntime, Role, RunConfig, RunId};
use sandbox::DirectSandbox;
use storage::{Database, Storage, StorageConfig, StorageHandle, StoredAgentMessage};
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

const TEST_SESSION: &str = "agent-messaging-persistence";

fn runtime_with(model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
}

fn storage_config(temp: &TempDir) -> StorageConfig {
    StorageConfig {
        db_path: temp.path().join("agent-messaging.db"),
        ..StorageConfig::default()
    }
}

fn spawn_storage_bridge(bus: &EventBus, handle: StorageHandle) -> JoinHandle<()> {
    let mut subscriber = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match subscriber.recv().await {
                Ok(event) => handle
                    .append_event(Some(TEST_SESSION), &event)
                    .expect("bridge はイベントを保存できる"),
                Err(RecvError::Lagged(skipped)) => {
                    panic!("storage bridge lagged by {skipped} events")
                }
                Err(RecvError::Closed) => return,
            }
        }
    })
}

async fn wait_for_phase(runtime: &AgentRuntime, run_id: RunId, phase: AgentRunPhase) {
    timeout(Duration::from_secs(2), async {
        loop {
            if runtime
                .inspect_agent(run_id)
                .expect("run は検査可能なまま")
                .phase
                == phase
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("phase transition timeout");
}

async fn wait_for_messages(config: &StorageConfig, count: usize) -> Vec<StoredAgentMessage> {
    timeout(Duration::from_secs(2), async {
        loop {
            let messages = Database::open(config)
                .expect("reader を開ける")
                .agent_messages_by_session(TEST_SESSION)
                .expect("AgentMessage transcript を読める");
            if messages.len() >= count {
                return messages;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("storage bridge drain timeout")
}

async fn stop_bridge(bridge: JoinHandle<()>) {
    bridge.abort();
    assert!(matches!(bridge.await, Err(error) if error.is_cancelled()));
}

#[tokio::test]
async fn agent_message_deliveries_persist_via_bus_bridge_and_restore() {
    // Given: 購読済み storage bridge と、モデル応答ゲートで Running を維持する親子 run
    let temp = TempDir::new().expect("一時ディレクトリを作れる");
    let config = storage_config(&temp);
    let storage = Storage::open(config.clone()).expect("storage を開ける");
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    let (runtime, bus) = runtime_with(model);
    let bridge = spawn_storage_bridge(&bus, storage.handle());
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .expect("子 run を生成できる");
    wait_for_phase(&runtime, parent, AgentRunPhase::Running).await;
    wait_for_phase(&runtime, child, AgentRunPhase::Running).await;

    // When: send、steering、相関 reply を決定順で Event Bus へ配送する
    let first_id = runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "調査して", None)
        .expect("send を配送できる");
    assert_eq!(first_id, "msg-1".to_string());
    assert_eq!(
        runtime.send_agent_message(parent, child, AgentMessageKind::Steering, "追加条件", None,),
        Ok("msg-2".to_string())
    );
    assert_eq!(
        runtime.send_agent_message(
            child,
            parent,
            AgentMessageKind::Reply,
            "結果",
            Some(first_id.clone()),
        ),
        Ok("msg-3".to_string())
    );
    let restored = wait_for_messages(&config, 3).await;

    // Then: 既存 storage ingress が配送順・宛先・相関・disposition のみを復元する
    assert_eq!(restored.len(), 3);
    assert_eq!(restored[0].message.message_id, "msg-1".to_string());
    assert_eq!(restored[0].message.sender_run_id, parent.to_string());
    assert_eq!(restored[0].message.recipient_run_id, child.to_string());
    assert_eq!(restored[0].message.kind, AgentMessageKind::Send);
    assert_eq!(restored[0].message.reply_to, None);
    assert_eq!(restored[0].disposition, DeliveryDisposition::Steering);
    assert_eq!(restored[1].message.message_id, "msg-2".to_string());
    assert_eq!(restored[1].message.sender_run_id, parent.to_string());
    assert_eq!(restored[1].message.recipient_run_id, child.to_string());
    assert_eq!(restored[1].message.kind, AgentMessageKind::Steering);
    assert_eq!(restored[1].message.reply_to, None);
    assert_eq!(restored[1].disposition, DeliveryDisposition::Steering);
    assert_eq!(restored[2].message.message_id, "msg-3".to_string());
    assert_eq!(restored[2].message.sender_run_id, child.to_string());
    assert_eq!(restored[2].message.recipient_run_id, parent.to_string());
    assert_eq!(restored[2].message.kind, AgentMessageKind::Reply);
    assert_eq!(restored[2].message.reply_to, Some(first_id));
    assert_eq!(restored[2].disposition, DeliveryDisposition::Aside);

    assert_eq!(runtime.cancel(parent), Ok(()));
    assert_eq!(runtime.cancel(child), Ok(()));
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(parent)).await,
        Ok(Ok(AgentRunPhase::Error))
    );
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(child)).await,
        Ok(Ok(AgentRunPhase::Error))
    );
    stop_bridge(bridge).await;
    storage.close();
}

#[tokio::test]
async fn parent_recovers_child_crash_by_new_run_with_reconstructed_context() {
    // Given: 永続化 bridge と、返信済み transcript を持つ Running の親子 run
    let temp = TempDir::new().expect("一時ディレクトリを作れる");
    let config = storage_config(&temp);
    let storage = Storage::open(config.clone()).expect("storage を開ける");
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let bridge = spawn_storage_bridge(&bus, storage.handle());
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT-RECOVERY".to_string(),
        RunConfig::default(),
    );
    let old_child = runtime
        .delegate_background_as_child(parent, Role::Worker, "OLD-CHILD", RunConfig::default())
        .expect("旧 child run を生成できる");
    wait_for_phase(&runtime, parent, AgentRunPhase::Running).await;
    wait_for_phase(&runtime, old_child, AgentRunPhase::Running).await;
    let first_id = runtime
        .send_agent_message(parent, old_child, AgentMessageKind::Send, "調査して", None)
        .expect("依頼を配送できる");
    assert_eq!(first_id, "msg-1".to_string());
    assert_eq!(
        runtime.send_agent_message(
            old_child,
            parent,
            AgentMessageKind::Reply,
            "結果マーカー-XYZ",
            Some(first_id),
        ),
        Ok("msg-2".to_string())
    );
    let persisted = wait_for_messages(&config, 2).await;

    // When: 旧 child を crash 相当で終端し、同じ DB から文脈を再構成して新 run を開始する
    assert_eq!(runtime.cancel(old_child), Ok(()));
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(old_child)).await,
        Ok(Ok(AgentRunPhase::Error))
    );
    assert_eq!(
        runtime
            .inspect_agent(old_child)
            .expect("旧 child は検査可能")
            .phase,
        AgentRunPhase::Error
    );
    stop_bridge(bridge).await;
    storage.close();
    let restored = Database::open(&config)
        .expect("crash 後に同じ DB を開ける")
        .agent_messages_by_session(TEST_SESSION)
        .expect("crash 後に transcript を読める");
    assert_eq!(restored, persisted);
    assert_eq!(restored.len(), 2);
    let transcript = restored
        .iter()
        .map(|record| {
            format!(
                "{} -> {}: {}",
                record.message.sender_run_id,
                record.message.recipient_run_id,
                record.message.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let reconstructed_prompt = format!("これまでの会話:\n{transcript}");
    model
        .add_keyed(
            &reconstructed_prompt,
            [Ok(text_response("復旧完了", FinishReason::Stop))],
        )
        .await;
    let observed_before = model.observed().await.len();
    let new_child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            reconstructed_prompt.clone(),
            RunConfig::default(),
        )
        .expect("再構成文脈で新 child run を生成できる");

    // Then: 新 RunId のモデル入力に復元結果が入り、旧 run を revive せず Done になる
    assert_ne!(new_child, old_child);
    assert_eq!(
        runtime
            .inspect_agent(old_child)
            .expect("旧 child は残る")
            .phase,
        AgentRunPhase::Error
    );
    timeout(Duration::from_secs(2), async {
        loop {
            if model.observed().await.len() > observed_before {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("新 run のモデル入力 timeout");
    let observed = model.observed().await;
    let new_run_messages = observed
        .iter()
        .find(|messages| {
            messages.first().is_some_and(|message| {
                message.content.first()
                    == Some(&ContentBlock::Text {
                        text: reconstructed_prompt.clone(),
                    })
            })
        })
        .expect("再構成 prompt のモデル入力がある");
    assert_eq!(new_run_messages[0].role, MessageRole::User);
    assert!(matches!(
        &new_run_messages[0].content[0],
        ContentBlock::Text { text } if text.contains("結果マーカー-XYZ")
    ));
    gate.notify_waiters();
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(new_child)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    assert_eq!(runtime.cancel(parent), Ok(()));
    assert_eq!(
        timeout(Duration::from_secs(2), runtime.wait(parent)).await,
        Ok(Ok(AgentRunPhase::Error))
    );
}
