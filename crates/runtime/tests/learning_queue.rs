use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{AgentInvocationContext, AgentModel, ModelPreference, Role, RunStore, RuntimeError};
use serde_json::json;
use std::sync::{Arc, Mutex};
use storage::{Database, Storage, StorageConfig};

#[derive(Default)]
struct QueueModel(Mutex<Vec<(Role, Option<ModelPreference>, usize)>>);

#[async_trait::async_trait]
impl AgentModel for QueueModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.0
            .lock()
            .unwrap()
            .push((role, invocation.model_preference.clone(), tools.len()));
        if invocation.category.as_deref() == Some("lesson")
            && !messages.iter().flat_map(|message| &message.content).any(|block| {
                matches!(block, ContentBlock::ToolResult { tool_call_id, .. } if tool_call_id == "inspect-source")
            })
        {
            let source_run = messages.iter().flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => text.split_whitespace()
                        .find(|word| word.starts_with("run-") && word.ends_with('.'))
                        .map(|word| word.trim_end_matches('.').to_owned()),
                    _ => None,
                })
                .next()
                .expect("source run id in lesson prompt");
            return Ok(ChatResponse {
                message: Message {
                    role: providers::Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "inspect-source".into(),
                        name: "inspect_learning_source".into(),
                        input: json!({"run_id":source_run}),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::ToolUse,
            });
        }
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Completed without a lesson submission".into(),
                }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "fixture".into()
    }
}

#[tokio::test]
async fn learning_queue_completes_without_candidates_after_extractor_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("queue.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    store
        .handle()
        .enqueue_task(&storage::entity::TaskRecord {
            id: "task".into(),
            session_id: None,
            status: storage::entity::TaskStatus::Pending,
            parent_run_id: None,
            input: None,
            progress: None,
            last_artifact: None,
            failure_reason: None,
            resume_cursor: None,
            attempts: 0,
            heartbeat_at: None,
            created_at: std::time::UNIX_EPOCH,
            updated_at: std::time::UNIX_EPOCH,
        })
        .unwrap();
    let model = Arc::new(QueueModel::default());
    let bus = Arc::new(event_bus::EventBus::new(128));
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let runtime = runtime::AgentRuntime::new(bus, executor, model.clone())
        .with_run_store(RunStore::open(&config, store.handle()).unwrap());
    let queue = runtime::memory_queue::LearningQueue::new(
        runtime,
        (store.handle(), config.clone()),
        ModelPreference {
            profile: "quick".into(),
            model: None,
        },
    );
    let lessons = queue
        .execute(runtime::memory_queue::QueuedTask {
            id: "task",
            project: "p",
            prompt: "Complete bounded work",
            config: runtime::RunConfig::default(),
        })
        .await
        .unwrap();
    assert!(lessons.is_empty());
    assert_eq!(
        Database::open(&config)
            .unwrap()
            .task("task")
            .unwrap()
            .unwrap()
            .status,
        storage::entity::TaskStatus::Completed
    );
    assert_eq!(model.0.lock().unwrap().len(), 3);
}
