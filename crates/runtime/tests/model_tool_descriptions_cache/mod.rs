//! B contracts share the registered cache binary and its input-derived mock.
use super::*;
const DESCRIPTION: &str = "Complete this run with its verified result.";

pub(super) fn configure(config: &mut Config) {
    for (profile, provider) in &config.providers {
        config.model_tool_descriptions.insert(
            profile.clone(),
            [(
                provider.default_model.clone(),
                [("working_note_read".into(), DESCRIPTION.into())].into(),
            )]
            .into(),
        );
    }
}
pub(super) fn assert_overrides(h: &Harness) {
    fn collect(value: &Value, descriptions: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                if object.get("name").and_then(Value::as_str) == Some("working_note_read")
                    && let Some(text) = object.get("description").and_then(Value::as_str)
                {
                    descriptions.push(text.into());
                }
                for child in object.values() {
                    collect(child, descriptions);
                }
            }
            Value::Array(values) => {
                for child in values {
                    collect(child, descriptions);
                }
            }
            _ => {}
        }
    }
    let mut descriptions = Vec::new();
    for request in h
        .mock
        .recorded_requests()
        .iter()
        .filter(|r| r.path == h.wire.path())
    {
        collect(&request.body, &mut descriptions);
    }
    assert!(
        !descriptions.is_empty(),
        "override must have reached actual wire tools"
    );
    assert!(descriptions.iter().all(|text| text == DESCRIPTION));
}

fn harness_for_descriptions(script: Vec<ScriptedResponse>) -> Harness {
    harness_with_config(
        script,
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.9,
        configure,
    )
}
fn attach_store(h: &mut Harness) -> (storage::Storage, storage::StorageConfig) {
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("descriptions.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .clone()
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    (storage, config)
}
fn saved(h: &Harness, run: RunId) -> runtime::restore::RunRestoreDescriptor {
    let record = h
        .runtime
        .with_storage_reader(|db| db.run_context(&run.to_string()))
        .unwrap()
        .flatten()
        .unwrap();
    serde_json::from_str(&record.config_json).unwrap()
}
#[tokio::test]
async fn ordinary_turns_and_setting_edits_keep_the_entry_snapshot_and_wire_prefix() {
    let review = support::ReviewStorage::new();
    let mut h = harness_for_descriptions(vec![
        read_response(0),
        text_response("first"),
        read_response(1),
        text_response("second"),
    ]);
    let (_storage, _) = attach_store(&mut h);
    let preview = h
        .runtime
        .preview_base_context(runtime::base_context::BaseContextRequest::new(Role::Worker))
        .await
        .unwrap();
    let run = h.runtime.delegate_background(
        Role::Worker,
        "Keep the selected tools".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    let original = saved(&h, run).tool_descriptions.unwrap();
    assert_eq!(original.descriptions["working_note_read"], DESCRIPTION);
    let mut edited = recovery_activation_cache::settings(&h);
    configure(&mut edited);
    edited
        .model_tool_descriptions
        .get_mut("local")
        .unwrap()
        .get_mut(MODEL)
        .unwrap()
        .insert(
            "working_note_read".into(),
            "Must not appear in an existing run".into(),
        );
    h.runtime.reload_model_settings(&edited);
    review.attach(&h.runtime);
    h.runtime
        .send_message(run, "Continue after editing configuration".into())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
    assert_eq!(saved(&h, run).tool_descriptions.unwrap(), original);
    let first = &chat_requests(&h)[0];
    let mut projected = preview.tools.iter().map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.input_schema}})).collect::<Vec<_>>();
    projected.sort_by_key(|tool| tool["function"]["name"].as_str().unwrap().to_string());
    assert_eq!(
        first["tools"],
        json!(projected),
        "preview and actual wire use one catalog projection"
    );
    assert_overrides(&h);
    verify_trace(&h, run, &events, 0);
    h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    h.runtime.wait(run).await.unwrap();
}
#[tokio::test]
async fn explicit_model_choice_at_entry_selects_its_profile_but_later_choice_does_not_reselect() {
    let review = support::ReviewStorage::new();
    let mut h = harness_with_config(
        vec![
            text_response("first"),
            read_response(1),
            text_response("second"),
        ],
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.9,
        |cfg| {
            cfg.providers
                .insert("chosen".into(), cfg.providers["local"].clone());
            configure(cfg);
            cfg.model_tool_descriptions
                .get_mut("local")
                .unwrap()
                .get_mut(MODEL)
                .unwrap()
                .insert("working_note_read".into(), "Wrong profile".into());
        },
    );
    let (_storage, _) = attach_store(&mut h);
    let run = h.runtime.delegate_background(
        Role::Worker,
        "Use the user-selected profile".into(),
        RunConfig {
            model_preference: Some(runtime::ModelPreference {
                profile: "chosen".into(),
                model: Some(MODEL.into()),
                reasoning_effort: None,
            }),
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    let snapshot = saved(&h, run).tool_descriptions.unwrap();
    assert_eq!(snapshot.profile.as_deref(), Some("chosen"));
    assert_eq!(snapshot.route_model_id.as_deref(), Some(MODEL));
    review.attach(&h.runtime);
    h.runtime
        .set_model_preference(
            run,
            Some(runtime::ModelPreference {
                profile: "local".into(),
                model: Some(MODEL.into()),
                reasoning_effort: None,
            }),
        )
        .unwrap();
    h.runtime
        .send_message(run, "Select the other profile after start".into())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
    assert_overrides(&h);
    // The profile is a legitimate routing boundary; descriptions still stay fixed.
    let requests = chat_requests(&h);
    assert_eq!(requests[0]["tools"], requests[1]["tools"]);
    assert_append_only(CacheProtocol::OpenAi, &requests[1], &requests[2]).unwrap();
    h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    h.runtime.wait(run).await.unwrap();
}
#[tokio::test]
async fn restore_and_fork_keep_saved_descriptions_after_settings_edit() {
    let mut h = harness_for_descriptions(vec![
        text_response("first"),
        read_response(2),
        text_response("forked"),
        read_response(1),
        text_response("restored"),
    ]);
    let (_storage, _) = attach_store(&mut h);
    let run = h
        .runtime
        .delegate_chat(
            "description-restore",
            Role::Worker,
            "Original turn".into(),
            RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    let original = saved(&h, run).tool_descriptions.unwrap();
    let completed = |event: &Event| match &event.kind {
        EventKind::Lifecycle(LifecycleEvent::TurnCompleted {
            run_id,
            context_len,
        }) if run_id == &run.to_string() => Some(*context_len),
        _ => None,
    };
    let context_len = match events.iter().find_map(completed) {
        Some(len) => len,
        None => loop {
            if let Some(len) = completed(&h.receiver.recv().await.unwrap()) {
                break len;
            }
        },
    };
    h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    through_phase(&mut h.receiver, run, AgentRunPhase::Stopped).await;
    h.runtime.wait(run).await.unwrap();
    let mut edited = recovery_activation_cache::settings(&h);
    configure(&mut edited);
    edited
        .model_tool_descriptions
        .get_mut("local")
        .unwrap()
        .get_mut(MODEL)
        .unwrap()
        .clear();
    h.runtime.reload_model_settings(&edited);
    let fork = h
        .runtime
        .delegate_chat_seeded(
            "description-fork",
            Role::Worker,
            "Fork original turn".into(),
            RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
            Some(runtime::ChatForkSeed {
                source_run_id: run.to_string(),
                context_len,
            }),
        )
        .unwrap();
    let fork_events = through_phase(&mut h.receiver, fork, AgentRunPhase::Waiting).await;
    assert_eq!(saved(&h, fork).tool_descriptions.unwrap(), original);
    h.runtime.stop(fork, runtime::StopScope::SelfOnly).unwrap();
    h.runtime.wait(fork).await.unwrap();
    h.runtime
        .resume_chat(
            run,
            RunConfig {
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    h.runtime
        .send_message(run, "Continue with the persisted tools".into())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
    assert_eq!(saved(&h, run).tool_descriptions.unwrap(), original);
    assert_overrides(&h);
    let requests = chat_requests(&h);
    let original_requests = requests[..1]
        .iter()
        .chain(&requests[3..])
        .cloned()
        .collect::<Vec<_>>();
    verify_fixed_trace(&h, run, &events, &original_requests);
    verify_fixed_trace(&h, fork, &fork_events, &requests[1..3]);
    h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    h.runtime.wait(run).await.unwrap();
}
#[tokio::test]
async fn initial_snapshot_write_failure_refuses_before_any_provider_request() {
    let mut h = harness_for_descriptions(vec![text_response("must not run")]);
    let (_storage, config) = attach_store(&mut h);
    let connection = rusqlite::Connection::open(&config.db_path).unwrap();
    connection.execute_batch("CREATE TRIGGER refuse_snapshot BEFORE INSERT ON run_contexts BEGIN SELECT RAISE(FAIL, 'injected snapshot failure'); END;").unwrap();
    let run = h.runtime.delegate_background(
        Role::Worker,
        "Must persist first".into(),
        RunConfig::default(),
    );
    through_phase(&mut h.receiver, run, AgentRunPhase::Error).await;
    assert_eq!(h.runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    assert!(chat_requests(&h).is_empty());
}
#[tokio::test]
async fn unknown_external_generated_and_team_targets_fail_before_any_provider_request() {
    for target in ["unknown", "read", "run_js", "mcp_read", "task_claim"] {
        // BulkRead is deliberately external even though its public name is read.
        let mut h = harness_with_config(
            vec![text_response("must not run")],
            1_000_000,
            Arc::new(BulkRead),
            Vec::new(),
            None,
            0.9,
            |cfg| {
                cfg.model_tool_descriptions.insert(
                    "local".into(),
                    [(
                        MODEL.into(),
                        [(target.into(), "Invalid target".into())].into(),
                    )]
                    .into(),
                );
            },
        );
        let run = h.runtime.delegate_background(
            Role::Worker,
            "Reject unsupported descriptions".into(),
            RunConfig::default(),
        );
        through_phase(&mut h.receiver, run, AgentRunPhase::Error).await;
        assert_eq!(h.runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
        assert!(chat_requests(&h).is_empty());
    }
}

#[tokio::test]
async fn child_selects_its_own_entry_identity_without_inheriting_the_parent_map() {
    let review = support::ReviewStorage::new();
    let mut h = harness_with_config(
        vec![
            text_response("parent first"),
            read_response(1),
            text_response("child done"),
            read_response(2),
            text_response("parent continued"),
        ],
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.9,
        |cfg| {
            cfg.providers
                .insert("child".into(), cfg.providers["local"].clone());
            configure(cfg);
            cfg.model_tool_descriptions
                .get_mut("child")
                .unwrap()
                .get_mut(MODEL)
                .unwrap()
                .insert(
                    "working_note_read".into(),
                    "Child-specific description".into(),
                );
        },
    );
    let (_storage, _) = attach_store(&mut h);
    let parent = h.runtime.delegate_background(
        Role::Worker,
        "Parent task".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    let mut parent_events = through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await;
    review.attach(&h.runtime);
    let child = h
        .runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "Child task",
            RunConfig {
                model_preference: Some(runtime::ModelPreference {
                    profile: "child".into(),
                    model: Some(MODEL.into()),
                    reasoning_effort: None,
                }),
                ..Default::default()
            },
        )
        .unwrap();
    let child_events = through_phase(&mut h.receiver, child, AgentRunPhase::Done).await;
    h.runtime.wait(child).await.unwrap();
    assert_eq!(
        saved(&h, parent).tool_descriptions.unwrap().descriptions["working_note_read"],
        DESCRIPTION
    );
    assert_eq!(
        saved(&h, child).tool_descriptions.unwrap().descriptions["working_note_read"],
        "Child-specific description"
    );
    parent_events.extend(child_events.clone());
    parent_events.extend(through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await);
    let requests = chat_requests(&h);
    let parent_requests = requests[..1]
        .iter()
        .chain(&requests[3..])
        .cloned()
        .collect::<Vec<_>>();
    verify_fixed_trace(&h, parent, &parent_events, &parent_requests);
    verify_fixed_trace(&h, child, &child_events, &requests[1..3]);
    let description = |r: &Value| {
        r["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["function"]["name"] == "working_note_read")
            .unwrap()["function"]["description"]
            .clone()
    };
    assert_eq!(description(&requests[0]), DESCRIPTION);
    for request in &requests[1..3] {
        assert_eq!(description(request), "Child-specific description");
    }
    for request in &requests[3..] {
        assert_eq!(description(request), DESCRIPTION);
    }
    h.runtime
        .stop(parent, runtime::StopScope::SelfOnly)
        .unwrap();
    h.runtime.wait(parent).await.unwrap();
}

struct RefuseCapture;
#[async_trait::async_trait]
impl runtime::benchmark::BenchmarkRecorder for RefuseCapture {
    async fn capture(&self, _: &runtime::benchmark::BenchmarkCheckpoint) -> Result<(), String> {
        panic!("unsupported override must not be recorded")
    }
}
#[tokio::test]
async fn benchmark_recording_and_replay_refuse_overrides_before_leaf_provider_calls() {
    // Recording applies only to the selected leaf, not unrelated normal parents.
    let mut h = harness_for_descriptions(vec![text_response("parent ready")]);
    h.runtime = h
        .runtime
        .with_benchmark_recorder(
            runtime::benchmark::BenchmarkSelector {
                role: Role::Worker,
                category: None,
                occurrence: 1,
            },
            Arc::new(RefuseCapture),
        )
        .unwrap();
    let parent = h.runtime.delegate_background(
        Role::Worker,
        "Parent".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await;
    let before = chat_requests(&h).len();
    let child = h
        .runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "Unsupported leaf",
            RunConfig::default(),
        )
        .unwrap();
    let events = through_phase(&mut h.receiver, child, AgentRunPhase::Error).await;
    h.runtime.wait(child).await.unwrap();
    assert_eq!(chat_requests(&h).len(), before);
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { reason: Some(reason), .. }) if reason.contains("model tool description overrides"))));
    h.runtime
        .stop(parent, runtime::StopScope::SelfOnly)
        .unwrap();
    h.runtime.wait(parent).await.unwrap();

    let mut h = harness_for_descriptions(vec![text_response("must not run")]);
    let preference = runtime::ModelPreference {
        profile: "local".into(),
        model: Some(MODEL.into()),
        reasoning_effort: None,
    };
    let root = h._directory.path().canonicalize().unwrap();
    let checkpoint = runtime::benchmark::BenchmarkCheckpoint {
        version: 1,
        origin_run_id: "run-91".parse().unwrap(),
        parent_run_id: "run-90".parse().unwrap(),
        role: Role::Worker,
        category: None,
        prompt: "Replay".into(),
        messages: vec![providers::Message {
            role: providers::Role::User,
            content: vec![providers::ContentBlock::Text {
                text: "Replay".into(),
            }],
        }],
        tools: Vec::new(),
        workspace_root: root.clone(),
        model: runtime::benchmark::BenchmarkModelSettings {
            protocol: model::ApiProtocol::OpenAiCompletions,
            preference: preference.clone(),
            generation: Default::default(),
            service_tier: None,
        },
        selected_model: format!("local/{MODEL}"),
        budget: Default::default(),
        load_skills: Vec::new(),
    };
    let run = h
        .runtime
        .replay_benchmark(checkpoint, root, preference)
        .unwrap();
    let events = through_phase(&mut h.receiver, run, AgentRunPhase::Error).await;
    h.runtime.wait(run).await.unwrap();
    assert!(chat_requests(&h).is_empty());
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { reason: Some(reason), .. }) if reason.contains("model tool description overrides"))));
}

#[tokio::test]
async fn empty_snapshot_and_pre_feature_record_do_not_adopt_new_config_on_restore() {
    for legacy in [false, true] {
        let mut h = harness(
            vec![
                text_response("first"),
                read_response(1),
                text_response("restored"),
            ],
            1_000_000,
        );
        let (storage, _) = attach_store(&mut h);
        let run = h
            .runtime
            .delegate_chat(
                "empty-restore",
                Role::Worker,
                "Original empty configuration".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        assert!(
            saved(&h, run)
                .tool_descriptions
                .unwrap()
                .descriptions
                .is_empty()
        );
        h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
        through_phase(&mut h.receiver, run, AgentRunPhase::Stopped).await;
        h.runtime.wait(run).await.unwrap();
        if legacy {
            let mut record = h
                .runtime
                .with_storage_reader(|db| db.run_context(&run.to_string()))
                .unwrap()
                .flatten()
                .unwrap();
            let mut descriptor: Value = serde_json::from_str(&record.config_json).unwrap();
            descriptor
                .as_object_mut()
                .unwrap()
                .remove("tool_descriptions");
            record.config_json = descriptor.to_string();
            storage.handle().upsert_run_context(&record).unwrap();
        }
        let mut edited = recovery_activation_cache::settings(&h);
        configure(&mut edited);
        h.runtime.reload_model_settings(&edited);
        h.runtime.resume_chat(run, RunConfig::default()).unwrap();
        through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        h.runtime
            .send_message(run, "Continue after new overrides were added".into())
            .unwrap();
        events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
        let snapshot = saved(&h, run).tool_descriptions;
        if legacy {
            assert!(snapshot.is_none());
        } else {
            assert!(snapshot.unwrap().descriptions.is_empty());
        }
        let requests = chat_requests(&h);
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        verify_trace(&h, run, &events, 0);
        h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
}
#[tokio::test]
async fn corrupt_present_snapshot_refuses_restore_without_using_current_settings() {
    let mut h =
        harness_for_descriptions(vec![text_response("first"), text_response("must not run")]);
    let (storage, _) = attach_store(&mut h);
    let run = h
        .runtime
        .delegate_chat(
            "corrupt-snapshot",
            Role::Worker,
            "Original".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    through_phase(&mut h.receiver, run, AgentRunPhase::Stopped).await;
    h.runtime.wait(run).await.unwrap();
    let mut record = h
        .runtime
        .with_storage_reader(|db| db.run_context(&run.to_string()))
        .unwrap()
        .flatten()
        .unwrap();
    let original: Value = serde_json::from_str(&record.config_json).unwrap();
    for corrupt in [Value::Null, json!({"version":1}), {
        let mut value = original["tool_descriptions"].clone();
        value["hash"] = "corrupt".into();
        value
    }] {
        let mut descriptor = original.clone();
        descriptor["tool_descriptions"] = corrupt;
        record.config_json = descriptor.to_string();
        storage.handle().upsert_run_context(&record).unwrap();
        assert!(h.runtime.resume_chat(run, RunConfig::default()).is_err());
        assert_eq!(chat_requests(&h).len(), 1);
    }
}

#[tokio::test]
async fn initial_family_is_intentionally_frozen_when_binding_changes_before_restore() {
    const GPT: &str = "gpt-5.2";
    let mut h = harness_with_model(
        Wire::ChatCompletions,
        GPT,
        vec![
            ScriptedResponse::tool_call(
                "first",
                GPT,
                0,
                "first",
                "finish",
                [json!({"result":"first"}).to_string()],
            ),
            read_response(1),
            ScriptedResponse::tool_call(
                "last",
                GPT,
                0,
                "last",
                "finish",
                [json!({"result":"last"}).to_string()],
            ),
        ],
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.9,
        configure,
    );
    let (storage, storage_config) = attach_store(&mut h);
    let preference = runtime::ModelPreference {
        profile: "local".into(),
        model: Some(GPT.into()),
        reasoning_effort: None,
    };
    let run = h
        .runtime
        .delegate_chat(
            "saved-family",
            Role::Orchestrator,
            "Finish the first turn".into(),
            RunConfig {
                model_preference: Some(preference.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Done).await;
    h.runtime.wait(run).await.unwrap();
    assert_eq!(
        saved(&h, run).tool_descriptions.unwrap().family,
        runtime::ModelFamily::Gpt5
    );
    let mut current = recovery_activation_cache::settings(&h);
    current.providers.get_mut("fallback").unwrap().models =
        serde_json::from_value(json!([{"id":"unrelated-family"}])).unwrap();
    current.providers.get_mut("fallback").unwrap().default_model = "unrelated-family".into();
    current.routing.routes.get_mut("orchestrator").unwrap()[0].profile = "fallback".into();
    let (runtime, model, mut receiver) = recovery_activation_cache::process_with_read(
        &h,
        &current,
        &storage,
        &storage_config,
        Arc::new(BulkRead),
    );
    assert_eq!(
        model.selected_model(Role::Orchestrator, None),
        "fallback/unrelated-family"
    );
    h.runtime = runtime.with_skills(Arc::new(runtime::skill::discover_skills(&[])));
    h.runtime
        .resume_chat(
            run,
            RunConfig {
                model_preference: Some(preference),
                ..Default::default()
            },
        )
        .unwrap();
    through_phase(&mut receiver, run, AgentRunPhase::Waiting).await;
    h.runtime
        .send_message(run, "Finish another turn".into())
        .unwrap();
    events.extend(through_phase(&mut receiver, run, AgentRunPhase::Done).await);
    h.runtime.wait(run).await.unwrap();
    let requests = chat_requests(&h);
    assert_eq!(
        requests[0]["tools"], requests[1]["tools"],
        "saved family keeps the delegate harness suffix after a binding edit"
    );
    assert!(
        requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["function"]["name"] == "delegate")
            .unwrap()["function"]["description"]
            .as_str()
            .unwrap()
            .ends_with(runtime::SUBAGENT_CONTEXT_NOTE)
    );
    assert_overrides(&h);
    verify_trace(&h, run, &events, 0);
}
#[tokio::test]
async fn removed_saved_target_is_diagnosed_without_retargeting_or_snapshot_rewrite() {
    use sha2::{Digest, Sha256};
    let mut h = harness_for_descriptions(vec![
        text_response("first"),
        read_response(1),
        text_response("restored"),
    ]);
    let (storage, _) = attach_store(&mut h);
    let run = h
        .runtime
        .delegate_chat(
            "removed-hidden",
            Role::Worker,
            "Original".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    through_phase(&mut h.receiver, run, AgentRunPhase::Stopped).await;
    h.runtime.wait(run).await.unwrap();
    let mut record = h
        .runtime
        .with_storage_reader(|db| db.run_context(&run.to_string()))
        .unwrap()
        .flatten()
        .unwrap();
    let mut descriptor = saved(&h, run);
    let snapshot = descriptor.tool_descriptions.as_mut().unwrap();
    // Prior-build fixture: a fixed builtin existed, but was hidden from Worker.
    // Its removal must not create a schema boundary because wire tools did not change.
    snapshot.descriptions.insert(
        "retired_hidden_builtin".into(),
        "Retired description must not be assigned to a different tool".into(),
    );
    snapshot.hash = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                snapshot.version,
                &snapshot.profile,
                &snapshot.route_model_id,
                &snapshot.service_tier,
                &snapshot.descriptions,
                snapshot.family
            ))
            .unwrap()
        )
    );
    let original = snapshot.clone();
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    h.runtime.resume_chat(run, RunConfig::default()).unwrap();
    let restored = through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    assert!(restored.iter().any(|e| matches!(&e.kind, EventKind::Diagnostic(d) if d.code=="SAVED_TOOL_DESCRIPTION_TARGET_REMOVED")));
    events.extend(restored);
    h.runtime
        .send_message(run, "Continue with remaining builtins".into())
        .unwrap();
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
    assert_eq!(saved(&h, run).tool_descriptions.unwrap(), original);
    let requests = chat_requests(&h);
    assert_eq!(requests[0]["tools"], requests[1]["tools"]);
    assert!(
        !requests[1]["tools"]
            .to_string()
            .contains("Retired description")
    );
    assert_overrides(&h);
    verify_trace(&h, run, &events, 0);
    h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    h.runtime.wait(run).await.unwrap();
}

// Usage events have no run ID. For interleaved parent/child runs, correlate
// provider completion IDs instead of attributing global usage to either run.
fn verify_fixed_trace(h: &Harness, run: RunId, events: &[Event], requests: &[Value]) {
    let starts = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestStarted {
                request_id,
                run_id: Some(id),
                ..
            }) if id == &run.to_string() => Some(request_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let completed = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                request_id,
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) if id == &run.to_string() => {
                Some((request_id.clone(), (*input_tokens, *cache_read_tokens)))
            }
            _ => None,
        })
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(requests.len(), starts.len());
    assert_eq!(requests.len(), completed.len());
    assert!(requests.len() >= 2);
    for (index, id) in starts.iter().enumerate() {
        let (input, cached) = completed[id];
        assert!(input > 0 && cached <= input);
        // Different runs may legitimately share a provider-cached system prefix.
        // The contract here is full reuse after this run's own first request.
        if index > 0 {
            assert_append_only(h.wire.protocol(), &requests[index - 1], &requests[index]).unwrap();
            assert!(
                cached >= completed[&starts[index - 1]].0,
                "following request must reuse the previously sent prefix"
            );
        }
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Diagnostic(d) if d.code=="CacheRegression"))
    );
}
