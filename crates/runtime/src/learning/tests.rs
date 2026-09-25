use super::*;
use crate::{AgentInvocationContext, AgentModel, ExecutionPolicy, Role, RunStore, RuntimeError};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use storage::{Storage, StorageConfig};

struct StopModel;
#[async_trait::async_trait]
impl AgentModel for StopModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "done".into(),
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

fn record(id: &str, parent: Option<&str>, text: &str) -> storage::RunContextRecord {
    storage::RunContextRecord {
        run_id: id.into(), role: "worker".into(), name: id.into(), parent_run_id: parent.map(str::to_string),
        config_json: "{}".into(), messages_json: json!([
            {"role":"system","content":[{"type":"text","text":"hidden system"}]},
            {"role":"user","content":[{"type":"text","text":text}]},
            {"role":"assistant","content":[{"type":"reasoning","text":"hidden reasoning"},{"type":"image","media_type":"image/png","data":"hidden image"}]}
        ]).to_string(), checkpoints_json:"[]".into(), terminal_phase:"Done".into(), restorable:false, updated_at_ns:42,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    storage: Storage,
    runtime: AgentRuntime,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("learning.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        storage
            .handle()
            .upsert_run_context(&record(
                "run-1",
                None,
                "source sk-abcdefghijklmnopqrstuvwxyz0123456789",
            ))
            .unwrap();
        storage
            .handle()
            .upsert_run_context(&record("run-2", Some("run-1"), "child evidence"))
            .unwrap();
        storage
            .handle()
            .upsert_run_context(&record("run-3", Some("run-2"), "grandchild evidence"))
            .unwrap();
        storage
            .handle()
            .upsert_run_context(&record("run-4", None, "unrelated secret"))
            .unwrap();
        let bus = Arc::new(event_bus::EventBus::new(128));
        let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let runtime = AgentRuntime::new(bus, executor, Arc::new(StopModel))
            .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        Self {
            _dir: dir,
            storage,
            runtime,
        }
    }
}
fn extract_config() -> RunConfig {
    RunConfig {
        learning_internal: true,
        purpose: RunPurpose::LessonExtract {
            source_run_id: RunId::new(1),
        },
        ..Default::default()
    }
}
fn inspect(run: Option<&str>) -> InspectSourceArgs {
    InspectSourceArgs {
        run_id: run.map(str::to_string),
        offset: 0,
        limit: 4,
        content_offset: 0,
    }
}

#[test]
fn learning_tools_require_internal_purpose_and_correct_role() {
    let specs: Vec<_> = crate::META_OPS
        .iter()
        .map(|name| crate::meta::tool_spec(name))
        .collect();
    for role in [
        Role::Worker,
        Role::Reviewer,
        Role::Orchestrator,
        Role::Explorer,
    ] {
        // Ordinary runs cannot gain authority through display name/category/recursion guard.
        let config = RunConfig {
            category: Some("lesson".into()),
            name: Some("learning-evidence-review".into()),
            learning_internal: true,
            ..Default::default()
        };
        let policy = ExecutionPolicy::for_role(role).for_run_config(&config);
        for tool in [
            "inspect_learning_source",
            "stack_lesson_candidate",
            "list_lesson_candidates",
            "submit_lesson_review",
        ] {
            assert!(policy.authorize(tool).is_err());
            assert!(
                !policy
                    .filter_tool_specs(specs.clone())
                    .iter()
                    .any(|spec| spec.name == tool)
            );
        }
    }
    let config = extract_config();
    let policy = ExecutionPolicy::for_role(Role::Worker).for_run_config(&config);
    assert_eq!(
        policy
            .filter_tool_specs(specs.clone())
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        ["inspect_learning_source", "stack_lesson_candidate"]
    );
    for forbidden in [
        "write",
        "shell",
        "delegate",
        "read",
        "submit_review",
        "submit_lesson_review",
    ] {
        assert!(policy.authorize(forbidden).is_err());
    }
    assert!(
        ExecutionPolicy::for_role(Role::Reviewer)
            .for_run_config(&config)
            .filter_tool_specs(specs.clone())
            .is_empty()
    );
    let mut untrusted = config;
    untrusted.learning_internal = false;
    assert!(
        ExecutionPolicy::for_role(Role::Worker)
            .for_run_config(&untrusted)
            .filter_tool_specs(specs)
            .is_empty()
    );
    let delegate = crate::meta::tool_spec("delegate");
    assert!(!delegate.input_schema.to_string().contains("lesson"));
    assert!(!delegate.input_schema.to_string().contains("purpose"));
}

#[tokio::test]
async fn scoped_snapshot_is_redacted_immutable_and_excludes_private_blocks() {
    let fixture = Fixture::new();
    let caller = RunId::new(50);
    let config = extract_config();
    let list = fixture
        .runtime
        .inspect_learning_source(caller, &config, inspect(None))
        .unwrap();
    assert_eq!(list["total"], 3);
    assert_eq!(list["runs"][2]["run_id"], "run-3");
    assert!(
        fixture
            .runtime
            .inspect_learning_source(caller, &config, inspect(Some("run-4")))
            .is_err()
    );
    let page = fixture
        .runtime
        .inspect_learning_source(caller, &config, inspect(Some("run-1")))
        .unwrap();
    let text = page.to_string();
    assert!(text.contains("REDACTED"));
    for secret in [
        "sk-abcdefghijklmnopqrstuvwxyz",
        "hidden system",
        "hidden reasoning",
        "hidden image",
    ] {
        assert!(!text.contains(secret));
    }
    assert_eq!(page["total"], 1);
    assert_eq!(page["records"][0]["reference"], "run-1@42:m1:b0");
    fixture
        .storage
        .handle()
        .upsert_run_context(&record("run-1", None, "changed after snapshot"))
        .unwrap();
    assert_eq!(
        fixture
            .runtime
            .inspect_learning_source(caller, &config, inspect(Some("run-1")))
            .unwrap(),
        page
    );
    assert!(
        fixture
            .runtime
            .inspect_learning_source(caller, &RunConfig::default(), inspect(None))
            .is_err()
    );
}

#[test]
fn source_pages_bound_unicode_and_escape_heavy_results() {
    let text = format!("{}{}", "検証".repeat(900), "\u{0001}".repeat(5000));
    let snapshot =
        SourceSnapshot::from_records(RunId::new(1), vec![record("run-1", None, &text)]).unwrap();
    let mut args = inspect(Some("run-1"));
    let mut reconstructed = String::new();
    loop {
        let (page, _) = snapshot.page(args).unwrap();
        assert!(page.to_string().len() <= MAX_TOOL_OUTPUT_BYTES);
        reconstructed.push_str(page["records"][0]["content"].as_str().unwrap());
        let Some(offset) = page["records"][0]["next_content_offset"].as_u64() else {
            break;
        };
        args = InspectSourceArgs {
            content_offset: offset as usize,
            ..inspect(Some("run-1"))
        };
    }
    assert_eq!(reconstructed, text);
    assert!(
        snapshot
            .page(InspectSourceArgs {
                limit: 0,
                ..inspect(None)
            })
            .is_err()
    );
    assert!(
        snapshot
            .page(InspectSourceArgs {
                content_offset: 1,
                ..inspect(Some("run-1"))
            })
            .is_err()
    );
    assert!(
        snapshot
            .page(InspectSourceArgs {
                offset: usize::MAX,
                ..inspect(Some("run-1"))
            })
            .is_err()
    );
}

#[tokio::test]
async fn typed_review_requires_independent_evidence_and_is_idempotent() {
    let fixture = Fixture::new();
    let config = extract_config();
    let extraction =
        fixture
            .runtime
            .delegate_background(Role::Worker, "extract".into(), config.clone());
    assert_eq!(
        fixture.runtime.wait(extraction).await.unwrap(),
        AgentRunPhase::Done
    );
    let args = || StackCandidateArgs {
        content: "Always verify child tool outcomes".into(),
        evidence_refs: vec!["run-2@42:m1:b0".into()],
    };
    assert!(
        fixture
            .runtime
            .stack_lesson_candidate(extraction, &config, args())
            .is_err()
    );
    fixture
        .runtime
        .inspect_learning_source(extraction, &config, inspect(Some("run-2")))
        .unwrap();
    let result = fixture
        .runtime
        .stack_lesson_candidate(extraction, &config, args())
        .unwrap();
    assert_eq!(
        fixture
            .runtime
            .stack_lesson_candidate(extraction, &config, args())
            .unwrap(),
        result
    );
    let candidates = fixture.runtime.learning_candidates(extraction).unwrap();
    assert_eq!(candidates.len(), 1);
    assert!(
        fixture
            .runtime
            .stack_lesson_candidate(
                extraction,
                &config,
                StackCandidateArgs {
                    content: "fabricated".into(),
                    evidence_refs: vec!["run-4@42:m1:b0".into()]
                }
            )
            .is_err()
    );
    let review_config = RunConfig {
        purpose: RunPurpose::LessonReview {
            source_run_id: RunId::new(1),
            extraction_run_id: extraction,
        },
        learning_internal: true,
        ..Default::default()
    };
    let reviewer =
        fixture
            .runtime
            .delegate_background(Role::Reviewer, "review".into(), review_config.clone());
    assert_eq!(
        fixture.runtime.wait(reviewer).await.unwrap(),
        AgentRunPhase::Done
    );
    let review = LessonReview {
        candidate_id: candidates[0].id.clone(),
        verdict: LessonVerdict::Approve,
        rationale: "Checked child evidence".into(),
        evidence_refs: candidates[0].evidence_refs.clone(),
    };
    assert!(
        fixture
            .runtime
            .submit_lesson_review(reviewer, &review_config, review.clone())
            .is_err()
    );
    fixture
        .runtime
        .list_lesson_candidates(
            reviewer,
            &review_config,
            ListCandidatesArgs {
                offset: 0,
                limit: 2,
            },
        )
        .unwrap();
    assert!(
        fixture
            .runtime
            .submit_lesson_review(reviewer, &review_config, review.clone())
            .is_err()
    );
    fixture
        .runtime
        .inspect_learning_source(reviewer, &review_config, inspect(Some("run-2")))
        .unwrap();
    fixture
        .runtime
        .submit_lesson_review(reviewer, &review_config, review.clone())
        .unwrap();
    fixture
        .runtime
        .submit_lesson_review(reviewer, &review_config, review.clone())
        .unwrap();
    assert_eq!(
        fixture.runtime.learning_reviews(reviewer).unwrap(),
        vec![review.clone()]
    );
    let mut conflicting = review;
    conflicting.verdict = LessonVerdict::Reject;
    assert!(
        fixture
            .runtime
            .submit_lesson_review(reviewer, &review_config, conflicting)
            .is_err()
    );
    assert!(
        fixture
            .runtime
            .list_lesson_candidates(
                reviewer,
                &RunConfig {
                    purpose: RunPurpose::LessonReview {
                        source_run_id: RunId::new(4),
                        extraction_run_id: extraction
                    },
                    ..review_config
                },
                ListCandidatesArgs {
                    offset: 0,
                    limit: 1
                }
            )
            .is_err()
    );
    fixture.runtime.clear_learning_run(reviewer);
    fixture.runtime.clear_learning_run(extraction);
    assert!(fixture.runtime.learning_candidates(extraction).is_err());
}

#[tokio::test]
async fn absent_or_incomplete_source_fails_closed() {
    let fixture = Fixture::new();
    let config = RunConfig {
        purpose: RunPurpose::LessonExtract {
            source_run_id: RunId::new(99),
        },
        ..extract_config()
    };
    assert!(
        fixture
            .runtime
            .inspect_learning_source(RunId::new(50), &config, inspect(None))
            .is_err()
    );
    let mut incomplete = record("run-1", None, "still running");
    incomplete.terminal_phase = "Checkpoint".into();
    fixture
        .storage
        .handle()
        .upsert_run_context(&incomplete)
        .unwrap();
    assert!(
        fixture
            .runtime
            .inspect_learning_source(RunId::new(50), &extract_config(), inspect(None))
            .is_err()
    );
    assert!(fixture.runtime.learning_candidates(RunId::new(50)).is_err());
}

#[tokio::test]
async fn oversized_descendant_tree_fails_instead_of_silently_truncating() {
    let fixture = Fixture::new();
    for id in 10..138 {
        fixture
            .storage
            .handle()
            .upsert_run_context(&record(&format!("run-{id}"), Some("run-1"), "evidence"))
            .unwrap();
    }
    let error = fixture
        .runtime
        .inspect_learning_source(RunId::new(500), &extract_config(), inspect(None))
        .unwrap_err();
    assert!(error.contains("snapshot limit"));
}
