//! 復元した run が公式 compaction の blob を読めないモデルへ移るとき、元のメッセージから
//! 要約し直してから送る (ADR 0030)。

mod support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::{EventBus, RequestPurpose};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, CompactionCheckpoint, Role, RunConfig,
    RunStore, RuntimeError,
};
use storage::{Storage, StorageConfig};
use support::text_response;
use tools::ToolExecutor;

/// The router's view after a restore: the pinned member cannot replay compaction blobs.
struct TextOnlyModel {
    summary: Result<ChatResponse, RuntimeError>,
    observed: Mutex<Vec<(RequestPurpose, Vec<Message>)>>,
}

#[async_trait]
impl AgentModel for TextOnlyModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.observed
            .lock()
            .unwrap()
            .push((invocation.purpose, messages.to_vec()));
        if invocation.purpose == RequestPurpose::Compaction {
            return self.summary.clone();
        }
        if messages.iter().any(carries_blob) {
            return Err(RuntimeError::ModelSwitch {
                selected: "fallback/text-model".into(),
                resummarize: true,
            });
        }
        Ok(text_response("answer", FinishReason::Stop))
    }

    fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
        "fallback/text-model".into()
    }
}

fn carries_blob(message: &Message) -> bool {
    message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Compaction { .. }))
}

fn checkpoint_text(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::Text { text } if text.starts_with("[COMPACTION CHECKPOINT") => {
                Some(text.as_str())
            }
            _ => None,
        })
}

/// Restores a chat whose snapshot ends in an official-compaction checkpoint, then
/// returns every request the model saw after the restore.
async fn restore_blob_checkpoint(
    summary: Result<ChatResponse, RuntimeError>,
) -> Vec<(RequestPurpose, Vec<Message>)> {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("switch.sqlite3"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let database = storage::Database::open(&config).unwrap();
    let model = Arc::new(TextOnlyModel {
        summary,
        observed: Mutex::new(Vec::new()),
    });
    let bus = Arc::new(EventBus::new(256));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let prior = runtime.delegate_background(
        Role::Worker,
        "prior request".into(),
        RunConfig {
            name: Some(format!("chat:{}:thread", Role::Worker.name())),
            ..RunConfig::default()
        },
    );
    runtime.wait(prior).await.unwrap();
    let mut record = database.run_context(&prior.to_string()).unwrap().unwrap();
    let blob = CompactionCheckpoint {
        id: "checkpoint-1".into(),
        summary: Message {
            role: providers::Role::User,
            content: vec![ContentBlock::Compaction {
                encrypted_content: "opaque".into(),
            }],
        },
        range: (0, 2),
        protects_history: false,
    };
    record.checkpoints_json = serde_json::to_string(&vec![blob]).unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    model.observed.lock().unwrap().clear();
    let run = runtime
        .delegate_chat(
            "thread",
            Role::Worker,
            "followup".into(),
            RunConfig::default(),
        )
        .unwrap();
    runtime.wait(run).await.unwrap();
    let saved = database.run_context(&run.to_string()).unwrap().unwrap();
    let checkpoints: Vec<CompactionCheckpoint> =
        serde_json::from_str(&saved.checkpoints_json).unwrap();
    assert!(
        !carries_blob(&checkpoints.last().unwrap().summary),
        "the persisted latest checkpoint is the text summary"
    );
    model.observed.lock().unwrap().clone()
}

// Given: a restored chat whose checkpoint is a blob the selected model cannot replay
// When: the first request after the restore is prepared
// Then: the covered raw messages are re-summarized and only the text checkpoint is sent
#[tokio::test]
async fn restored_blob_checkpoint_is_resummarized_before_the_switch() {
    let observed =
        restore_blob_checkpoint(Ok(text_response("re-summary", FinishReason::Stop))).await;
    let purposes = observed
        .iter()
        .map(|(purpose, _)| *purpose)
        .collect::<Vec<_>>();
    assert_eq!(
        purposes,
        [
            RequestPurpose::Agent,
            RequestPurpose::Compaction,
            RequestPurpose::Agent
        ]
    );
    let (_, summary_input) = &observed[1];
    assert!(!summary_input.iter().any(carries_blob));
    assert!(summary_input.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text == "prior request"))
    }));
    let (_, resent) = &observed[2];
    assert!(!resent.iter().any(carries_blob));
    assert!(checkpoint_text(resent).unwrap().ends_with("re-summary"));
}

// Given: the same restore, but no chain member can produce the summary
// When: the switch is prepared
// Then: the structural summary completes it and the turn is still sent
#[tokio::test]
async fn failed_resummary_falls_back_to_the_structural_summary() {
    let observed = restore_blob_checkpoint(Err(RuntimeError::Model {
        reason: "no available candidate".into(),
    }))
    .await;
    let (purpose, resent) = observed.last().unwrap();
    assert_eq!(*purpose, RequestPurpose::Agent);
    assert!(!resent.iter().any(carries_blob));
    let text = checkpoint_text(resent).unwrap();
    assert!(text.contains("prior request"), "{text}");
}
