use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, ToolResultContent, ToolSpec, Usage,
};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRunPhase, AgentRuntime, ModelPreference, Role,
    RunConfig, RunId, RunStore, RuntimeError,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use storage::memory::MemoryStatus;
use storage::{Database, Storage, StorageConfig};
use tokio::sync::Notify;

#[derive(Clone, Copy)]
enum ReviewMode {
    Approve,
    Reject,
    FinalTextOnly,
    PartialReview,
    WaitForCancel,
}

struct Model {
    mode: ReviewMode,
    reviewer_started: Notify,
    reviewer_calls: AtomicUsize,
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: providers::Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        },
        usage: Usage::default(),
        finish_reason: FinishReason::Stop,
    }
}

fn tool_response(id: &str, name: &str, input: Value) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: providers::Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            }],
        },
        usage: Usage::default(),
        finish_reason: FinishReason::ToolUse,
    }
}

fn tool_result(messages: &[Message], id: &str) -> Option<Value> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == id => {
                assert!(!is_error, "learning tool {id} failed: {content:?}");
                let ToolResultContent::Text { text } = &content[0];
                Some(serde_json::from_str(text).expect("learning tool JSON"))
            }
            _ => None,
        })
}

fn source_run(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|message| message.role == providers::Role::User)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => text
                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-')
                .find(|token| {
                    token.strip_prefix("run-").is_some_and(|number| {
                        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                    })
                })
                .map(str::to_owned),
            _ => None,
        })
        .next()
        .expect("internal learning prompt identifies source run")
}

#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let names: Vec<_> = tools.iter().map(|tool| tool.name.as_str()).collect();
        match invocation.category.as_deref() {
            Some("lesson") => {
                assert_eq!(role, Role::Worker);
                assert_eq!(
                    invocation.model_preference.as_ref().unwrap().profile,
                    "quick"
                );
                assert!(names.contains(&"inspect_learning_source"));
                assert!(names.contains(&"stack_lesson_candidate"));
                assert!(!names.contains(&"shell"));
                assert!(!names.contains(&"submit_lesson_review"));
                if tool_result(messages, "extract-source").is_none() {
                    return Ok(tool_response(
                        "extract-source",
                        "inspect_learning_source",
                        json!({"run_id":source_run(messages)}),
                    ));
                }
                if tool_result(messages, "stack-lesson").is_none() {
                    let source = tool_result(messages, "extract-source").unwrap();
                    let reference = source["records"]
                        .as_array()
                        .and_then(|records| records.last())
                        .and_then(|record| record["reference"].as_str())
                        .expect("source evidence reference");
                    return Ok(tool_response(
                        "stack-lesson",
                        "stack_lesson_candidate",
                        json!({
                            "content":"Keep the bounded completion check for future tasks",
                            "evidence_refs":[reference]
                        }),
                    ));
                }
                if matches!(self.mode, ReviewMode::PartialReview)
                    && tool_result(messages, "stack-second").is_none()
                {
                    let source = tool_result(messages, "extract-source").unwrap();
                    let reference = source["records"]
                        .as_array()
                        .and_then(|records| records.last())
                        .and_then(|record| record["reference"].as_str())
                        .expect("source evidence reference");
                    return Ok(tool_response(
                        "stack-second",
                        "stack_lesson_candidate",
                        json!({
                            "content":"Keep a second distinct bounded check for future tasks",
                            "evidence_refs":[reference]
                        }),
                    ));
                }
                Ok(text_response(
                    "Extraction complete. This final text is deliberately not JSON.",
                ))
            }
            Some("lesson_review") => {
                assert_eq!(role, Role::Reviewer);
                assert!(names.contains(&"inspect_learning_source"));
                assert!(names.contains(&"list_lesson_candidates"));
                assert!(names.contains(&"submit_lesson_review"));
                assert!(!names.contains(&"shell"));
                assert!(!names.contains(&"stack_lesson_candidate"));
                if self.reviewer_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.reviewer_started.notify_one();
                }
                if matches!(self.mode, ReviewMode::WaitForCancel) {
                    std::future::pending::<()>().await
                }
                if tool_result(messages, "review-list").is_none() {
                    return Ok(tool_response(
                        "review-list",
                        "list_lesson_candidates",
                        json!({}),
                    ));
                }
                if tool_result(messages, "review-source").is_none() {
                    return Ok(tool_response(
                        "review-source",
                        "inspect_learning_source",
                        json!({"run_id":source_run(messages)}),
                    ));
                }
                if matches!(self.mode, ReviewMode::FinalTextOnly) {
                    return Ok(text_response("{\"verdict\":\"approve\"}"));
                }
                if tool_result(messages, "review-submit").is_none() {
                    let listed = tool_result(messages, "review-list").unwrap();
                    let candidate = &listed["candidates"][0];
                    let verdict =
                        if matches!(self.mode, ReviewMode::Approve | ReviewMode::PartialReview) {
                            "approve"
                        } else {
                            "reject"
                        };
                    return Ok(tool_response(
                        "review-submit",
                        "submit_lesson_review",
                        json!({
                            "candidate_id": candidate["id"],
                            "verdict": verdict,
                            "rationale": "Checked the cited source record",
                            "evidence_refs": candidate["evidence_refs"]
                        }),
                    ));
                }
                Ok(text_response(
                    "Review complete. This final text is deliberately not JSON.",
                ))
            }
            _ => {
                assert_eq!(role, Role::Worker);
                assert!(!names.contains(&"inspect_learning_source"));
                assert!(!names.contains(&"stack_lesson_candidate"));
                Ok(text_response("Completed with evidence test:bound"))
            }
        }
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "fixture".into()
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    _store: Storage,
    config: StorageConfig,
    model: Arc<Model>,
    runtime: AgentRuntime,
}

impl Fixture {
    fn new(mode: ReviewMode) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("memory.db"),
            ..Default::default()
        };
        let store = Storage::open(config.clone()).unwrap();
        let model = Arc::new(Model {
            mode,
            reviewer_started: Notify::new(),
            reviewer_calls: AtomicUsize::new(0),
        });
        let bus = Arc::new(event_bus::EventBus::new(512));
        let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let runtime = AgentRuntime::new(bus, executor, model.clone())
            .with_run_store(RunStore::open(&config, store.handle()).unwrap())
            .with_learning(runtime::memory_queue::LearningSettings {
                writer: store.handle(),
                storage: config.clone(),
                project: "p".into(),
                quick: ModelPreference {
                    profile: "quick".into(),
                    model: None,
                },
            });
        Self {
            _dir: dir,
            _store: store,
            config,
            model,
            runtime,
        }
    }

    async fn start(&self) -> RunId {
        let run = self.runtime.delegate_background(
            Role::Worker,
            "Complete bounded work".into(),
            RunConfig::default(),
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), self.runtime.wait(run))
                .await
                .unwrap()
                .unwrap(),
            AgentRunPhase::Done
        );
        run
    }

    async fn learning(&self, source: RunId) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(5), self.runtime.wait_learning(source))
            .await
            .expect("learning must finish")
            .unwrap()
    }

    fn entries(&self) -> Vec<storage::memory::MemoryEntry> {
        Database::open(&self.config)
            .unwrap()
            .search_memory("p", "", None)
            .unwrap()
    }
}

#[tokio::test]
async fn typed_approval_promotes_despite_non_json_final_text() {
    let fixture = Fixture::new(ReviewMode::Approve);
    let source = fixture.start().await;
    assert_eq!(fixture.learning(source).await, Ok(()));
    let entries = fixture.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, MemoryStatus::Promoted);
    assert_eq!(
        fixture.runtime.list_agents().len(),
        3,
        "learning must not recurse"
    );
}

#[tokio::test]
async fn typed_rejection_keeps_lesson_as_candidate() {
    let fixture = Fixture::new(ReviewMode::Reject);
    let source = fixture.start().await;
    assert_eq!(fixture.learning(source).await, Ok(()));
    let entries = fixture.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, MemoryStatus::Candidate);
}

#[tokio::test]
async fn json_looking_final_text_without_typed_review_cannot_promote() {
    let fixture = Fixture::new(ReviewMode::FinalTextOnly);
    let source = fixture.start().await;
    assert_eq!(
        fixture.learning(source).await,
        Err(runtime::memory_queue::LearningError::Incomplete.to_string())
    );
    let entries = fixture.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, MemoryStatus::Candidate);
}

#[tokio::test]
async fn cancelling_lesson_reviewer_does_not_promote() {
    let fixture = Fixture::new(ReviewMode::WaitForCancel);
    let source = fixture.start().await;
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.model.reviewer_started.notified(),
    )
    .await
    .expect("reviewer started");
    let reviewer = fixture
        .runtime
        .list_agents()
        .into_iter()
        .find(|agent| agent.name == "learning-evidence-review")
        .unwrap()
        .run_id;
    fixture.runtime.cancel(reviewer).unwrap();
    assert_eq!(
        fixture.learning(source).await,
        Err(runtime::memory_queue::LearningError::Incomplete.to_string())
    );
    let entries = fixture.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, MemoryStatus::Candidate);
}

#[tokio::test]
async fn incomplete_candidate_review_batch_promotes_none() {
    let fixture = Fixture::new(ReviewMode::PartialReview);
    let source = fixture.start().await;
    assert_eq!(
        fixture.learning(source).await,
        Err(runtime::memory_queue::LearningError::Incomplete.to_string())
    );
    let entries = fixture.entries();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == MemoryStatus::Candidate)
    );
}
