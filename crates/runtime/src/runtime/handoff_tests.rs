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

fn runtime_for_handoff_restart(
    config: &storage::StorageConfig,
    db: &storage::Storage,
    model: Arc<RepeatedHandoffModel>,
) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(256));
    AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
        .with_run_store(crate::RunStore::open(config, db.handle()).unwrap())
}

async fn started_handoff_for_retention() -> (
    tempfile::TempDir,
    storage::StorageConfig,
    storage::Storage,
    Arc<RepeatedHandoffModel>,
    AgentRuntime,
    RunId,
    RunId,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("handoff-retention.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    let model = Arc::new(RepeatedHandoffModel {
        entered: Notify::new(),
        release: Notify::new(),
        worker_calls: Default::default(),
    });
    let runtime = runtime_for_handoff_restart(&config, &db, model.clone());
    let source = runtime
        .delegate_chat(
            "source",
            Role::Worker,
            "Original request; do not publish without approval".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    model.entered.notified().await;
    let mut goal = runtime
        .create_thread_goal(
            "source",
            source,
            "original goal".into(),
            vec!["evidence".into()],
        )
        .unwrap();
    goal.checks_paused = true;
    runtime.restore_thread_goal(goal).unwrap();
    model.release.notify_one();
    assert_eq!(runtime.wait(source).await.unwrap(), AgentRunPhase::Done);
    let receipt = db
        .handle()
        .handoff_for_source(source.to_string())
        .unwrap()
        .unwrap();
    let target = receipt.target_run_id.parse().unwrap();
    waiting(&runtime, target).await;
    runtime.stop(target, StopScope::SelfOnly).unwrap();
    runtime.wait(target).await.unwrap();
    (dir, config, db, model, runtime, source, target)
}

fn reclaim_handoff_thread(
    config: &storage::StorageConfig,
    db: &storage::Storage,
    source: RunId,
    target: RunId,
    delete_source: bool,
) {
    use storage::thread_retention::*;
    let target_thread = event_bus::escalation_thread_id(&target.to_string());
    let deleted = if delete_source {
        "source"
    } else {
        &target_thread
    };
    db.handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: deleted.into(),
            archived: Some(true),
            archived_at_ns: Some(1),
            archive_revision: 1,
            ..Default::default()
        }])
        .unwrap();
    let input = RetentionInput {
        threads: ["source", target_thread.as_str()]
            .into_iter()
            .map(|id| SidebarThread {
                thread_id: id.into(),
                archived: id == deleted,
                pinned: false,
                archived_at_s: (id == deleted).then_some(0),
                created_at_s: 1,
                updated_at_s: 1,
                archive_revision: Some(u64::from(id == deleted)),
                pin_revision: Some(0),
            })
            .collect(),
        sidebar_path: "/handoff-retention/sidebar.json".into(),
        ownership_root: "/handoff-retention/owners".into(),
        now_ns: i64::MAX / 2,
    };
    let database = storage::Database::open(config).unwrap();
    let policy = ThreadRetentionPolicy::default();
    for _ in 0..1000 {
        let states = database
            .retention_explore(
                &input,
                &policy,
                ExploreBudget {
                    max_edges: 1_000_000,
                },
            )
            .unwrap();
        if states
            .iter()
            .all(|(_, s)| !matches!(s, FamilyState::Exploring { .. }))
        {
            break;
        }
    }
    let outcome = database
        .retention_finalize(
            deleted,
            &input,
            &policy,
            &Default::default(),
            &BindingInput {
                ownership_root: input.ownership_root.clone(),
                sidebar_path: input.sidebar_path.clone(),
            },
            FinalizeBudget {
                max_edges: 1_000_000,
            },
        )
        .unwrap();
    let FinalizeOutcome::Confirmed(family) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(family.threads, [deleted]);
    assert_eq!(
        family.runs,
        [if delete_source { source } else { target }.to_string()]
    );
    for _ in 0..100 {
        let progress = db.handle().reclaim_step(ReclaimBudget::default()).unwrap();
        assert_eq!(progress.last_error, None);
        if progress.jobs_db_pending == 0 {
            break;
        }
    }
    assert_eq!(database.reclaim_summary().unwrap().jobs_db_pending, 0);
}

#[tokio::test]
async fn started_target_restores_latest_goal_after_source_reclaim_without_replaying_seed() {
    let (_dir, config, db, model, runtime, source, target) = started_handoff_for_retention().await;
    let thread = event_bus::escalation_thread_id(&target.to_string());
    let mut latest = runtime.thread_goal(&thread).unwrap();
    latest.phase = event_bus::ThreadGoalPhase::Blocked;
    latest.reason = Some("new blocker found after target started".into());
    latest.checks_paused = false;
    latest.usage.input_tokens = 9123;
    latest.usage.output_tokens = 4567;
    latest.max_tokens = Some(123_456);
    runtime.restore_thread_goal(latest.clone()).unwrap();
    db.handle()
        .append_event(
            None,
            &Event::new(event_bus::OrchestratorEvent::ThreadGoalUpdated {
                snapshot: latest.clone(),
            }),
        )
        .unwrap();
    reclaim_handoff_thread(&config, &db, source, target, true);
    drop(runtime);

    let runtime = runtime_for_handoff_restart(&config, &db, model);
    runtime.restore_committed_handoff(target).unwrap();
    assert!(
        runtime.thread_goal(&thread).is_none(),
        "Started receipt is not a current goal snapshot"
    );
    let saved = storage::Database::open(&config)
        .unwrap()
        .orchestrator_events_ordered()
        .unwrap()
        .into_iter()
        .filter_map(|row| match row.event.kind {
            event_bus::EventKind::Orchestrator(
                event_bus::OrchestratorEvent::ThreadGoalUpdated { snapshot },
            ) if snapshot.thread_id == thread => Some(snapshot),
            _ => None,
        })
        .next_back()
        .unwrap();
    let mut invalid = saved.clone();
    invalid
        .related_root_run_ids
        .push("run-unproven-deleted".into());
    assert!(
        runtime
            .set_tombstone_guard(crate::thread_retention::tombstones_for_tests(
                &["source"],
                &[&source.to_string(), "run-unproven-deleted"],
            ))
            .is_ok()
    );
    assert!(
        runtime.restore_thread_goal(invalid).is_err(),
        "only receipt-proven provenance may outlive its source"
    );
    runtime.restore_thread_goal(saved).unwrap();
    runtime.resume_chat(target, RunConfig::default()).unwrap();
    waiting(&runtime, target).await;
    let restored = runtime.thread_goal(&thread).unwrap();
    assert_eq!(restored.phase, latest.phase);
    assert_eq!(restored.reason, latest.reason);
    assert_eq!(restored.checks_paused, latest.checks_paused);
    assert_eq!(restored.usage, latest.usage);
    assert_eq!(restored.max_tokens, latest.max_tokens);
    assert_eq!(restored.original_request, latest.original_request);
    runtime.stop(target, StopScope::SelfOnly).unwrap();
    runtime.wait(target).await.unwrap();
}

#[tokio::test]
async fn reclaimed_target_keeps_source_transfer_fence_but_allows_explicit_human_followup() {
    let (_dir, config, db, model, runtime, source, target) = started_handoff_for_retention().await;
    reclaim_handoff_thread(&config, &db, source, target, false);
    assert!(
        db.handle()
            .lane_by_thread(&event_bus::escalation_thread_id(&target.to_string()))
            .unwrap()
            .is_none()
    );
    let receipt = db
        .handle()
        .handoff_for_source(source.to_string())
        .unwrap()
        .unwrap();
    assert!(receipt.state.is_committed());
    drop(runtime);
    let runtime = runtime_for_handoff_restart(&config, &db, model.clone());
    assert!(runtime.wake_chat(source, RunConfig::default()).is_err());
    assert!(runtime.resume_chat(source, RunConfig::default()).is_err());
    assert!(runtime.inspect_agent(source).is_err());
    runtime
        .continue_goal(
            source,
            "A new explicit human request".into(),
            RunConfig::default(),
        )
        .unwrap();
    model.entered.notified().await;
    assert_ne!(
        runtime
            .execution_snapshot(source)
            .unwrap()
            .publication()
            .incarnation(),
        receipt.source_incarnation
    );
    runtime.stop(source, StopScope::SelfOnly).unwrap();
    runtime.wait(source).await.unwrap();
}

#[tokio::test]
async fn handoff_copies_authentic_question_answers_without_sharing_review_authority() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("handoff-evidence.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    let model = Arc::new(Model {
        worker_entered: Notify::new(),
        target_calls: Default::default(),
    });
    let bus = Arc::new(EventBus::new(256));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(crate::RunStore::open(&config, db.handle()).unwrap());
    let source = runtime
        .delegate_chat(
            "source",
            Role::Worker,
            "Investigate only; do not publish".into(),
            RunConfig::default(),
        )
        .unwrap();
    model.worker_entered.notified().await;
    let question = runtime
        .request_user_question(source, "May I inspect the output?".into(), vec![], false)
        .unwrap();
    runtime
        .answer_user_question(&question.id, "Read only the failing output")
        .unwrap();
    // Creation time deliberately disagrees with acceptance order. The most
    // recently accepted attachment must win, not the last evidence-row sort key.
    for (prompt, body, created) in [
        ("skill-first", "older accepted body", 200),
        ("skill-last", "current accepted skill body", 100),
    ] {
        let mut row = crate::run_store::evidence_row_with_attachments(
            &source.to_string(),
            &source.to_string(),
            "host_chat",
            &format!("Use @workflow ({prompt})"),
            vec![storage::EvidenceAttachment {
                name: "workflow".into(),
                scope: "user".into(),
                body: body.into(),
            }],
        );
        row.created_at_ns = created;
        db.handle()
            .append_host_input_accepted(
                std::slice::from_ref(&row),
                &[],
                None,
                &storage::HostInputReceipt {
                    provenance: storage::host_input::HostInputProvenance::User,
                    prompt_id: prompt.into(),
                    run_id: source.to_string(),
                    root_run_id: source.to_string(),
                    text: row.text.clone(),
                    image_count: 0,
                    status: storage::host_input::HostInputStatus::Delivered,
                    reason: None,
                    accepted_at_ns: created,
                    status_at_ns: created,
                    evidence_id: Some(row.id.clone()),
                },
            )
            .unwrap();
    }
    let store = runtime.shared.run_store.get().unwrap();
    let before = store
        .review_evidence_for_lineage(&[source.to_string()])
        .unwrap();
    assert!(
        before
            .iter()
            .any(|r| r.text == "Investigate only; do not publish")
    );
    assert!(before.iter().any(
        |r| r.in_reply_to_question_id.as_deref() == Some(&question.id)
            && r.text == "Read only the failing output"
    ));
    let publication = runtime.execution_snapshot(source).unwrap().publication();
    let target = runtime
        .prepare_handoff_to_lane(
            &memo(source),
            &RunConfig::default(),
            Default::default(),
            &publication,
        )
        .unwrap();
    let copied = store
        .review_evidence_for_lineage(&[target.to_string()])
        .unwrap();
    let expected: Vec<_> = before
        .into_iter()
        .map(|mut row| {
            row.id = format!("handoff-evidence-{target}-{}", row.id);
            row.root_run_id = target.to_string();
            row.target_run_id = target.to_string();
            row
        })
        .collect();
    assert_eq!(
        copied, expected,
        "preserve kind, text, answer identity/title, references, attachments and ordering"
    );
    let context = crate::run_store::materialize_review_context(store, target).unwrap();
    assert_eq!(context.lineage_run_ids, [target.to_string()]);
    assert!(context.delegation_chain.is_empty());
    assert!(!Arc::ptr_eq(
        &context.user_requests,
        &runtime.shared.review_runs.lock().unwrap()[&source].user_requests
    ));
    assert_eq!(
        context
            .attached_requests()
            .iter()
            .map(|r| r.attachments[0].body.as_str())
            .collect::<Vec<_>>(),
        ["older accepted body", "current accepted skill body"]
    );
    // Recopying is idempotent and does not turn historical acceptances into
    // new inbox deliveries or change their order.
    db.handle()
        .copy_handoff_evidence(vec![source.to_string()], target.to_string())
        .unwrap();
    assert_eq!(
        store
            .review_attachment_acceptances_for_lineage(&[target.to_string()])
            .unwrap()
            .len(),
        2
    );
    runtime.stop(source, StopScope::SelfOnly).unwrap();
    runtime.wait(source).await.unwrap();
    reclaim_handoff_thread(&config, &db, source, target, true);
    let context = crate::run_store::materialize_review_context(store, target).unwrap();
    let (review_context, _) = crate::escalation_review::review_context(context, None).unwrap();
    let reviewer = Arc::new(crate::escalation_review::support::ScriptedModel::new([Ok(
        crate::escalation_review::support::text_response(
            r#"{"approve":true,"reason":"inspect","risk_level":"low","authorization_level":"high"}"#,
            providers::FinishReason::Stop,
        ),
    )]));
    crate::escalation_review::QuickModelReviewer::new(reviewer.clone())
        .review_with_context(
            &target.to_string(),
            "git status",
            "inspect",
            Some(review_context),
        )
        .await
        .unwrap();
    let observed = reviewer.observed().await;
    let providers::ContentBlock::Text { text } = &observed[0][1].content[0] else {
        panic!("reviewer JSON missing");
    };
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    let skills = payload["context"]["loaded_skills"].as_array().unwrap();
    let skill = skills.iter().find(|s| s["name"] == "workflow").unwrap();
    assert_eq!(skill["text"], "current accepted skill body");
    assert_eq!(skill["loaded_by"], "user_mention");
    assert_eq!(skill["trust"], "user-attached");
}

struct CrashAtHandoffStartModel {
    worker: RepeatedHandoffModel,
    target_entered: Notify,
}
#[async_trait::async_trait]
impl crate::AgentModel for CrashAtHandoffStartModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "handoff-start-crash".into()
    }
    async fn complete(
        &self,
        invocation: &crate::AgentInvocationContext,
        role: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        if role == Role::Orchestrator {
            self.target_entered.notify_one();
            std::future::pending::<()>().await;
        }
        self.worker
            .complete(invocation, role, messages, tools)
            .await
    }
}

#[tokio::test]
async fn started_target_restores_initial_control_state_when_recorder_was_stopped() {
    use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("recorder-stopped.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut recorder = OwnedStorageBridge::spawn(
        bus.clone(),
        db,
        |h| StorageBridge::new(h, "before-handoff"),
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    let handle = recorder.handle();
    let model = Arc::new(CrashAtHandoffStartModel {
        worker: RepeatedHandoffModel {
            entered: Notify::new(),
            release: Notify::new(),
            worker_calls: Default::default(),
        },
        target_entered: Notify::new(),
    });
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(crate::RunStore::open(&config, handle.clone()).unwrap());
    let source = runtime
        .delegate_chat(
            "source",
            Role::Worker,
            "Keep original restrictions".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    model.worker.entered.notified().await;
    let mut goal = runtime
        .create_thread_goal("source", source, "goal".into(), vec!["evidence".into()])
        .unwrap();
    goal.phase = event_bus::ThreadGoalPhase::Blocked;
    goal.reason = Some("blocked before handoff".into());
    goal.original_request = "Keep original restrictions".into();
    goal.checks_paused = true;
    goal.usage.input_tokens = 333;
    goal.usage.output_tokens = 777;
    goal.max_tokens = Some(654_321);
    runtime.restore_thread_goal(goal).unwrap();
    runtime
        .restore_thread_todo(event_bus::ThreadTodoSnapshot {
            list_id: "procedure".into(),
            thread_id: "source".into(),
            revision: 7,
            items: vec![event_bus::ThreadTodoItem {
                content: "retain this procedure".into(),
                status: event_bus::ThreadTodoStatus::Pending,
            }],
        })
        .unwrap();
    // Join the real recorder before preparation. No initial target event can
    // reach SQLite through EventBus, regardless of runner scheduling.
    recorder.shutdown();
    model.worker.release.notify_one();
    runtime.wait(source).await.unwrap();
    model.target_entered.notified().await;
    let receipt = handle
        .handoff_for_source(source.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(receipt.state, HandoffState::Started);
    let target: RunId = receipt.target_run_id.parse().unwrap();
    let expected_goal: event_bus::ThreadGoalSnapshot =
        serde_json::from_str(receipt.goal_json.as_ref().unwrap()).unwrap();
    let expected_todo: event_bus::ThreadTodoSnapshot =
        serde_json::from_str(receipt.todo_json.as_ref().unwrap()).unwrap();
    let task = lock_runs(&runtime.shared.runs)
        .get_mut(&target)
        .unwrap()
        ._join
        .take()
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(runtime);
    drop(handle);
    drop(recorder);
    // Reopen actual SQLite as a fresh host, using only the normal event restore
    // source. Never synthesize a goal event or replay the Started receipt.
    let db = storage::Storage::open(config.clone()).unwrap();
    let runtime = runtime_for_handoff_restart(
        &config,
        &db,
        Arc::new(RepeatedHandoffModel {
            entered: Notify::new(),
            release: Notify::new(),
            worker_calls: Default::default(),
        }),
    );
    let thread = event_bus::escalation_thread_id(&target.to_string());
    runtime.restore_committed_handoff(target).unwrap();
    assert!(runtime.thread_goal(&thread).is_none());
    let mut restored_goal = None;
    let mut restored_todo = None;
    for row in storage::Database::open(&config)
        .unwrap()
        .orchestrator_events_ordered()
        .unwrap()
    {
        match row.event.kind {
            event_bus::EventKind::Orchestrator(
                event_bus::OrchestratorEvent::ThreadGoalUpdated { snapshot },
            ) if snapshot.thread_id == thread => restored_goal = Some(snapshot),
            event_bus::EventKind::Orchestrator(
                event_bus::OrchestratorEvent::ThreadTodoUpdated { snapshot },
            ) if snapshot.thread_id == thread => restored_todo = Some(snapshot),
            _ => {}
        }
    }
    assert_eq!(restored_goal.as_ref(), Some(&expected_goal));
    assert_eq!(restored_todo.as_ref(), Some(&expected_todo));
    runtime.restore_thread_goal(restored_goal.unwrap()).unwrap();
    runtime.restore_thread_todo(restored_todo.unwrap()).unwrap();
    let goal = runtime.thread_goal(&thread).unwrap();
    assert_eq!(goal.phase, event_bus::ThreadGoalPhase::Blocked);
    assert_eq!(goal.reason.as_deref(), Some("blocked before handoff"));
    assert!(goal.checks_paused);
    assert_eq!(
        (goal.usage.input_tokens, goal.usage.output_tokens),
        (333, 777)
    );
    assert_eq!(goal.max_tokens, Some(654_321));
    assert_eq!(goal.original_request, "Keep original restrictions");
    assert_eq!(runtime.thread_todo(&thread).unwrap(), expected_todo);
}

#[tokio::test]
async fn initial_control_event_failure_rolls_back_commit_and_keeps_source_running() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("control-event-failure.db"),
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
        .delegate_chat(
            "source",
            Role::Worker,
            "Keep working on preparation failure".into(),
            RunConfig::default(),
        )
        .unwrap();
    model.worker_entered.notified().await;
    let goal = runtime
        .create_thread_goal("source", source, "goal".into(), vec!["evidence".into()])
        .unwrap();
    runtime
        .restore_thread_todo(event_bus::ThreadTodoSnapshot {
            list_id: "procedure".into(),
            thread_id: "source".into(),
            revision: 1,
            items: vec![],
        })
        .unwrap();
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    // Fail the second event, after the goal INSERT succeeded inside the same
    // transaction. Neither an initial goal nor responsibility may leak out.
    conn.execute_batch("CREATE TRIGGER fail_initial_todo BEFORE INSERT ON events WHEN NEW.subkind='ThreadTodoUpdated' BEGIN SELECT RAISE(ABORT,'initial procedure event injection'); END;").unwrap();
    let publication = runtime.execution_snapshot(source).unwrap().publication();
    assert!(
        runtime
            .prepare_handoff_to_lane(
                &memo(source),
                &RunConfig::default(),
                Default::default(),
                &publication
            )
            .is_err()
    );
    let receipt = db
        .handle()
        .handoff_for_source(source.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(receipt.state, HandoffState::Prepared);
    assert_eq!(
        runtime.inspect_agent(source).unwrap().phase,
        AgentRunPhase::Running
    );
    assert_eq!(runtime.thread_goal("source").unwrap().goal_id, goal.goal_id);
    assert_eq!(model.target_calls.load(Ordering::SeqCst), 0);
    assert!(
        storage::Database::open(&config)
            .unwrap()
            .orchestrator_events_ordered()
            .unwrap()
            .is_empty()
    );
    conn.execute_batch("DROP TRIGGER fail_initial_todo")
        .unwrap();
    let target = runtime
        .prepare_handoff_to_lane(
            &memo(source),
            &RunConfig::default(),
            Default::default(),
            &publication,
        )
        .unwrap();
    assert_eq!(target.to_string(), receipt.target_run_id);
    assert_eq!(
        db.handle()
            .handoff_for_target(target.to_string())
            .unwrap()
            .unwrap()
            .state,
        HandoffState::Committed
    );
    assert_eq!(
        storage::Database::open(&config)
            .unwrap()
            .orchestrator_events_ordered()
            .unwrap()
            .len(),
        2
    );
    runtime.stop(source, StopScope::SelfOnly).unwrap();
    runtime.wait(source).await.unwrap();
}
