mod support;

use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, Message};
use runtime::{AgentRuntime, Role, RunConfig, RunStore};
use std::sync::Arc;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response};
use tools::ToolExecutor;

fn runtime(model: Arc<ScriptedModel>, config: &StorageConfig, storage: &Storage) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(128));
    AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
        .with_run_store(RunStore::open(config, storage.handle()).unwrap())
}

fn user(text: &str) -> Message {
    Message {
        role: providers::Role::User,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

#[tokio::test]
async fn latest_terminal_chat_preserves_checkpoints_and_ignores_other_threads() {
    // Given: two terminal snapshots for one chat plus a newer unrelated chat.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("chat.sqlite3"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let database = storage::Database::open(&config).unwrap();
    let model = Arc::new(ScriptedModel::new(
        (0..4).map(|_| Ok(text_response("answer", FinishReason::Stop))),
    ));
    let runtime = runtime(model.clone(), &config, &storage);
    let mut selected = None;
    for (index, name) in ["chat:thread", "chat:thread", "chat:other"]
        .into_iter()
        .enumerate()
    {
        let run = runtime.delegate_background(
            Role::Worker,
            format!("turn-{index}"),
            RunConfig {
                name: Some(name.into()),
                ..RunConfig::default()
            },
        );
        runtime.wait(run).await.unwrap();
        if index == 1 {
            selected = Some(run);
        }
    }
    let mut record = database
        .run_context(&selected.unwrap().to_string())
        .unwrap()
        .unwrap();
    let checkpoint = runtime::CompactionCheckpoint {
        id: "checkpoint-1".into(),
        summary: user("summary"),
        range: (0, 2),
    };
    record.checkpoints_json = serde_json::to_string(&vec![checkpoint.clone()]).unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    // When: the selected thread receives another message.
    let run = runtime
        .delegate_chat("thread", "followup".into(), RunConfig::default())
        .unwrap();
    runtime.wait(run).await.unwrap();
    // Then: the newest matching snapshot's compacted window and raw checkpoint survive.
    assert_eq!(
        model.observed().await[3],
        vec![user("summary"), user("followup")]
    );
    let saved = database.run_context(&run.to_string()).unwrap().unwrap();
    let checkpoints: Vec<runtime::CompactionCheckpoint> =
        serde_json::from_str(&saved.checkpoints_json).unwrap();
    assert_eq!(checkpoints, vec![checkpoint]);
    let messages: Vec<Message> = serde_json::from_str(&saved.messages_json).unwrap();
    assert_eq!(messages[0], user("turn-1"));
}

#[tokio::test]
async fn followup_request_includes_prior_terminal_turn_when_original_run_gone() {
    // Given: a terminal chat snapshot, with no original runtime left in memory.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("chat.sqlite3"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let answer = text_response("turn-1-answer", FinishReason::Stop);
    let model = Arc::new(ScriptedModel::new([
        Ok(answer.clone()),
        Ok(text_response("turn-2-answer", FinishReason::Stop)),
    ]));
    let first = runtime(model.clone(), &config, &storage);
    let run = first.delegate_background(
        Role::Worker,
        "turn-1".into(),
        RunConfig {
            name: Some("chat:thread".into()),
            ..RunConfig::default()
        },
    );
    assert_eq!(first.wait(run).await.unwrap(), AgentRunPhase::Done);
    drop(first);
    let next = runtime(model.clone(), &config, &storage);
    // When: a follow-up is submitted through the chat continuation entry point.
    let run = next
        .delegate_chat("thread", "turn-2".into(), RunConfig::default())
        .unwrap();
    assert_eq!(next.wait(run).await.unwrap(), AgentRunPhase::Done);
    // Then: the provider sees both prior turns before the new user message.
    let observed = model.observed().await;
    assert_eq!(
        observed[1],
        vec![user("turn-1"), answer.message, user("turn-2")]
    );
}

#[tokio::test]
async fn followup_without_prior_terminal_snapshot_behaves_as_new_run() {
    // Given: an empty run store.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("chat.sqlite3"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "answer",
        FinishReason::Stop,
    ))]));
    let runtime = runtime(model.clone(), &config, &storage);
    // When: the first chat is submitted.
    let run = runtime
        .delegate_chat("thread", "first".into(), RunConfig::default())
        .unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    // Then: only the current user message reaches the provider.
    assert_eq!(model.observed().await, vec![vec![user("first")]]);
}
