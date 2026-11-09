//! Handoff is a new run, not a history replacement on either existing root.
use super::*;

#[tokio::test]
async fn handoff_and_inherited_answer_preserve_source_target_and_unrelated_lead_prefixes() {
    let mut h = harness(vec![text_response("lead idle")], 1_000_000);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("handoff-cache.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let interactive = RunConfig {
        conversation: true,
        interactive: true,
        keep_alive: true,
        ..Default::default()
    };
    let lead = h
        .runtime
        .delegate_chat(
            "unrelated-lead",
            Role::Orchestrator,
            "LEAD-CACHE".into(),
            interactive.clone(),
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    h.mock.enqueue([
        call(
            "inherited-question",
            "ask_user",
            json!({"title":"handoff decision","blocking":false}),
        ),
        text_response("source idle"),
    ]);
    let source = h
        .runtime
        .delegate_chat("source", Role::Worker, "SOURCE-CACHE".into(), interactive)
        .unwrap();
    events.extend(through_phase(&mut h.receiver, source, AgentRunPhase::Waiting).await);
    let question = result(&h, "inherited-question")["question_id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.mock.enqueue([
        call("handoff", "handoff_to_lane", json!({"original_request":"SOURCE-CACHE","escalation_reason":"coordinate the original request"})),
        text_response("target idle"),
    ]);
    h.runtime
        .send_message(source, "handoff without resetting the source prefix".into())
        .unwrap();
    let target = loop {
        let event = h.receiver.recv().await.unwrap();
        let target = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                source_run_id,
                new_run_id,
                ..
            }) if source_run_id == &source.to_string() => {
                Some(new_run_id.parse::<RunId>().unwrap())
            }
            _ => None,
        };
        events.push(event);
        if let Some(target) = target {
            break target;
        }
    };
    assert_ne!(target, source);
    events.extend(through_phase(&mut h.receiver, target, AgentRunPhase::Waiting).await);
    let inherited = h.runtime.user_answers(target).unwrap();
    assert_eq!(inherited[0].id, question);
    assert_eq!(inherited[0].run_id, source.to_string());
    reclaim_source_family(&config, &db, source, target);
    // The recipient guard, rather than a family relationship, keeps this ID
    // answerable after the requester's thread and context have been reclaimed.
    assert_eq!(h.runtime.user_answers(target).unwrap()[0].id, question);
    h.mock.enqueue([text_response("answer observed")]);
    h.runtime
        .answer_user_question(&question, "retain the agreed conditions")
        .unwrap();
    events.extend(through_phase(&mut h.receiver, target, AgentRunPhase::Waiting).await);
    input(
        &mut h,
        lead,
        "unrelated lead continues",
        vec![text_response("lead still idle")],
        &mut events,
    )
    .await;
    input(
        &mut h,
        target,
        "continue the new run",
        vec![text_response("target still idle")],
        &mut events,
    )
    .await;
    let requests: Vec<_> = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == Wire::ChatCompletions.path())
        .map(|r| r.body)
        .collect();
    let first_user = |r: &Value| {
        r["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "user")
            .unwrap()["content"]
            .to_string()
    };
    let groups = [
        (
            lead,
            requests
                .iter()
                .filter(|r| first_user(r).contains("LEAD-CACHE"))
                .collect::<Vec<_>>(),
        ),
        (
            source,
            requests
                .iter()
                .filter(|r| {
                    first_user(r).contains("SOURCE-CACHE")
                        && !first_user(r).contains("[evorch escalation")
                })
                .collect(),
        ),
        (
            target,
            requests
                .iter()
                .filter(|r| first_user(r).contains("[evorch escalation"))
                .collect(),
        ),
    ];
    for (run, group) in groups {
        assert!(group.len() >= 2, "run {run} must exercise reuse");
        for pair in group.windows(2) {
            assert_append_only(CacheProtocol::OpenAi, pair[0], pair[1]).unwrap();
        }
        let usage: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    run_id: Some(id),
                    input_tokens,
                    cache_read_tokens,
                    ..
                }) if id == &run.to_string() => Some((*input_tokens, *cache_read_tokens)),
                EventKind::Diagnostic(d) if d.code == "CacheRegression" => panic!("{d:?}"),
                _ => None,
            })
            .collect();
        assert_eq!(usage.len(), group.len());
        for pair in usage.windows(2) {
            assert!(pair[1].1 >= pair[0].0);
        }
    }
    let lane = db
        .handle()
        .lane_by_thread(&event_bus::escalation_thread_id(&target.to_string()))
        .unwrap()
        .unwrap();
    assert_eq!(lane.policy, event_bus::LanePolicy::Detached);
    assert_eq!(lane.lead_thread_id, None);
    assert_eq!(
        h.runtime.inspect_agent(source).unwrap().phase,
        AgentRunPhase::Done
    );
    assert_eq!(h.mock.remaining_scripts(), 0);
    for run in [lead, target] {
        h.runtime.cancel(run).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
}

/// Only the admission boundary is faulted; provider requests still go through
/// the production routing client and the input-derived HTTP cache mock.
struct RejectFirstTarget {
    inner: Arc<dyn runtime::AgentModel>,
    reject: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl runtime::AgentModel for RejectFirstTarget {
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: Role,
    ) -> Result<(), runtime::RuntimeError> {
        if role == Role::Orchestrator
            && self.reject.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(runtime::RuntimeError::Model {
                reason: "injected handoff admission failure".into(),
            });
        }
        self.inner.admit(invocation, role).await
    }
    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        self.inner.selected_model(role, category)
    }
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.inner.complete(invocation, role, messages, tools).await
    }
}
#[tokio::test]
async fn failed_target_admission_recovers_its_seed_once_then_reuses_its_own_wire_prefix() {
    let mut h = harness(
        vec![
            call(
                "recover-question",
                "ask_user",
                json!({"title":"retain this ID","blocking":false}),
            ),
            call(
                "recover-handoff",
                "handoff_to_lane",
                json!({"original_request":"recover the same target","escalation_reason":"coordination"}),
            ),
        ],
        1_000_000,
    );
    let config = Config::load(&LoadOptions {
        project_dir: Some(h._directory.path().into()),
        user_config_dir: Some(h._directory.path().join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(512));
    h.receiver = bus.subscribe();
    let model = runtime::compose::compose_routed_model(
        &config,
        routing::ComposeDeps {
            credential_store: Arc::new(
                FileCredentialStore::open(h._directory.path().join("recovery-credentials"))
                    .unwrap(),
            ),
            event_bus: Some(bus.clone()),
            env: Arc::new(MapEnv::from_iter([(KEY_ENV, "offline-test-key")])),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions::default(),
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap();
    let model = Arc::new(RejectFirstTarget {
        inner: model,
        reject: std::sync::atomic::AtomicBool::new(true),
    });
    let storage_config = storage::StorageConfig {
        db_path: h._directory.path().join("handoff-recovery.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(storage_config.clone()).unwrap();
    let make = || {
        AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus.clone())),
            model.clone(),
        )
        .with_system_prompts(contract_prompts())
        .with_compaction(config.compaction.clone())
        .with_run_store(runtime::RunStore::open(&storage_config, db.handle()).unwrap())
    };
    h.runtime = make();
    let source = h
        .runtime
        .delegate_chat(
            "source",
            Role::Worker,
            "RECOVER-SOURCE".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, source, AgentRunPhase::Done).await;
    let target: RunId = db
        .handle()
        .handoff_for_source(source.to_string())
        .unwrap()
        .unwrap()
        .target_run_id
        .parse()
        .unwrap();
    while !events.iter().any(|e|matches!(&e.kind,EventKind::Diagnostic(d) if d.code=="EscalationAdmissionFailed" && d.run_id.as_deref()==Some(&target.to_string()))) {
        events.push(h.receiver.recv().await.unwrap());
    }
    let receipt = db
        .handle()
        .handoff_for_source(source.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt.state,
        storage::handoffs::HandoffState::RecoverableStartFailure
    );
    let question = storage::Database::open(&storage_config)
        .unwrap()
        .user_questions_for_run(&source.to_string())
        .unwrap()[0]
        .id
        .clone();
    assert!(
        h.runtime.resume_chat(source, RunConfig::default()).is_err(),
        "the committed source must not reclaim responsibility"
    );
    // Simulated process restart: no provider input was sent for the failed
    // target, so recovery is its first request, not a reset of the source.
    h.runtime = make();
    h.mock.enqueue([text_response("recovered target idle")]);
    assert_eq!(
        h.runtime.recover_unstarted_lanes(None).unwrap(),
        vec![target]
    );
    events.extend(through_phase(&mut h.receiver, target, AgentRunPhase::Waiting).await);
    assert_eq!(h.runtime.user_answers(target).unwrap()[0].id, question);
    input(
        &mut h,
        target,
        "continue recovered target",
        vec![text_response("reuse recovered prefix")],
        &mut events,
    )
    .await;
    assert!(h.runtime.recover_unstarted_lanes(None).unwrap().is_empty());
    let requests: Vec<_> = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == Wire::ChatCompletions.path())
        .map(|r| r.body)
        .collect();
    assert_eq!(requests.len(), 4);
    assert_append_only(CacheProtocol::OpenAi, &requests[0], &requests[1]).unwrap();
    assert_append_only(CacheProtocol::OpenAi, &requests[2], &requests[3]).unwrap();
    assert_ne!(
        requests[0]["messages"][0], requests[2]["messages"][0],
        "Worker and Orchestrator have independent instructions"
    );
    let usage: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) if id == &target.to_string() => Some((*input_tokens, *cache_read_tokens)),
            EventKind::Diagnostic(d) if d.code == "CacheRegression" => panic!("{d:?}"),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 2);
    assert!(usage[0].0 > 0 && usage[1].1 >= usage[0].0);
    h.runtime.cancel(target).unwrap();
    h.runtime.wait(target).await.unwrap();
}

fn reclaim_source_family(
    config: &storage::StorageConfig,
    storage: &storage::Storage,
    source: RunId,
    target: RunId,
) {
    use storage::thread_retention::*;
    storage
        .handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "source".into(),
            archived: Some(true),
            archived_at_ns: Some(1),
            archive_revision: 1,
            ..Default::default()
        }])
        .unwrap();
    let target_thread = event_bus::escalation_thread_id(&target.to_string());
    let input = RetentionInput {
        threads: [
            ("source", true),
            (target_thread.as_str(), false),
            ("unrelated-lead", false),
        ]
        .into_iter()
        .map(|(id, archived)| SidebarThread {
            thread_id: id.into(),
            archived,
            pinned: false,
            archived_at_s: archived.then_some(0),
            created_at_s: 1,
            updated_at_s: 1,
            archive_revision: Some(u64::from(archived)),
            pin_revision: Some(0),
        })
        .collect(),
        sidebar_path: "/handoff-test/sidebar.json".into(),
        ownership_root: "/handoff-test/owners".into(),
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
            .all(|(_, state)| !matches!(state, FamilyState::Exploring { .. }))
        {
            break;
        }
    }
    let outcome = database
        .retention_finalize(
            "source",
            &input,
            &policy,
            &std::collections::BTreeMap::new(),
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
        panic!("source retention not confirmed: {outcome:?}");
    };
    assert_eq!(family.threads, vec!["source".to_owned()]);
    assert_eq!(family.runs, vec![source.to_string()]);
    for _ in 0..100 {
        let progress = storage
            .handle()
            .reclaim_step(ReclaimBudget::default())
            .unwrap();
        assert_eq!(progress.last_error, None);
        if progress.jobs_db_pending == 0 {
            break;
        }
    }
    assert_eq!(database.reclaim_summary().unwrap().jobs_db_pending, 0);
    assert!(database.run_context(&source.to_string()).unwrap().is_none());
    assert!(database.thread(&target_thread).unwrap().is_some());
    assert!(
        storage
            .handle()
            .handoff_for_target(target.to_string())
            .unwrap()
            .is_some()
    );
}
