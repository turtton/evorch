use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, ModelPreference, Role, RunConfig,
    RuntimeError,
};
use std::sync::{Arc, Mutex};
use storage::memory::MemoryStatus;
use storage::{Database, Storage, StorageConfig};

struct Model {
    calls: Mutex<Vec<Role>>,
    approve: bool,
    invalid_interview: bool,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        _: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.calls.lock().unwrap().push(role);
        let text = if invocation.run_id.starts_with("interview:") {
            assert!(tools.is_empty());
            assert_eq!(
                invocation.model_preference.as_ref().unwrap().profile,
                "quick"
            );
            if self.invalid_interview && role == Role::Reviewer {
                r#"{"content":"Bound work","evidence":""}"#
            } else {
                r#"{"content":"Bound work","evidence":"test:bound"}"#
            }
        } else if role == Role::Reviewer {
            if self.approve {
                "```json\n{\"verdict\":\"approve\",\"criteria\":[{\"id\":\"test:bound\",\"status\":\"met\",\"note\":\"Verified bounded execution\"}]}\n```"
            } else {
                "```json\n{\"verdict\":\"request-update\",\"findings\":[\"Missing proof\"]}\n```"
            }
        } else {
            "Completed with evidence test:bound"
        };
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text { text: text.into() }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role) -> String {
        "fixture".into()
    }
}

async fn run(
    approve: bool,
    invalid_interview: bool,
) -> (
    Vec<storage::memory::MemoryEntry>,
    Vec<Role>,
    Result<(), String>,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let model = Arc::new(Model {
        calls: Mutex::new(Vec::new()),
        approve,
        invalid_interview,
    });
    let bus = Arc::new(event_bus::EventBus::new(128));
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, model.clone()).with_learning(
        runtime::memory_queue::LearningSettings {
            writer: store.handle(),
            storage: config.clone(),
            project: "p".into(),
            quick: ModelPreference {
                profile: "quick".into(),
                model: None,
            },
        },
    );
    let id = runtime.delegate_background(
        Role::Worker,
        "Complete bounded work".into(),
        RunConfig::default(),
    );
    assert_eq!(
        runtime.wait(id).await.unwrap(),
        runtime::AgentRunPhase::Done
    );
    let outcome =
        tokio::time::timeout(std::time::Duration::from_secs(5), runtime.wait_learning(id))
            .await
            .unwrap()
            .unwrap();
    let entries = Database::open(&config)
        .unwrap()
        .search_memory("p", "", None)
        .unwrap();
    let calls = model.calls.lock().unwrap().clone();
    (entries, calls, outcome)
}

#[tokio::test]
async fn ordinary_run_automatically_interviews_and_promotes_verified_lessons() {
    // Given / When: run through the ordinary runtime surface with verified evidence.
    let (entries, calls, outcome) = run(true, false).await;
    // Then: both lessons are promoted and learning does not recurse.
    assert!(outcome.is_ok());
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == MemoryStatus::Promoted)
    );
    assert_eq!(
        calls,
        vec![Role::Worker, Role::Reviewer, Role::Worker, Role::Reviewer]
    );
}

#[tokio::test]
async fn rejected_review_keeps_lessons_as_candidates() {
    // Given / When: a completed run whose reviewer rejects the evidence.
    let (entries, _, outcome) = run(false, false).await;
    // Then: interview still runs, without bypassing promotion authority.
    assert!(outcome.is_ok());
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == MemoryStatus::Candidate)
    );
}

#[tokio::test]
async fn invalid_second_interview_does_not_persist_partial_lessons() {
    // Given / When: the second interview has invalid evidence.
    let (entries, _, outcome) = run(true, true).await;
    // Then: learning failure is observable without failing the completed task.
    assert!(outcome.is_err());
    assert!(entries.is_empty());
}
