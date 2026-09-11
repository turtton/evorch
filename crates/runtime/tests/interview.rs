use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::memory::{InterviewInput, Interviewer};
use runtime::{AgentInvocationContext, AgentModel, ModelPreference, Role, RuntimeError};
use std::sync::{Arc, Mutex};
use storage::memory::MemoryStatus;
use storage::{Database, Storage, StorageConfig};

#[derive(Default)]
struct InterviewModel(Mutex<Vec<(Role, Option<ModelPreference>, usize)>>);

#[async_trait::async_trait]
impl AgentModel for InterviewModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        _messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.0
            .lock()
            .unwrap()
            .push((role, invocation.model_preference.clone(), tools.len()));
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: r#"{"content":"Bound concurrency","evidence":"test:bound"}"#.into(),
                }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role) -> String {
        "fixture".into()
    }
}

#[tokio::test]
async fn interview_uses_quick_for_both_roles_and_only_persists_candidates() {
    // Given: a quick-model preference and real single-writer storage.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let model = Arc::new(InterviewModel::default());
    let quick = ModelPreference {
        profile: "quick-profile".into(),
        model: Some("quick-model".into()),
    };
    let interviewer = Interviewer::new(model.clone(), quick.clone(), store.handle());
    // When: interview both completed task reports.
    interviewer
        .interview(&InterviewInput {
            project: "p",
            task_id: "t",
            worker_report: "worker evidence",
            reviewer_report: "review evidence",
        })
        .await
        .unwrap();
    // Then: no tools or promotion authority reach the model.
    assert_eq!(
        *model.0.lock().unwrap(),
        vec![
            (Role::Worker, Some(quick.clone()), 0),
            (Role::Reviewer, Some(quick), 0)
        ]
    );
    let db = Database::open(&config).unwrap();
    assert_eq!(
        db.search_memory("p", "", Some(MemoryStatus::Candidate))
            .unwrap()
            .len(),
        2
    );
    assert!(
        db.search_memory("p", "", Some(MemoryStatus::Promoted))
            .unwrap()
            .is_empty()
    );
    interviewer
        .interview(&InterviewInput {
            project: "p",
            task_id: "t",
            worker_report: "worker evidence",
            reviewer_report: "review evidence",
        })
        .await
        .unwrap();
    for entry in db.search_memory("p", "", None).unwrap() {
        assert_eq!(db.memory_history(&entry.lesson.id).unwrap().len(), 1);
    }
}

#[tokio::test]
async fn learning_queue_runs_worker_reviewer_and_interview_after_dependency_gate() {
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
            created_at: std::time::UNIX_EPOCH,
            updated_at: std::time::UNIX_EPOCH,
        })
        .unwrap();
    let model = Arc::new(InterviewModel::default());
    let bus = Arc::new(event_bus::EventBus::new(128));
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let runtime = runtime::AgentRuntime::new(bus, executor, model.clone());
    let interviewer = Interviewer::new(
        model.clone(),
        ModelPreference {
            profile: "quick".into(),
            model: None,
        },
        store.handle(),
    );
    let queue = runtime::memory_queue::LearningQueue::new(
        runtime,
        (store.handle(), config.clone()),
        interviewer,
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
    assert_eq!(lessons.len(), 2);
    assert_eq!(
        Database::open(&config)
            .unwrap()
            .task("task")
            .unwrap()
            .unwrap()
            .status,
        storage::entity::TaskStatus::Completed
    );
    assert_eq!(model.0.lock().unwrap().len(), 4);
}
