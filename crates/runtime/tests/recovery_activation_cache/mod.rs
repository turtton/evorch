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
    process_with_read(h, current, storage, config, Arc::new(BulkRead))
}

fn process_with_read(
    h: &Harness,
    current: &Config,
    storage: &storage::Storage,
    config: &storage::StorageConfig,
    read: Arc<dyn Tool>,
) -> (
    AgentRuntime,
    Arc<runtime::compose::RoutedModel>,
    EventReceiver,
) {
    let bus = Arc::new(EventBus::new(1024));
    let receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(read).unwrap();
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
    recover_case(changed_settings, false, false, false).await;
}

#[tokio::test]
async fn planned_tool_schema_boundary_preserves_sent_history_and_reuses_the_new_prefix() {
    for (different_build, changed_tool) in
        [(false, false), (true, false), (true, true), (false, true)]
    {
        recover_case(false, true, different_build, changed_tool).await;
    }
}

struct ChangedRead;
#[async_trait::async_trait]
impl Tool for ChangedRead {
    fn name(&self) -> &'static str {
        BulkRead.name()
    }
    fn schema(&self) -> Value {
        changed_schema()
    }
    fn permissions(&self) -> Permissions {
        BulkRead.permissions()
    }
    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        BulkRead.execute(input).await
    }
}
fn changed_schema() -> Value {
    let mut schema = BulkRead.schema();
    schema["properties"]["offset"] = json!({"type":"integer"});
    schema
}

async fn recover_case(
    changed_settings: bool,
    tools_case: bool,
    different_build: bool,
    changed_tool: bool,
) {
    let h = harness(
        if tools_case {
            vec![
                read_response(0),
                text_response("completed"),
                text_response("followup"),
                text_response("reuse"),
            ]
        } else {
            vec![text_response("completed"), text_response("followup")]
        },
        1_000_000,
    );
    let mut current = settings(&h);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("activation.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let (old, old_model, mut events) = process(&h, &current, &storage, &config);
    // Current-thread test: pin is installed before the admitted task is polled.
    let run = old
        .delegate_chat(
            "wire-recovery-thread",
            Role::Worker,
            "Keep original task".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..RunConfig::default()
            },
        )
        .unwrap();
    let pin = runtime::RoutePinRecord {
        chain: "worker".into(),
        position: 1,
        profile: "fallback".into(),
        model: MODEL.into(),
    };
    // Persist a real fallback pin. Its production creation path is independently
    // covered by compose::tests::checkpoint::recovery_validates_fallback_pin… .
    old_model.restore_route_pins(&run.to_string(), std::slice::from_ref(&pin));
    let mut all_events = through_phase(&mut events, run, AgentRunPhase::Waiting).await;
    use storage::recovery::restart::RestartMutation;
    let recipes = BTreeMap::from([(
        run,
        RecoveryRecipe {
            settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
            policy: ResumePolicy::CoreOwned,
            goal_id: None,
            team: None,
            lane: None,
            descriptor: None,
        },
    )]);
    old.begin_planned_restart_quiesce(&recipes).unwrap();
    old.wait_quiesced().await.unwrap();
    let fence = old.fence_dispatch_restart().await.unwrap();
    let gate = runtime::quiesce::HostRestartGate::default();
    assert!(gate.transition(1, runtime::quiesce::HostRestartPhase::Sealing));
    let sealed = gate.sealed(&old, 1, fence).await.unwrap();
    let old_input = old
        .resume_intents()
        .unwrap()
        .into_iter()
        .find(|i| i.run_id == run)
        .unwrap()
        .provider_input
        .unwrap();
    let mut metadata = support::fixture_restart_metadata("routed-wire");
    if tools_case {
        metadata.old_builtin_tool_names = old_input
            .request
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect();
        if different_build {
            metadata.successor_build = "different-fixture-build".into();
        }
    }
    let tx = storage
        .handle()
        .mutate_restart(
            "routed-wire".into(),
            None,
            RestartMutation::Create(Box::new(metadata)),
        )
        .unwrap();
    let mut session = sealed.begin_capture(&old, &tx, recipes).unwrap();
    old.persist_restart_capture_expected(&mut session).unwrap();
    if tools_case {
        old.persist_restart_tool_snapshot(
            &mut session,
            run,
            &old_input.request.tools,
            "fixture-permissions",
        )
        .unwrap();
    }
    let committed = old.commit_restart_capture(&mut session).unwrap();
    storage
        .handle()
        .mutate_restart(
            "routed-wire".into(),
            Some(committed.revision),
            RestartMutation::Arm {
                nonce_hash: "fixture-nonce".into(),
            },
        )
        .unwrap();
    let before = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(before.len(), if tools_case { 2 } else { 1 });
    let original = before.last().unwrap().body.clone();
    if changed_settings {
        current.agents.worker.base.generation.temperature = Some(0.75);
    }
    let (restarted, model, mut events) = process_with_read(
        &h,
        &current,
        &storage,
        &config,
        if changed_tool {
            Arc::new(ChangedRead)
        } else {
            Arc::new(BulkRead)
        },
    );
    let fixture_db = storage::Database::open(&config).unwrap();
    let proof = support::inherited_fixture_proof(&fixture_db, &storage.handle())
        .unwrap()
        .unwrap();
    restarted.adopt_restart_handoff(proof).unwrap();
    let mut expected_wire_tools = original["tools"].clone();
    if tools_case {
        let tx = fixture_db
            .restart_transaction("routed-wire")
            .unwrap()
            .unwrap();
        let incarnation = &tx
            .expected
            .iter()
            .find(|m| m.run_id == run.to_string())
            .unwrap()
            .incarnation;
        let mut candidate = old_input.clone();
        if changed_tool {
            candidate
                .request
                .tools
                .iter_mut()
                .find(|t| t.name == "read")
                .unwrap()
                .input_schema = changed_schema();
        }
        let comparison = restarted.prepare_restart_tool_evidence(
            run,
            incarnation,
            &candidate,
            &candidate.request.tools,
            "fixture-permissions",
        );
        if changed_tool && !different_build {
            assert!(comparison.is_err());
            assert!(restarted.list_agents().is_empty());
            assert_eq!(
                h.mock
                    .recorded_requests()
                    .iter()
                    .filter(|r| r.path == h.wire.path())
                    .count(),
                before.len()
            );
            return;
        }
        let evidence = comparison.unwrap();
        assert_eq!(evidence.boundary, changed_tool);
        if changed_tool {
            assert!(
                restarted
                    .consume_restart_tool_boundary(evidence.clone(), candidate.clone())
                    .is_err(),
                "Ready required"
            );
        }
        let tx = fixture_db
            .restart_transaction("routed-wire")
            .unwrap()
            .unwrap();
        storage
            .handle()
            .mutate_restart(
                "routed-wire".into(),
                Some(tx.revision),
                RestartMutation::Ready { at_ms: 123 },
            )
            .unwrap();
        let boundary = restarted
            .consume_restart_tool_boundary(evidence.clone(), candidate.clone())
            .unwrap();
        let permitted = if changed_tool {
            let permitted = boundary.unwrap().apply(&old_input).unwrap();
            assert_eq!(permitted, candidate);
            assert!(
                restarted
                    .consume_restart_tool_boundary(evidence, candidate.clone())
                    .unwrap()
                    .is_none()
            );
            permitted
        } else {
            assert!(boundary.is_none());
            candidate.clone()
        };
        // OpenAI's wire contract orders functions by name, not by the executor's
        // registration order. Project the candidate independently of the encoder;
        // never sort the recorded wire, so an unexpected wire order still fails.
        let mut wire_order = permitted.request.tools.iter().collect::<Vec<_>>();
        wire_order.sort_by(|left, right| left.name.cmp(&right.name));
        expected_wire_tools = Value::Array(
            wire_order
                .into_iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                        },
                    })
                })
                .collect(),
        );
        let mut changed_permissions = candidate.clone();
        changed_permissions.request.temperature = Some(0.99);
        assert!(
            restarted
                .prepare_restart_tool_evidence(
                    run,
                    incarnation,
                    &changed_permissions,
                    &candidate.request.tools,
                    "fixture-permissions"
                )
                .is_err()
        );
        assert!(
            restarted
                .prepare_restart_tool_evidence(
                    run,
                    incarnation,
                    &candidate,
                    &candidate.request.tools,
                    "changed-permissions"
                )
                .is_err()
        );
    }
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
        h._directory.sockets(),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    let authority = CurrentRecoveryAuthority {
        members: BTreeMap::from([(
            run,
            RunConfig {
                ownership: Some(owner.start("wire-recovery-thread").unwrap()),
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
    if tools_case {
        restarted
            .send_message(run, "After schema boundary".into())
            .unwrap();
        all_events.extend(through_phase(&mut events, run, AgentRunPhase::Waiting).await);
    }
    restarted.cancel(run).unwrap();
    restarted.wait(run).await.unwrap();
    let requests = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| r.body)
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), if tools_case { 4 } else { 2 });
    for request in &requests[if tools_case { 2 } else { 1 }..] {
        assert_eq!(
            request["tools"], expected_wire_tools,
            "wire tools must exactly match the permitted candidate, including order"
        );
    }
    let after = &requests[if tools_case { 2 } else { 1 }];
    if changed_tool {
        assert_ne!(original["tools"], after["tools"]);
    } else {
        assert_append_only(CacheProtocol::OpenAi, &original, after).unwrap();
    }
    if tools_case {
        assert_append_only(CacheProtocol::OpenAi, after, &requests[3]).unwrap();
    }
    // Independent assertions, not the mock cache's comparison algorithm.
    let prefix = original["messages"].as_array().unwrap();
    assert_eq!(
        &after["messages"].as_array().unwrap()[..prefix.len()],
        prefix
    );
    // Affinity and already-sent ToolResults are independent wire assertions.
    assert_eq!(after["prompt_cache_key"], original["prompt_cache_key"]);
    if tools_case {
        assert!(
            original["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "tool")
        );
    }
    for key in ["model", "temperature", "max_tokens", "tool_choice"] {
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
    assert_eq!(usage.len(), if tools_case { 4 } else { 2 });
    assert!(usage[0].0 > 0);
    assert_eq!(usage[1].1, usage[0].0);
    if tools_case {
        if changed_tool {
            assert!(usage[2].1 < usage[1].0);
        } else {
            assert_eq!(usage[2].1, usage[1].0);
        }
        assert_eq!(usage[3].1, usage[2].0);
    }
}
