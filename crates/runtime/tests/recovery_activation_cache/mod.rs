//! Real RoutedModel + HTTP recovery, independently checking the recorded wire prefix.
use super::*;
use runtime::quiesce::{RecoveryRecipe, ResumePolicy};
use runtime::{AgentModel, CurrentRecoveryAuthority};
use std::collections::BTreeMap;

fn settings(h: &Harness) -> Config {
    let root = h._directory.path();
    let mut current = Config::load(&LoadOptions {
        project_dir: Some(root.into()),
        user_config_dir: Some(root.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    current.agents.worker.base.generation.temperature = Some(0.25);
    current.agents.worker.base.generation.max_tokens = Some(321);
    current
        .providers
        .insert("fallback".into(), current.providers["local"].clone());
    current
        .routing
        .routes
        .get_mut("worker")
        .unwrap()
        .push(config::RouteCandidateConfig {
            profile: "fallback".into(),
            model: None,
            auto: false,
        });
    current
}

fn process(
    h: &Harness,
    current: &Config,
    storage: &storage::Storage,
    config: &storage::StorageConfig,
) -> (
    AgentRuntime,
    Arc<runtime::compose::RoutedModel>,
    EventReceiver,
) {
    let bus = Arc::new(EventBus::new(1024));
    let receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(BulkRead)).unwrap();
    let model = runtime::compose::compose_routed_model(
        current,
        routing::ComposeDeps {
            credential_store: Arc::new(
                FileCredentialStore::open(h._directory.path().join("credentials")).unwrap(),
            ),
            event_bus: Some(bus.clone()),
            env: Arc::new(MapEnv::from_iter([(KEY_ENV, "offline-test-key")])),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions::default(),
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap();
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone())
        .with_run_store(runtime::RunStore::open(config, storage.handle()).unwrap())
        .with_system_prompts(contract_prompts())
        .with_compaction(current.compaction.clone());
    (runtime, model, receiver)
}

#[tokio::test]
async fn routed_recovery_activation_cache_keeps_fallback_pin_and_wire_prefix() {
    recover(false).await;
}

#[tokio::test]
async fn routed_recovery_same_model_changed_temperature_blocks_before_ack_or_http() {
    recover(true).await;
}

async fn recover(changed_settings: bool) {
    let h = harness(
        vec![text_response("completed"), text_response("followup")],
        1_000_000,
    );
    let mut current = settings(&h);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("activation.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let (old, old_model, mut events) = process(&h, &current, &storage, &config);
    let run = old.reserve_run_id();
    let pin = runtime::RoutePinRecord {
        chain: "worker".into(),
        position: 1,
        profile: "fallback".into(),
        model: MODEL.into(),
    };
    // Persist a real fallback pin. Its production creation path is independently
    // covered by compose::tests::checkpoint::recovery_validates_fallback_pin… .
    old_model.restore_route_pins(&run.to_string(), std::slice::from_ref(&pin));
    old.spawn_reserved(
        run,
        None,
        Role::Worker,
        "Keep original task",
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    let mut all_events = through_phase(&mut events, run, AgentRunPhase::Waiting).await;
    old.request_quiesce();
    old.wait_quiesced().await.unwrap();
    let fence = old.fence_dispatch_restart().await.unwrap();
    old.drain_dispatch_tails(&fence).await.unwrap();
    let records = old
        .capture_recovery_records(&BTreeMap::from([(
            run,
            RecoveryRecipe {
                settings_reference: "current".into(),
                policy: ResumePolicy::CoreOwned,
                goal_id: None,
                team: None,
                lane: None,
                descriptor: None,
            },
        )]))
        .unwrap();
    old.save_recovery_graph("routed-wire", &records).unwrap();
    let before = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(before.len(), 1);
    let original = before[0].body.clone();
    if changed_settings {
        current.agents.worker.base.generation.temperature = Some(0.75);
    }
    let (restarted, model, mut events) = process(&h, &current, &storage, &config);
    assert!(model.route_pins(&run.to_string()).is_empty());
    let ownership = h._directory.path().join("ownership");
    let leases =
        storage::shared_with_wait(&config.db_path, &ownership, std::time::Duration::ZERO).unwrap();
    let startup = leases
        .recovery_startup_complete(&config.db_path, &ownership)
        .unwrap();
    let plan = restarted.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 1, "{plan:?}");
    let mut paused = restarted
        .prepare_component(&plan.components[0], startup, &ownership)
        .unwrap();
    restarted
        .restore_relations_and_continuations(&mut paused)
        .unwrap();
    let owner = runtime::ownership::OwnerHost::open(
        &h._directory.path().join("owner"),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    let authority = CurrentRecoveryAuthority {
        members: BTreeMap::from([(
            run,
            RunConfig {
                ownership: Some(owner.start("current").unwrap()),
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )]),
    };
    let result = restarted.activate_component(paused, authority);
    if changed_settings {
        assert!(
            result.is_err(),
            "same model with changed settings must block"
        );
        assert!(restarted.list_agents().is_empty());
        assert!(model.route_pins(&run.to_string()).is_empty());
        assert_eq!(
            storage::Database::open(&config)
                .unwrap()
                .recovery_claims()
                .unwrap()[0]
                .state,
            storage::recovery::RecoveryClaimState::Prepared
        );
        assert_eq!(
            h.mock
                .recorded_requests()
                .iter()
                .filter(|r| r.path == "/v1/chat/completions")
                .count(),
            1
        );
        return;
    }
    assert_eq!(result.unwrap(), [run]);
    assert_eq!(model.route_pins(&run.to_string()), [pin]);
    all_events.extend(through_phase(&mut events, run, AgentRunPhase::Waiting).await);
    restarted
        .send_message(run, "Actual followup".into())
        .unwrap();
    all_events.extend(through_phase(&mut events, run, AgentRunPhase::Waiting).await);
    restarted.cancel(run).unwrap();
    restarted.wait(run).await.unwrap();
    let requests = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| r.body)
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    let after = &requests[1];
    assert_append_only(CacheProtocol::OpenAi, &original, after).unwrap();
    // Independent assertions, not the mock cache's comparison algorithm.
    let prefix = original["messages"].as_array().unwrap();
    assert_eq!(
        &after["messages"].as_array().unwrap()[..prefix.len()],
        prefix
    );
    for key in ["model", "temperature", "max_tokens", "tools", "tool_choice"] {
        assert_eq!(after[key], original[key], "changed {key}");
    }
    let usage: Vec<_> = all_events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                cache_read_tokens,
                ..
            }) => Some((*input_tokens, *cache_read_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 2);
    assert!(usage[0].0 > 0);
    assert_eq!(usage[1].1, usage[0].0);
}
