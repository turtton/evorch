use super::*;
use runtime::{ModelPreference, ModelPreferenceUpdate};

fn preference(profile: &str, model: &str, effort: &str) -> ModelPreference {
    ModelPreference {
        profile: profile.into(),
        model: Some(model.into()),
        reasoning_effort: Some(effort.into()),
    }
}

async fn waiting(
    events: &mut event_bus::EventReceiver,
    run: runtime::RunId,
    observed: &mut Vec<event_bus::Event>,
) {
    loop {
        let event = events.recv().await.unwrap();
        let ready = matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id, to: AgentRunPhase::Waiting, ..
        }) if run_id == &run.to_string());
        observed.push(event);
        if ready {
            break;
        }
    }
}

#[tokio::test]
async fn adopted_model_and_effort_choices_preserve_history_and_reuse_each_new_prefix() {
    scenario(false).await;
}

#[tokio::test]
async fn planned_restart_preserves_adopted_selection_and_internal_wake_prefix() {
    scenario(true).await;
}

async fn scenario(restart: bool) {
    let mut choices = vec![
        ("primary", PRIMARY_MODEL, Some("low"), true),
        ("small", SMALL_MODEL, Some("low"), true),
        ("small", SMALL_MODEL, Some("high"), true),
        ("primary", PRIMARY_MODEL, None, false), // Role default
        ("primary", PRIMARY_MODEL, None, true),  // Same effective selection
        ("small", SMALL_MODEL, Some("low"), true),
    ];
    if restart {
        choices.insert(2, choices[1]);
    }
    let scripts = choices
        .iter()
        .enumerate()
        .flat_map(|(i, (_, model, _, _))| {
            [
                read(model, i),
                ScriptedResponse::text_stream(&format!("done-{i}"), model, ["complete"]),
            ]
        })
        .collect();
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        scripts,
        vec![PRIMARY_MODEL.into(), SMALL_MODEL.into()],
    );
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(
        config::project_main_config_path(directory.path()),
        format!(
            r#"
version = 3
[providers.primary]
type = "openai-compatible"
base_url = "{url}"
api_key_env = "{KEY_ENV}"
models = [{{ id = "{PRIMARY_MODEL}", enabled = true, metadata_source = "manual", context_window = 1000000, preset = "stable-output" }}]
default_model = "{PRIMARY_MODEL}"
[providers.small]
type = "openai-compatible"
base_url = "{url}"
api_key_env = "{KEY_ENV}"
models = [{{ id = "{SMALL_MODEL}", enabled = true, metadata_source = "manual", context_window = 1000000, preset = "stable-output" }}]
default_model = "{SMALL_MODEL}"
[model_presets.stable-output]
max_output_tokens = 321
[[routing.routes.primary]]
profile = "primary"
[agents.worker.generation]
max_tokens = 321
[agents.worker]
models = [{{ route = "primary" }}]
[compaction]
context_window_tokens = 1000000
threshold = 0.9
summarizer = "structural"
"#,
            url = mock.base_url()
        ),
    )
    .unwrap();
    let config = Config::load(&LoadOptions {
        user_config_dir: Some(directory.path().join(config::PROJECT_CONFIG_DIR)),
        project_dir: Some(directory.path().into()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let storage_config = storage::StorageConfig {
        db_path: directory.path().join("cache.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let process = || {
        let bus = Arc::new(EventBus::new(1024));
        let events = bus.subscribe();
        let mut executor = ToolExecutor::new(bus.clone());
        executor.register(Arc::new(Read)).unwrap();
        let runtime = compose_runtime(RuntimeComposition {
            model_catalog_dir: Some(test_support::model_catalog_dir()),
            user_config_dir: Some(directory.path().join(config::PROJECT_CONFIG_DIR)),
            config: &config,
            bus,
            executor: Arc::new(executor),
            credential_store: Arc::new(
                FileCredentialStore::open(directory.path().join("credentials")).unwrap(),
            ),
            env: Arc::new(MapEnv::from_iter([(KEY_ENV, "offline-test-key")])),
            model_source: ModelSource::Configured,
            workspace: None,
        })
        .unwrap()
        .runtime
        .with_run_store(runtime::RunStore::open(&storage_config, storage.handle()).unwrap())
        .with_compaction(config.compaction.clone());
        (runtime, events)
    };
    let (mut runtime, mut events) = process();
    let review = support::ReviewStorage::new();
    review.attach(&runtime);
    let run = runtime.delegate_background(
        Role::Worker,
        "original goal".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            model_preference: Some(preference("primary", PRIMARY_MODEL, "low")),
            ..Default::default()
        },
    );
    runtime
        .bind_thread_root("selection-cache-thread", run)
        .unwrap();
    let mut observed = vec![];
    waiting(&mut events, run, &mut observed).await;
    for (turn, (profile, model, effort, explicit)) in choices[1..].iter().enumerate() {
        let update = if *explicit {
            ModelPreferenceUpdate::explicit(ModelPreference {
                profile: (*profile).into(),
                model: Some((*model).into()),
                reasoning_effort: effort.map(str::to_owned),
            })
        } else {
            ModelPreferenceUpdate::RoleDefault
        };
        if restart && turn == 1 {
            runtime
                .continue_goal(
                    run,
                    "internal wake after restart".into(),
                    RunConfig::default(),
                )
                .unwrap();
        } else {
            runtime
                .continue_goal_with_model_update(
                    run,
                    format!("next {profile} {effort:?}"),
                    RunConfig::default(),
                    update,
                )
                .unwrap();
        }
        waiting(&mut events, run, &mut observed).await;
        if restart && turn == 0 {
            use runtime::quiesce::{RecoveryRecipe, ResumePolicy};
            use std::collections::BTreeMap;
            let recipes = BTreeMap::from([(
                run,
                RecoveryRecipe {
                    settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                    policy: ResumePolicy::CoreOwned,
                    descriptor: None,
                    goal_id: None,
                    team: None,
                    lane: None,
                },
            )]);
            runtime.begin_planned_restart_quiesce(&recipes).unwrap();
            runtime.wait_quiesced().await.unwrap();
            let fence = runtime.fence_dispatch_restart().await.unwrap();
            let gate = runtime::quiesce::HostRestartGate::default();
            assert!(gate.transition(1, runtime::quiesce::HostRestartPhase::Sealing));
            let sealed = gate.sealed(&runtime, 1, fence).await.unwrap();
            use storage::recovery::restart::RestartMutation;
            let tx = storage
                .handle()
                .mutate_restart(
                    "selection-cache".into(),
                    None,
                    RestartMutation::Create(Box::new(support::fixture_restart_metadata(
                        "selection-cache",
                    ))),
                )
                .unwrap();
            let mut session = sealed.begin_capture(&runtime, &tx, recipes).unwrap();
            runtime
                .persist_restart_capture_expected(&mut session)
                .unwrap();
            let records = runtime.read_restart_capture_records(&session).unwrap();
            assert_eq!(
                records[0]
                    .recipe
                    .descriptor
                    .as_ref()
                    .unwrap()
                    .model_preference,
                Some(preference(profile, model, effort.unwrap()))
            );
            let committed = runtime.commit_restart_capture(&mut session).unwrap();
            storage
                .handle()
                .mutate_restart(
                    "selection-cache".into(),
                    Some(committed.revision),
                    RestartMutation::Arm {
                        nonce_hash: "fixture-nonce".into(),
                    },
                )
                .unwrap();
            drop(runtime);
            (runtime, events) = process();
            let db = storage::Database::open(&storage_config).unwrap();
            let proof = support::inherited_fixture_proof(&db, &storage.handle())
                .unwrap()
                .unwrap();
            runtime.adopt_restart_handoff(proof).unwrap();
            let tx = runtime.adopted_restart_transaction().unwrap();
            storage
                .handle()
                .mutate_restart(
                    tx.metadata.restart_id,
                    Some(tx.revision),
                    storage::recovery::restart::RestartMutation::Ready { at_ms: 0 },
                )
                .unwrap();
            let ownership = directory.path().join("recovery");
            let leases = storage::shared_with_wait(
                &storage_config.db_path,
                &ownership,
                std::time::Duration::ZERO,
            )
            .unwrap();
            let startup = leases
                .recovery_startup_complete(&storage_config.db_path, &ownership)
                .unwrap();
            let plan = runtime.recovery_plan().unwrap();
            let mut paused = runtime
                .prepare_component(&plan.components[0], startup, &ownership)
                .unwrap();
            runtime
                .restore_relations_and_continuations(&mut paused)
                .unwrap();
            let authority = runtime::CurrentRecoveryAuthority {
                members: BTreeMap::from([(
                    run,
                    RunConfig {
                        interactive: true,
                        keep_alive: true,
                        model_preference: Some(preference(profile, model, effort.unwrap())),
                        ..Default::default()
                    },
                )]),
            };
            let models = runtime.pin_recovery_models(&authority);
            models.model(run).unwrap().publish_model_catalog(Arc::new(
                catalog::ModelCatalog::load_or_refresh(&test_support::model_catalog_dir())
                    .await
                    .unwrap(),
            ));
            let reservation = runtime
                .reserve_automatic_activation(&paused.claim().manifest_id)
                .unwrap();
            runtime
                .activate_component_with_models(paused, authority, models, reservation)
                .unwrap()
                .1
                .release();
            waiting(&mut events, run, &mut observed).await;
        }
    }
    let requests = chat_requests(&mock);
    assert_eq!(requests.len(), choices.len() * 2);
    assert!(
        observed.iter().all(|e| !matches!(
            e.kind,
            EventKind::Compaction(CompactionEvent::Compacted { .. })
        )),
        "compatible selections must not replace history"
    );
    for (i, pair) in requests.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(pair[0]["model"], choices[i].1);
        assert_eq!(pair[0]["max_tokens"], 321, "turn {i}");
        assert_eq!(pair[0]["reasoning_effort"].as_str(), choices[i].2);
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
        assert_eq!(pair[0]["tools"], requests[0]["tools"]);
        assert_eq!(pair[0]["messages"][0], requests[0]["messages"][0]);
        if i > 0 {
            let mut previous = requests[i * 2 - 1].clone();
            previous["model"] = pair[0]["model"].clone();
            if let Some(effort) = pair[0].get("reasoning_effort") {
                previous["reasoning_effort"] = effort.clone();
            } else {
                previous.as_object_mut().unwrap().remove("reasoning_effort");
            }
            assert_append_only(CacheProtocol::OpenAi, &previous, &pair[0]).unwrap();
        }
    }
    let usage = observed
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                cache_read_tokens,
                ..
            }) => Some((*input_tokens, *cache_read_tokens)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), requests.len());
    for pair in usage.as_chunks::<2>().0 {
        assert!(
            pair[1].1 >= pair[0].0,
            "input-derived cache must reuse the new prefix: {pair:?}"
        );
    }
    assert_eq!(mock.remaining_scripts(), 0);
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
}
