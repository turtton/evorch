//! Crash at the responsibility boundary, with actual runtime and SQLite state.
use super::*;
use storage::handoffs::HandoffState;
use tokio::sync::Notify;

struct Model {
    worker_entered: Notify,
    target_calls: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl crate::AgentModel for Model {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "handoff-crash".into()
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        role: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        if role == Role::Worker {
            self.worker_entered.notify_one();
            std::future::pending::<()>().await;
        }
        self.target_calls.fetch_add(1, Ordering::SeqCst);
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: Default::default(),
            usage_reported: true,
        })
    }
}
fn memo(source: RunId) -> crate::EscalationMemo {
    crate::EscalationMemo {
        source_run_id: source,
        original_request: "original request and conditions".into(),
        escalation_reason: "coordinate".into(),
        findings: vec![],
        files_touched: vec![],
        blockers: vec![],
        workspace_state: String::new(),
        suggested_next: String::new(),
    }
}

#[tokio::test]
async fn missing_handoff_receipt_keeps_the_source_goal_and_execution() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("missing-receipt.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    let model = Arc::new(Model {
        worker_entered: Notify::new(),
        target_calls: Default::default(),
    });
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(crate::RunStore::open(&config, db.handle()).unwrap());
    let source = runtime
        .delegate_chat("source", Role::Worker, "work".into(), RunConfig::default())
        .unwrap();
    model.worker_entered.notified().await;
    let goal = runtime
        .create_thread_goal("source", source, "goal".into(), vec!["evidence".into()])
        .unwrap();
    let target = runtime.reserve_run_id();
    let incarnation = runtime
        .execution_snapshot(source)
        .unwrap()
        .publication()
        .incarnation();
    assert!(
        runtime
            .handoff_thread_goal_prepared(source, target, None, Some(&incarnation), |_| {})
            .is_err()
    );
    assert_eq!(runtime.thread_goal("source").unwrap().goal_id, goal.goal_id);
    assert_eq!(
        runtime.inspect_agent(source).unwrap().phase,
        AgentRunPhase::Running
    );
    assert_eq!(model.target_calls.load(Ordering::SeqCst), 0);
    runtime.cancel(source).unwrap();
    runtime.wait(source).await.unwrap();
}

#[tokio::test]
async fn committed_target_starts_with_its_prepared_permit_after_source_releases() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("released-source.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    let path = dir.path().join("owners.db");
    let mut registry = crate::ownership::Registry::open(&path).unwrap();
    let lease = crate::ownership::Lease {
        owner_id: "handoff-host".into(),
        generation: 1,
        expires_at: u64::MAX,
    };
    registry
        .start(&crate::ownership::ThreadOwner::new(
            "source".into(),
            lease.clone(),
        ))
        .unwrap();
    let permit = crate::ownership::OwnerPermit {
        registry_path: path,
        thread_id: "source".into(),
        lease: lease.clone(),
        run_id: None,
    };
    let model = Arc::new(Model {
        worker_entered: Notify::new(),
        target_calls: Default::default(),
    });
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(crate::RunStore::open(&config, db.handle()).unwrap());
    let authority = RunConfig {
        ownership: Some(permit.clone()),
        ..Default::default()
    };
    let source = runtime
        .delegate_chat("source", Role::Worker, "work".into(), authority.clone())
        .unwrap();
    model.worker_entered.notified().await;
    let publication = runtime.execution_snapshot(source).unwrap().publication();
    let target = runtime
        .prepare_handoff_to_lane(&memo(source), &authority, Default::default(), &publication)
        .unwrap();
    runtime.stop(source, crate::StopScope::SelfOnly).unwrap();
    assert_eq!(runtime.wait(source).await.unwrap(), AgentRunPhase::Stopped);
    registry
        .update("source", |owner| owner.quiesce(&lease))
        .unwrap();
    registry
        .update("source", |owner| owner.release(&lease))
        .unwrap();
    assert!(permit.validate_generation().is_err());
    assert_eq!(
        runtime
            .start_committed_handoff(source, &publication.incarnation(), &authority)
            .unwrap(),
        target
    );
    assert_eq!(runtime.wait(target).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(model.target_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        db.handle()
            .handoff_for_target(target.to_string())
            .unwrap()
            .unwrap()
            .state,
        HandoffState::Started
    );
}

#[tokio::test]
async fn committed_before_source_terminal_recovers_the_same_target_and_goal() {
    for phase in [
        event_bus::ThreadGoalPhase::Blocked,
        event_bus::ThreadGoalPhase::Complete,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("handoff-crash.db"),
            ..Default::default()
        };
        let db = storage::Storage::open(config.clone()).unwrap();
        let model = Arc::new(Model {
            worker_entered: Notify::new(),
            target_calls: Default::default(),
        });
        let make = || {
            let bus = Arc::new(EventBus::new(256));
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(crate::RunStore::open(&config, db.handle()).unwrap())
        };
        let runtime = make();
        let source_config = RunConfig {
            budget: crate::budget_tracker::BudgetSettings {
                max_tokens: crate::budget_tracker::ResourceLimit::Bounded(10_000),
                max_file_rereads: 7,
                ..Default::default()
            },
            ..Default::default()
        };
        let source = runtime
            .delegate_chat(
                "source",
                Role::Worker,
                "original request".into(),
                source_config.clone(),
            )
            .unwrap();
        model.worker_entered.notified().await;
        let mut goal = runtime
            .create_thread_goal("source", source, "goal".into(), vec!["evidence".into()])
            .unwrap();
        goal.phase = phase;
        goal.checks = vec![event_bus::ThreadGoalCheck {
            criterion: 0,
            met: true,
            evidence: "obsolete root review".into(),
        }];
        goal.checks_paused = true;
        goal.usage.input_tokens = 111;
        goal.usage.output_tokens = 222;
        goal.max_tokens = Some(50_000);
        goal.reason = Some("explicitly blocked".into());
        runtime.restore_thread_goal(goal.clone()).unwrap();
        let question = runtime
            .request_user_question(source, "same question ID".into(), vec![], false)
            .unwrap();
        let publication = runtime.execution_snapshot(source).unwrap().publication();
        let target = runtime
            .prepare_handoff_to_lane(
                &memo(source),
                &source_config,
                Default::default(),
                &publication,
            )
            .unwrap();
        assert_eq!(
            runtime.inspect_agent(source).unwrap().phase,
            AgentRunPhase::Running
        );
        assert_eq!(model.target_calls.load(Ordering::SeqCst), 0);
        let receipt = db
            .handle()
            .handoff_for_source(source.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(receipt.state, HandoffState::Committed);
        assert_eq!(receipt.target_run_id, target.to_string());
        // No second target or provider request on an idempotent replay.
        assert_eq!(
            runtime
                .prepare_handoff_to_lane(
                    &memo(source),
                    &source_config,
                    Default::default(),
                    &publication
                )
                .unwrap(),
            target
        );
        assert!(runtime.recover_unstarted_lanes(None).is_err());
        assert_eq!(model.target_calls.load(Ordering::SeqCst), 0);

        // Abort the actual task without finalize: exactly the crash window after
        // commit and before the source terminal was published.
        let task = lock_runs(&runtime.shared.runs)
            .get_mut(&source)
            .unwrap()
            ._join
            .take()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        drop(runtime);
        let runtime = make();
        assert!(
            runtime.resume_chat(source, RunConfig::default()).is_err(),
            "a crash must not revive the committed source"
        );
        assert_eq!(runtime.recover_unstarted_lanes(None).unwrap(), vec![target]);
        assert_eq!(runtime.wait(target).await.unwrap(), AgentRunPhase::Done);
        assert_eq!(model.target_calls.load(Ordering::SeqCst), 1);
        // Lane roots deliberately normalize quantity caps to Unlimited. The
        // source's runaway guards still survive the durable handoff boundary.
        assert_eq!(
            runtime.entry(target).unwrap().config.budget,
            source_config.budget.clone().with_unlimited_resources()
        );
        let thread = event_bus::escalation_thread_id(&target.to_string());
        let restored = runtime.thread_goal(&thread).unwrap();
        assert_eq!(
            restored.phase,
            if phase == event_bus::ThreadGoalPhase::Blocked {
                phase
            } else {
                event_bus::ThreadGoalPhase::Working
            }
        );
        assert!(restored.checks_paused);
        assert_eq!(restored.usage.input_tokens, 111);
        assert_eq!(restored.usage.output_tokens, 222);
        assert_eq!(restored.max_tokens, goal.max_tokens);
        assert_eq!(restored.original_request, goal.original_request);
        assert!(restored.checks.is_empty());
        assert_eq!(runtime.user_answers(target).unwrap()[0].id, question.id);
        let receipt = db
            .handle()
            .handoff_for_source(source.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(receipt.state, HandoffState::Started);
        let lane = db.handle().lane(&receipt.lane_id).unwrap().unwrap();
        assert_eq!(lane.lead_thread_id, None);
        assert_eq!(lane.policy, event_bus::LanePolicy::Detached);
        assert_eq!(lane.scope, event_bus::LaneScope::Lane);
        assert!(runtime.recover_unstarted_lanes(None).unwrap().is_empty());
    }
}

#[tokio::test]
async fn deleted_requester_answers_require_a_matching_committed_receipt() {
    for fault in ["missing", "wrong_target", "prepared", "committed"] {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("questions.db"),
            ..Default::default()
        };
        let db = storage::Storage::open(config.clone()).unwrap();
        let model = Arc::new(Model {
            worker_entered: Notify::new(),
            target_calls: Default::default(),
        });
        let bus = Arc::new(EventBus::new(256));
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(crate::RunStore::open(&config, db.handle()).unwrap());
        let source = runtime
            .delegate_chat(
                "source",
                Role::Worker,
                "request".into(),
                RunConfig::default(),
            )
            .unwrap();
        model.worker_entered.notified().await;
        let question = runtime
            .request_user_question(source, "decision".into(), vec![], false)
            .unwrap();
        let publication = runtime.execution_snapshot(source).unwrap().publication();
        let target = runtime
            .prepare_handoff_to_lane(
                &memo(source),
                &RunConfig::default(),
                Default::default(),
                &publication,
            )
            .unwrap();
        let conn = rusqlite::Connection::open(&config.db_path).unwrap();
        match fault {
            "missing" => {
                conn.execute("DELETE FROM lane_handoffs", []).unwrap();
            }
            "wrong_target" => {
                conn.execute(
                    "UPDATE lane_handoffs SET target_run_id='another-target'",
                    [],
                )
                .unwrap();
            }
            "prepared" => {
                conn.execute_batch("UPDATE lane_handoffs SET state='prepared';UPDATE lane_handoff_questions SET committed=0;").unwrap();
            }
            _ => {}
        }
        runtime
            .set_tombstone_guard(crate::thread_retention::TombstoneGuard::new(
                storage::thread_retention::TombstoneSet {
                    runs: [source.to_string()].into(),
                    ..Default::default()
                },
            ))
            .unwrap();
        let answered = runtime.answer_user_question(&question.id, "user choice");
        assert_eq!(
            answered.is_ok(),
            fault == "committed",
            "{fault}: {answered:?}"
        );
        let saved = storage::Database::open(&config)
            .unwrap()
            .user_question(&question.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.answer.as_deref(),
            if fault == "committed" {
                Some("user choice")
            } else {
                None
            }
        );
        assert_eq!(saved.run_id, source.to_string());
        assert!(saved.recipient_run_ids.contains(&target.to_string()));
        let task = lock_runs(&runtime.shared.runs)
            .get_mut(&source)
            .unwrap()
            ._join
            .take()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
}
struct RepeatedHandoffModel {
    entered: Notify,
    release: Notify,
    worker_calls: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl crate::AgentModel for RepeatedHandoffModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "repeated-handoff".into()
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        role: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        let (content, finish_reason) = if role == Role::Worker {
            let call = self.worker_calls.fetch_add(1, Ordering::SeqCst);
            if call.is_multiple_of(2) {
                self.entered.notify_one();
                self.release.notified().await;
                (
                    providers::ContentBlock::ToolUse {
                        id: format!("question-{call}"),
                        name: "ask_user".into(),
                        input: serde_json::json!({"title":format!("decision-{call}"),"blocking":false}),
                    },
                    providers::FinishReason::ToolUse,
                )
            } else {
                (
                    providers::ContentBlock::ToolUse {
                        id: format!("handoff-{call}"),
                        name: "escalate".into(),
                        input: serde_json::json!({"original_request":"current work","escalation_reason":"coordinate"}),
                    },
                    providers::FinishReason::ToolUse,
                )
            }
        } else {
            (
                providers::ContentBlock::Text {
                    text: "target done".into(),
                },
                providers::FinishReason::Stop,
            )
        };
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![content],
            },
            finish_reason,
            usage: Default::default(),
            usage_reported: true,
        })
    }
}
#[tokio::test]
async fn explicit_source_followup_gets_a_new_operation_without_taking_old_goal_or_question() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("repeated.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    let model = Arc::new(RepeatedHandoffModel {
        entered: Notify::new(),
        release: Notify::new(),
        worker_calls: Default::default(),
    });
    let bus = Arc::new(EventBus::new(256));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(crate::RunStore::open(&config, db.handle()).unwrap());
    let source = runtime
        .delegate_chat(
            "source",
            Role::Worker,
            "first work".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    model.entered.notified().await;
    let mut first_goal = runtime
        .create_thread_goal(
            "source",
            source,
            "first goal".into(),
            vec!["evidence".into()],
        )
        .unwrap();
    first_goal.checks_paused = true;
    runtime.restore_thread_goal(first_goal.clone()).unwrap();
    model.release.notify_one();
    assert_eq!(runtime.wait(source).await.unwrap(), AgentRunPhase::Done);
    let first = db
        .handle()
        .handoff_for_source(source.to_string())
        .unwrap()
        .unwrap();
    let first_target: RunId = first.target_run_id.parse().unwrap();
    waiting(&runtime, first_target).await;
    let first_question = runtime.user_answers(first_target).unwrap()[0].id.clone();
    assert!(runtime.user_answers(source).unwrap().is_empty());
    assert!(runtime.resume_chat(source, RunConfig::default()).is_err());
    runtime
        .continue_goal(source, "new human work".into(), RunConfig::default())
        .unwrap();
    model.entered.notified().await;
    let mut second_goal = runtime
        .create_thread_goal(
            "source",
            source,
            "second goal".into(),
            vec!["new evidence".into()],
        )
        .unwrap();
    assert_ne!(first_goal.goal_id, second_goal.goal_id);
    second_goal.checks_paused = true;
    runtime.restore_thread_goal(second_goal.clone()).unwrap();
    // A wake may observe an already-live, explicitly submitted new incarnation,
    // but cannot revive the committed original incarnation above.
    assert_eq!(
        runtime.resume_chat(source, RunConfig::default()).unwrap(),
        source
    );
    model.release.notify_one();
    assert_eq!(runtime.wait(source).await.unwrap(), AgentRunPhase::Done);
    let second = db
        .handle()
        .handoff_for_source(source.to_string())
        .unwrap()
        .unwrap();
    let second_target: RunId = second.target_run_id.parse().unwrap();
    assert_ne!(first_target, second_target);
    assert_ne!(first.source_incarnation, second.source_incarnation);
    waiting(&runtime, second_target).await;
    assert_eq!(
        db.handle()
            .handoff_operation(source.to_string(), first.source_incarnation)
            .unwrap()
            .unwrap()
            .target_run_id,
        first.target_run_id
    );
    assert_eq!(
        runtime
            .thread_goal(&event_bus::escalation_thread_id(&first.target_run_id))
            .unwrap()
            .goal_id,
        first_goal.goal_id
    );
    assert_eq!(
        runtime
            .thread_goal(&event_bus::escalation_thread_id(&second.target_run_id))
            .unwrap()
            .goal_id,
        second_goal.goal_id
    );
    let questions = runtime.user_answers(second_target).unwrap();
    assert_eq!(questions.len(), 1);
    assert_ne!(questions[0].id, first_question);
    runtime
        .answer_user_question(&first_question, "old target decision")
        .unwrap();
    assert!(matches!(
        runtime
            .answer_user_question_if_pending(&first_question, "duplicate")
            .unwrap(),
        crate::AnswerOutcome::AlreadyAnswered
    ));
    assert!(runtime.user_answers(source).unwrap().is_empty());
    assert_eq!(
        runtime.user_answers(first_target).unwrap()[0]
            .answer
            .as_deref(),
        Some("old target decision")
    );
    assert!(
        runtime.user_answers(second_target).unwrap()[0]
            .answer
            .is_none()
    );
}

async fn waiting(runtime: &AgentRuntime, run: RunId) {
    runtime.wait_admission(run).await.unwrap();
    let mut rx = runtime.entry(run).unwrap().phase_rx.clone();
    loop {
        let phase = *rx.borrow_and_update();
        if phase == AgentRunPhase::Waiting {
            return;
        }
        assert!(
            !matches!(
                phase,
                AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
            ),
            "target ended before waiting: {phase:?}"
        );
        rx.changed().await.unwrap();
    }
}
