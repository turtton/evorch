use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRunPhase, AgentRuntime, ModelPreference, Role,
    RunConfig, RunId, RuntimeError,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use storage::memory::MemoryStatus;
use storage::{Database, Storage, StorageConfig};
use tokio::sync::Notify;

enum ReviewerBehavior {
    Immediate,
    Delayed(Duration),
    WaitForCancel,
    RepeatRead(PathBuf),
}

struct Model {
    calls: Mutex<Vec<Role>>,
    approve: bool,
    invalid_interview: bool,
    raw_review: bool,
    reviewer_behavior: ReviewerBehavior,
    reviewer_started: Notify,
    reviewer_calls: AtomicUsize,
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
            let call = self.reviewer_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                let prompt = messages
                    .iter()
                    .filter(|message| message.role == providers::Role::User)
                    .flat_map(|message| &message.content)
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(prompt.contains("durable memory lessons for FUTURE tasks"));
                assert!(prompt.contains("do not reopen or re-execute the original task work"));
                assert!(prompt.contains("Mark unverifiable evidence as unknown"));
                // Preserve the exact output contract while changing only its framing.
                assert!(prompt.contains("Return a JSON object (a fenced json block is also accepted) with verdict (approve or request-update), findings, and criteria (id: exact verified evidence reference, status: met/unmet/unknown, note, evidence). Each evidence object has command, exit_status (integer), target_sha, and optional diff_ref, artifact_path, red_evidence strings. Use null when evidence is unavailable; never invent evidence or mark evidence met without checking it."));
                let names: Vec<_> = tools.iter().map(|tool| tool.name.as_str()).collect();
                for required in ["read", "grep", "git_diff", "submit_review"] {
                    assert!(names.contains(&required));
                }
                for forbidden in ["write", "edit", "shell", "delegate"] {
                    assert!(!names.contains(&forbidden));
                }
                self.reviewer_started.notify_one();
            }
            match &self.reviewer_behavior {
                ReviewerBehavior::Immediate => {}
                ReviewerBehavior::Delayed(delay) => tokio::time::sleep(*delay).await,
                ReviewerBehavior::WaitForCancel => std::future::pending::<()>().await,
                ReviewerBehavior::RepeatRead(path) => {
                    return Ok(ChatResponse {
                        message: Message {
                            role: providers::Role::Assistant,
                            content: vec![ContentBlock::ToolUse {
                                id: format!("repeat-{call}"),
                                name: "read".into(),
                                input: serde_json::json!({"path": path}),
                            }],
                        },
                        usage: Usage::default(),
                        finish_reason: FinishReason::ToolUse,
                    });
                }
            }
            if self.approve && self.raw_review {
                r#"{"verdict":"approve","criteria":[{"id":"test:bound","status":"met","note":"checked","evidence":{"command":"cargo test","exit_status":0,"target_sha":"abc","artifact_path":"test.log"}}]}"#
            } else if self.approve {
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
    fn new(
        approve: bool,
        invalid_interview: bool,
        raw_review: bool,
        reviewer_behavior: ReviewerBehavior,
    ) -> Self {
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
            raw_review,
            reviewer_behavior,
            reviewer_started: Notify::new(),
            reviewer_calls: AtomicUsize::new(0),
        });
        let bus = Arc::new(event_bus::EventBus::new(512));
        let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let runtime = AgentRuntime::new(bus.clone(), executor, model.clone()).with_learning(
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
        let id = self.runtime.delegate_background(
            Role::Worker,
            "Complete bounded work".into(),
            RunConfig::default(),
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), self.runtime.wait(id))
                .await
                .expect("worker must finish")
                .unwrap(),
            AgentRunPhase::Done
        );
        id
    }

    async fn reviewer(&self) -> RunId {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.model.reviewer_started.notified(),
        )
        .await
        .expect("learning reviewer must start");
        let reviewers: Vec<_> = self
            .runtime
            .list_agents()
            .into_iter()
            .filter(|agent| agent.role_name == Role::Reviewer.name())
            .collect();
        assert_eq!(reviewers.len(), 1);
        assert_eq!(reviewers[0].name, "learning-evidence-review");
        reviewers[0].run_id
    }

    async fn learning(&self, worker: RunId, deadline: Duration) -> Result<(), String> {
        tokio::time::timeout(deadline, self.runtime.wait_learning(worker))
            .await
            .expect("learning must finish without hanging")
            .unwrap()
    }

    fn entries(&self) -> Vec<storage::memory::MemoryEntry> {
        Database::open(&self.config)
            .unwrap()
            .search_memory("p", "", None)
            .unwrap()
    }

    fn assert_incomplete(&self, outcome: Result<(), String>) {
        assert_eq!(
            outcome,
            Err(runtime::memory::InterviewError::Incomplete.to_string())
        );
        assert!(self.entries().is_empty(), "no lessons may be promoted");
        assert_eq!(
            self.runtime.list_agents().len(),
            2,
            "learning must not recurse"
        );
        assert_eq!(
            self.model.calls.lock().unwrap().len(),
            1 + self.model.reviewer_calls.load(Ordering::SeqCst),
            "an incomplete review must not reach the interviewer"
        );
    }
}

async fn run(
    approve: bool,
    invalid_interview: bool,
    raw_review: bool,
) -> (
    Vec<storage::memory::MemoryEntry>,
    Vec<Role>,
    Result<(), String>,
) {
    let fixture = Fixture::new(
        approve,
        invalid_interview,
        raw_review,
        ReviewerBehavior::Immediate,
    );
    let id = fixture.start().await;
    fixture.reviewer().await;
    let outcome = fixture.learning(id, Duration::from_secs(5)).await;
    let entries = fixture.entries();
    let calls = fixture.model.calls.lock().unwrap().clone();
    (entries, calls, outcome)
}

#[tokio::test(start_paused = true)]
async fn reviewer_can_exceed_120_seconds_and_promote_verified_lessons() {
    // Given: a legitimate evidence review taking longer than the old outer timeout.
    let fixture = Fixture::new(
        true,
        false,
        true,
        ReviewerBehavior::Delayed(Duration::from_secs(121)),
    );
    let worker = fixture.start().await;
    let reviewer = fixture.reviewer().await;
    let started = tokio::time::Instant::now();

    // When: Tokio's paused clock advances automatically; no real-time sleep is used.
    assert_eq!(
        fixture.learning(worker, Duration::from_secs(180)).await,
        Ok(())
    );

    // Then: the reviewer finishes normally and both evidence-matched lessons promote.
    assert!(started.elapsed() >= Duration::from_secs(121));
    assert_eq!(
        fixture.runtime.inspect_agent(reviewer).unwrap().phase,
        AgentRunPhase::Done
    );
    let entries = fixture.entries();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == MemoryStatus::Promoted)
    );
    assert_eq!(
        *fixture.model.calls.lock().unwrap(),
        vec![Role::Worker, Role::Reviewer, Role::Worker, Role::Reviewer]
    );
}

#[tokio::test(start_paused = true)]
async fn cancelling_learning_reviewer_returns_incomplete_without_promotion() {
    // Given: a reviewer suspended inside the mock provider, with no response forthcoming.
    let fixture = Fixture::new(true, false, true, ReviewerBehavior::WaitForCancel);
    let worker = fixture.start().await;
    let reviewer = fixture.reviewer().await;

    // When: explicit cancellation interrupts that in-flight provider call.
    fixture.runtime.cancel(reviewer).unwrap();
    let outcome = fixture.learning(worker, Duration::from_secs(5)).await;

    // Then: the reviewer is non-Done, learning fails promptly, and nothing is promoted.
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), fixture.runtime.wait(reviewer))
            .await
            .expect("cancelled reviewer must terminate")
            .unwrap(),
        AgentRunPhase::Error
    );
    assert_eq!(
        fixture.runtime.inspect_agent(worker).unwrap().phase,
        AgentRunPhase::Done
    );
    fixture.assert_incomplete(outcome);
}

#[tokio::test]
async fn identical_reviewer_tool_calls_hard_stop_without_promotion() {
    // Given: a reviewer repeatedly reading the same local evidence with fresh call IDs.
    let dir = tempfile::tempdir().unwrap();
    let evidence = dir.path().join("evidence.txt");
    std::fs::write(&evidence, "test:bound passed\n").unwrap();
    let fixture = Fixture::new(true, false, true, ReviewerBehavior::RepeatRead(evidence));
    let mut events = fixture.bus.subscribe();
    let worker = fixture.start().await;
    let reviewer = fixture.reviewer().await;

    // When: the ordinary, unmodified identical-call guard observes the tenth call.
    let outcome = fixture.learning(worker, Duration::from_secs(5)).await;

    // Then: the normal guard, not a learning deadline, hard-stops the reviewer.
    assert_eq!(
        RunConfig::default().budget.max_identical_tool_call_repeats,
        10
    );
    assert_eq!(fixture.model.reviewer_calls.load(Ordering::SeqCst), 10);
    assert_eq!(
        fixture.runtime.inspect_agent(reviewer).unwrap().phase,
        AgentRunPhase::Error
    );
    let diagnostic = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let event_bus::EventKind::Diagnostic(diagnostic) = events.recv().await.unwrap().kind
                && diagnostic.code == "IdenticalToolCalls"
            {
                break diagnostic;
            }
        }
    })
    .await
    .expect("the identical-call guard must emit its diagnostic");
    assert_eq!(diagnostic.run_id, Some(reviewer.to_string()));
    assert_eq!(diagnostic.call_id.as_deref(), Some("repeat-9"));
    assert_eq!(diagnostic.severity, event_bus::DiagnosticSeverity::Error);
    fixture.assert_incomplete(outcome);
}

#[tokio::test]
async fn ordinary_run_automatically_interviews_and_promotes_verified_lessons() {
    // Given / When: run through the ordinary runtime surface with verified evidence.
    let (entries, calls, outcome) = run(true, false, false).await;
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
    let (entries, _, outcome) = run(false, false, false).await;
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
    let (entries, _, outcome) = run(true, true, false).await;
    // Then: learning failure is observable without failing the completed task.
    assert!(outcome.is_err());
    assert!(entries.is_empty());
}

#[tokio::test]
async fn raw_typed_review_promotes_lessons_with_matching_evidence_reference() {
    // Given / When: the runtime receives raw JSON with criterion evidence.
    let (entries, _, outcome) = run(true, false, true).await;
    // Then: the unchanged lesson reference matching promotes both lessons.
    assert_eq!(outcome, Ok(()));
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == MemoryStatus::Promoted)
    );
}
