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
        Self::with_runtime(mode, |runtime| runtime)
    }

    fn with_runtime(
        mode: ReviewMode,
        configure: impl FnOnce(AgentRuntime) -> AgentRuntime,
    ) -> Self {
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
        let runtime = configure(AgentRuntime::new(bus, executor, model.clone()))
            .with_sequential_run_ids()
            .with_run_store(RunStore::open(&config, store.handle()).unwrap())
            .with_learning(runtime::memory_queue::LearningSettings {
                writer: store.handle(),
                storage: config.clone(),
                project: "p".into(),
                quick: ModelPreference {
                    profile: "quick".into(),
                    model: None,
                    reasoning_effort: None,
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
        self.start_with(RunConfig::default()).await
    }

    async fn start_with(&self, config: RunConfig) -> RunId {
        let run =
            self.runtime
                .delegate_background(Role::Worker, "Complete bounded work".into(), config);
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
        self.entries_in("p")
    }

    fn entries_in(&self, project: &str) -> Vec<storage::memory::MemoryEntry> {
        Database::open(&self.config)
            .unwrap()
            .search_memory(project, "", None)
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

// Lessons are partitioned by the project the run worked in, not the startup one.
#[tokio::test]
async fn lessons_land_in_the_project_the_run_worked_in() {
    let fixture = Fixture::with_runtime(ReviewMode::Approve, |runtime| {
        runtime.with_project_slugs(Arc::new(|root| {
            format!("slug:{}", root.file_name().unwrap().to_string_lossy())
        }))
    });
    let project = tempfile::tempdir().unwrap();
    let slug = format!(
        "slug:{}",
        project.path().file_name().unwrap().to_string_lossy()
    );

    let source = fixture
        .start_with(RunConfig {
            project_root: Some(project.path().to_path_buf()),
            ..RunConfig::default()
        })
        .await;

    assert_eq!(fixture.learning(source).await, Ok(()));
    assert_eq!(fixture.entries_in(&slug).len(), 1);
    assert!(fixture.entries().is_empty());
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

#[tokio::test]
async fn self_improvement_observes_only_promoted_lessons_after_completion() {
    use runtime::self_improvement::{ImprovementPolicy, ImprovementSettings};
    for mode in [ReviewMode::Approve, ReviewMode::Reject] {
        let mut fixture = Fixture::new(mode);
        let drafts = fixture._dir.path().join("drafts");
        fixture.runtime = fixture.runtime.with_self_improvement(ImprovementSettings {
            writer: fixture._store.handle(),
            project: "p".into(),
            policy: ImprovementPolicy {
                draft_dir: Some(drafts.clone()),
                ..Default::default()
            },
        });
        let source = fixture.start().await;
        assert_eq!(fixture.learning(source).await, Ok(()));
        let candidates = Database::open(&fixture.config)
            .unwrap()
            .improvement_candidates("p", None, 10)
            .unwrap();
        let promoted = fixture
            .entries()
            .into_iter()
            .filter(|entry| entry.status == MemoryStatus::Promoted)
            .count();
        assert_eq!(candidates.len(), promoted);
        for candidate in candidates {
            assert_eq!(candidate.code, "LessonPromoted");
            assert!(drafts.join(candidate.draft_path.unwrap()).exists());
        }
        assert_eq!(
            fixture.runtime.list_agents().len(),
            3,
            "intake must not run another model"
        );
    }
}

#[tokio::test]
async fn self_improvement_draft_failure_preserves_successful_learning() {
    use runtime::self_improvement::{ImprovementPolicy, ImprovementSettings};
    let mut fixture = Fixture::new(ReviewMode::Approve);
    let blocked = fixture._dir.path().join("not-a-directory");
    std::fs::write(&blocked, "blocked").unwrap();
    fixture.runtime = fixture.runtime.with_self_improvement(ImprovementSettings {
        writer: fixture._store.handle(),
        project: "p".into(),
        policy: ImprovementPolicy {
            draft_dir: Some(blocked),
            ..Default::default()
        },
    });
    let source = fixture.start().await;
    assert_eq!(fixture.learning(source).await, Ok(()));
    assert_eq!(fixture.entries()[0].status, MemoryStatus::Promoted);
    let candidates = Database::open(&fixture.config)
        .unwrap()
        .improvement_candidates("p", None, 10)
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert!(candidates[0].draft_path.is_none());
}
