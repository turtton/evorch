use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, ToolResultContent, ToolSpec, Usage,
};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRunPhase, AgentRuntime, ModelPreference, Role,
    RunConfig, RunId, RunStore, RuntimeError,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use storage::memory::{LessonScope, MemoryStatus};
use storage::{Database, Storage, StorageConfig};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy)]
enum ReviewMode {
    Approve,
    Reject,
    FinalTextOnly,
    PartialReview,
    WaitForCancel,
    WaitForExtraction,
    WaitForAdmission,
    WaitForRoot,
    /// Two private candidates: one rejected and one approved.
    MixedReviewBatch,
    /// An approving review whose rationale the SecretGuard refuses at publication.
    SecretRationale,
}

const FAVORITE_PREFIX: &str = "Keep the bounded completion check";

type ExtractionRequest = (RunId, Vec<Message>, Vec<ToolSpec>);

struct Model {
    mode: ReviewMode,
    config: StorageConfig,
    extraction_started: Notify,
    root_started: Notify,
    root_release: Notify,
    hold_learning: AtomicBool,
    first_extractions: Mutex<Vec<ExtractionRequest>>,
    reviewer_started: Notify,
    reviewer_calls: AtomicUsize,
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        usage_reported: true,
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
        usage_reported: true,
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
                let ToolResultContent::Text { text } = &content[0] else {
                    panic!("expected text result");
                };
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
    fn requires_admission(&self) -> bool {
        matches!(self.mode, ReviewMode::WaitForAdmission)
    }
    async fn admit(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
    ) -> Result<(), RuntimeError> {
        if matches!(self.mode, ReviewMode::WaitForAdmission)
            && invocation.category.as_deref() == Some("lesson")
        {
            self.extraction_started.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
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
                if tool_result(messages, "extract-source").is_none() {
                    self.first_extractions.lock().unwrap().push((
                        invocation.run_id.parse().unwrap(),
                        messages.to_vec(),
                        tools.to_vec(),
                    ));
                    self.extraction_started.notify_one();
                }
                if matches!(self.mode, ReviewMode::WaitForExtraction)
                    && self.hold_learning.load(Ordering::SeqCst)
                {
                    std::future::pending::<()>().await;
                }
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
                            "evidence_refs":[reference],
                            "scope":"harness"
                        }),
                    ));
                }
                if matches!(
                    self.mode,
                    ReviewMode::PartialReview | ReviewMode::MixedReviewBatch
                ) && tool_result(messages, "stack-second").is_none()
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
                            "evidence_refs":[reference],
                            "scope":"project"
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
                if matches!(self.mode, ReviewMode::WaitForCancel)
                    && self.hold_learning.load(Ordering::SeqCst)
                {
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
                if matches!(self.mode, ReviewMode::MixedReviewBatch) {
                    let listed = tool_result(messages, "review-list").unwrap();
                    for (index, candidate) in listed["candidates"]
                        .as_array()
                        .expect("candidate list")
                        .iter()
                        .enumerate()
                    {
                        let call = format!("review-submit-{index}");
                        if tool_result(messages, &call).is_some() {
                            continue;
                        }
                        let rejected = candidate["content"]
                            .as_str()
                            .is_some_and(|content| content.starts_with(FAVORITE_PREFIX));
                        assert!(
                            Database::open(&self.config)
                                .unwrap()
                                .search_memory("p", "", None)
                                .unwrap()
                                .is_empty(),
                            "review cannot publish part of a batch"
                        );
                        return Ok(tool_response(
                            &call,
                            "submit_lesson_review",
                            json!({
                                "candidate_id": candidate["id"],
                                "verdict": if rejected { "reject" } else { "approve" },
                                "rationale": "Checked the cited source record",
                                "evidence_refs": candidate["evidence_refs"]
                            }),
                        ));
                    }
                } else if tool_result(messages, "review-submit").is_none() {
                    let listed = tool_result(messages, "review-list").unwrap();
                    let candidate = &listed["candidates"][0];
                    let verdict = if matches!(
                        self.mode,
                        ReviewMode::Approve
                            | ReviewMode::PartialReview
                            | ReviewMode::WaitForExtraction
                            | ReviewMode::SecretRationale
                    ) {
                        "approve"
                    } else {
                        "reject"
                    };
                    let rationale = if matches!(self.mode, ReviewMode::SecretRationale) {
                        format!("sk-{}", "a".repeat(32))
                    } else {
                        "Checked the cited source record".to_owned()
                    };
                    return Ok(tool_response(
                        "review-submit",
                        "submit_lesson_review",
                        json!({
                            "candidate_id": candidate["id"],
                            "verdict": verdict,
                            "rationale": rationale,
                            "evidence_refs": candidate["evidence_refs"]
                        }),
                    ));
                }
                Ok(text_response(
                    "Review complete. This final text is deliberately not JSON.",
                ))
            }
            _ => {
                if matches!(self.mode, ReviewMode::WaitForRoot) {
                    self.root_started.notify_one();
                    self.root_release.notified().await;
                }
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
    bus: Arc<event_bus::EventBus>,
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
            config: config.clone(),
            extraction_started: Notify::new(),
            root_started: Notify::new(),
            root_release: Notify::new(),
            hold_learning: AtomicBool::new(true),
            first_extractions: Mutex::default(),
            reviewer_started: Notify::new(),
            reviewer_calls: AtomicUsize::new(0),
        });
        let bus = Arc::new(event_bus::EventBus::new(512));
        let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let runtime = configure(AgentRuntime::new(bus.clone(), executor, model.clone()))
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
            bus,
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
        assert_eq!(self.runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
        run
    }

    async fn learning(&self, source: RunId) -> Result<(), String> {
        self.runtime.wait_learning(source).await.unwrap()
    }

    fn config_database(&self) -> Database {
        Database::open(&self.config).unwrap()
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
async fn typed_rejection_marks_lesson_rejected() {
    // Given: a reviewer that submits a typed reject verdict.
    let fixture = Fixture::new(ReviewMode::Reject);
    let source = fixture.start().await;
    // When: post-run learning completes.
    assert_eq!(fixture.learning(source).await, Ok(()));
    // Then: the verdict is recorded, so the lesson no longer looks unreviewed.
    let entries = fixture.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, MemoryStatus::Rejected);
}

#[tokio::test]
async fn review_keeps_the_batch_private_until_atomic_publication() {
    let fixture = Fixture::new(ReviewMode::MixedReviewBatch);
    let source = fixture.start().await;
    assert_eq!(fixture.learning(source).await, Ok(()));
    let entries = fixture.entries();
    assert_eq!(entries.len(), 2);
    let rejected = entries
        .iter()
        .find(|entry| entry.lesson.content.starts_with(FAVORITE_PREFIX))
        .unwrap();
    assert_eq!(rejected.status, MemoryStatus::Rejected);
    let other = entries
        .iter()
        .find(|entry| !entry.lesson.content.starts_with(FAVORITE_PREFIX))
        .unwrap();
    assert_eq!(other.status, MemoryStatus::Promoted);
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
    fixture.model.reviewer_started.notified().await;
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
async fn learning_failure_diagnostic_carries_its_cause() {
    let fixture = Fixture::new(ReviewMode::PartialReview);
    let mut receiver = fixture.bus.subscribe();
    let source = fixture.start().await;
    let _error = fixture.learning(source).await.unwrap_err();
    let diagnostic = receiver
        .drain_pending_snapshot()
        .into_iter()
        .find_map(|event| match event.kind {
            event_bus::EventKind::Diagnostic(d) if d.code == "LearningPipelineFailed" => Some(d),
            _ => None,
        })
        .expect("learning failure diagnostic");
    assert_eq!(diagnostic.run_id, Some(source.to_string()));
    assert!(
        diagnostic.detail.ends_with("\ncause: pipeline_failed"),
        "{}",
        diagnostic.detail
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
            .inspect(|entry| assert_eq!(entry.lesson.scope, LessonScope::Harness))
            .count();
        assert_eq!(candidates.len(), promoted);
        for candidate in candidates {
            assert_eq!(candidate.code, "LessonPromoted");
            assert_eq!(candidate.run_id, Some(source.to_string()));
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

#[tokio::test]
async fn pending_learning_starts_once_after_resume_and_never_after_shutdown() {
    let fixture = Fixture::new(ReviewMode::Approve);
    let gate = fixture.runtime.producer_gate();
    let pause = gate.pause();
    let source = fixture.start().await;
    assert_eq!(fixture.runtime.list_agents().len(), 1);
    let mut waiting = Box::pin(fixture.runtime.wait_learning(source));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(waiting);
    assert!(gate.resume(pause));
    assert_eq!(fixture.learning(source).await, Ok(()));
    assert_eq!(fixture.runtime.list_agents().len(), 3);
    assert_eq!(fixture.entries().len(), 1);
    assert!(!gate.resume(pause));
    let fixture = Fixture::new(ReviewMode::Approve);
    let gate = fixture.runtime.producer_gate();
    let pause = gate.pause();
    let source = fixture.start().await;
    gate.close();
    assert_eq!(fixture.learning(source).await, Ok(()));
    assert!(!gate.resume(pause));
    assert_eq!(fixture.runtime.list_agents().len(), 1);
}

fn job_states(fixture: &Fixture) -> Vec<storage::learning_jobs::LearningJobState> {
    use storage::learning_jobs::LearningJobState;
    let database = Database::open(&fixture.config).unwrap();
    LearningJobState::ALL
        .into_iter()
        .flat_map(|state| {
            database
                .learning_jobs(&[state])
                .unwrap()
                .into_iter()
                .map(move |_| state)
        })
        .collect()
}

#[tokio::test]
async fn a_completed_pipeline_ends_its_durable_job_and_drops_the_evidence() {
    use storage::learning_jobs::LearningJobState;
    let fixture = Fixture::new(ReviewMode::Approve);
    let source = fixture.start().await;
    assert_eq!(fixture.learning(source).await, Ok(()));
    assert_eq!(job_states(&fixture), [LearningJobState::Finished]);
    let database = Database::open(&fixture.config).unwrap();
    let job = &database
        .learning_jobs(&[LearningJobState::Finished])
        .unwrap()[0];
    assert!(
        database
            .learning_job_evidence(&job.task_id)
            .unwrap()
            .is_none()
    );
}

// New pipelines wait behind a paused learning gate, but a paused learning gate is
// not the shared producer gate: chats and lanes keep going.
#[tokio::test]
async fn a_learning_pause_holds_the_job_pending_without_pausing_other_producers() {
    use storage::learning_jobs::LearningJobState;
    let fixture = Fixture::new(ReviewMode::Approve);
    let learning = fixture.runtime.learning_gate();
    let pause = learning.pause();
    let source = fixture.start().await;
    // The pipeline admits its job on a spawned task and gives no completion signal
    // before it waits at the paused gate, so yield until the job is recorded. Only
    // scheduling is awaited, never a duration; a missing job hangs and the runner's
    // slow-timeout reports it.
    while job_states(&fixture).is_empty() {
        tokio::task::yield_now().await;
    }
    assert_eq!(job_states(&fixture), [LearningJobState::Pending]);
    let database = Database::open(&fixture.config).unwrap();
    let pending = &database
        .learning_jobs(&[LearningJobState::Pending])
        .unwrap()[0];
    assert!(
        database
            .learning_job_evidence(&pending.task_id)
            .unwrap()
            .is_some(),
        "a pending job keeps its evidence for a successor"
    );
    assert!(fixture.runtime.producer_gate().try_enter().is_some());
    assert_eq!(fixture.runtime.list_agents().len(), 1);
    assert!(learning.resume(pause));
    assert_eq!(fixture.learning(source).await, Ok(()));
    assert_eq!(job_states(&fixture), [LearningJobState::Finished]);
}

// Each case uses an explicit model/lifecycle notification, never elapsed time.
// Sealing a real dispatch epoch proves both root and internal scope leases returned.
#[tokio::test]
async fn learning_suspend_returns_all_four_dispatch_tails_and_pending_admission_without_provider_response()
 {
    for mode in [
        ReviewMode::WaitForExtraction,
        ReviewMode::WaitForAdmission,
        ReviewMode::WaitForCancel,
        ReviewMode::Approve,
        ReviewMode::WaitForRoot,
    ] {
        let fixture = Fixture::new(mode);
        let mut events = fixture.runtime.event_bus().subscribe();
        let late = matches!(mode, ReviewMode::WaitForRoot);
        let pending = matches!(mode, ReviewMode::Approve);
        let pause = if late || pending {
            Some(fixture.runtime.pause_learning())
        } else {
            None
        };
        let source = if late {
            let run = fixture.runtime.delegate_background(
                Role::Worker,
                "Late terminal root".into(),
                RunConfig::default(),
            );
            fixture.model.root_started.notified().await;
            run
        } else {
            fixture.start().await
        };
        if matches!(
            mode,
            ReviewMode::WaitForExtraction | ReviewMode::WaitForAdmission
        ) {
            fixture.model.extraction_started.notified().await;
        }
        if matches!(mode, ReviewMode::WaitForCancel) {
            fixture.model.reviewer_started.notified().await;
        }
        let pause = pause.unwrap_or_else(|| fixture.runtime.pause_learning());
        let suspension = fixture.runtime.suspend_learning(&pause).unwrap();
        if late {
            fixture.model.root_release.notify_one();
            assert_eq!(
                fixture.runtime.wait(source).await.unwrap(),
                AgentRunPhase::Done
            );
            fixture.runtime.drain_learning().await;
        }
        suspension.wait().await.unwrap();
        let job = fixture
            .config_database()
            .learning_jobs(&[
                storage::learning_jobs::LearningJobState::Interrupted,
                storage::learning_jobs::LearningJobState::Pending,
            ])
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(job.attempt, u32::from(!pending && !late));
        assert!(fixture.entries().is_empty());
        assert_eq!(fixture.runtime.learning_gate().active(), 0);
        assert_eq!(fixture.runtime.producer_gate().active(), 0);
        let attempt = fixture.runtime.begin_restart_quiesce().unwrap();
        fixture.runtime.wait_quiesced().await.unwrap();
        let fence = fixture.runtime.fence_dispatch_restart().await.unwrap();
        fixture.runtime.drain_dispatch_tails(&fence).await.unwrap();
        fixture.runtime.begin_recovery_capture(&fence).unwrap();
        let records = fixture
            .runtime
            .capture_recovery_records(&fence, &Default::default())
            .unwrap();
        assert!(
            records.iter().all(|record| {
                fixture.runtime.inspect_agent(record.run_id).map_or_else(
                    |error| {
                        matches!(mode, ReviewMode::WaitForAdmission)
                            && matches!(error, RuntimeError::UnknownRun { .. })
                    },
                    |agent| {
                        matches!(
                            agent.phase,
                            AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                        )
                    },
                )
            }),
            "mode {mode:?}, captured phases: {:?}",
            records
                .iter()
                .map(|r| (
                    r.run_id,
                    fixture.runtime.inspect_agent(r.run_id).map(|a| a.phase)
                ))
                .collect::<Vec<_>>()
        );
        assert!(events.drain_pending_snapshot().iter().all(|event| !matches!(&event.kind,
            event_bus::EventKind::Diagnostic(d) if d.code == event_bus::event::diagnostic_codes::LEARNING_PIPELINE_FAILED)));
        // Capturing is irreversible for learning in the old image.
        assert!(
            fixture
                .runtime
                .cancel_learning_suspension(pause, Arc::new(suspension))
                .is_err()
        );
        drop(attempt);
    }
}

#[tokio::test]
async fn cancelled_suspend_retries_same_evidence_with_fresh_runs_and_no_old_conversation() {
    use runtime::learning_jobs::{LearningJobState, LearningOriginKind, current_origin};
    let fixture = Fixture::with_runtime(ReviewMode::WaitForExtraction, |runtime| {
        runtime.with_learning_origin(current_origin(
            LearningOriginKind::Core,
            Some("fixture-config".into()),
        ))
    });
    let source = fixture.start().await;
    fixture.model.extraction_started.notified().await;
    let job = fixture
        .config_database()
        .learning_jobs(&[LearningJobState::Started])
        .unwrap()
        .pop()
        .unwrap();
    let evidence = fixture
        .config_database()
        .learning_job_evidence(&job.task_id)
        .unwrap()
        .unwrap();
    let pause = fixture.runtime.pause_learning();
    let suspension = Arc::new(fixture.runtime.suspend_learning(&pause).unwrap());
    suspension.wait().await.unwrap();
    assert_eq!(
        fixture
            .config_database()
            .learning_job_evidence(&job.task_id)
            .unwrap()
            .unwrap(),
        evidence
    );
    fixture.model.hold_learning.store(false, Ordering::SeqCst);
    fixture
        .runtime
        .cancel_learning_suspension(pause, suspension)
        .unwrap();
    fixture
        .runtime
        .wait_learning(source)
        .await
        .unwrap()
        .unwrap();
    let finished = fixture
        .config_database()
        .learning_job(&job.task_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        (finished.state, finished.attempt),
        (LearningJobState::Finished, 2)
    );
    assert!(
        fixture
            .config_database()
            .learning_job_evidence(&job.task_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.entries().len(), 1);
    let requests = fixture.model.first_extractions.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0].0, requests[1].0);
    assert_eq!(
        serde_json::to_value(&requests[0].1).unwrap(),
        serde_json::to_value(&requests[1].1).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&requests[0].2).unwrap(),
        serde_json::to_value(&requests[1].2).unwrap()
    );
}

// The SecretGuard refuses the batch at publication. Retrying that publish can never
// succeed, so the job must end through the failure terminal and release its cleanup.
#[tokio::test]
async fn secret_guard_rejected_publication_ends_failed_and_releases_cleanup() {
    use storage::learning_jobs::LearningJobState;
    let fixture = Fixture::new(ReviewMode::SecretRationale);
    let source = fixture.start().await;
    assert!(fixture.learning(source).await.is_err());
    assert_eq!(job_states(&fixture), [LearningJobState::Failed]);
    let database = Database::open(&fixture.config).unwrap();
    let job = &database.learning_jobs(&[LearningJobState::Failed]).unwrap()[0];
    assert_eq!(
        job.reason.as_deref(),
        Some(storage::learning_jobs::LearningEndReason::PipelineFailed.as_str())
    );
    assert!(
        database
            .learning_job_evidence(&job.task_id)
            .unwrap()
            .is_none()
    );
    // The secret never reaches a promoted lesson.
    assert!(
        fixture
            .entries()
            .iter()
            .all(|entry| entry.status != MemoryStatus::Promoted)
    );
    // Nothing remains owned: a suspension has no cleanup to wait for.
    let pause = fixture.runtime.pause_learning();
    let suspension = fixture.runtime.suspend_learning(&pause).unwrap();
    suspension.wait().await.unwrap();
}
