//! Real RoutedModel + HTTP recovery, independently checking the recorded wire prefix.
use super::*;
use runtime::quiesce::{RecoveryRecipe, ResumePolicy};
use runtime::{AgentModel, CurrentRecoveryAuthority};
use std::collections::BTreeMap;

mod output_attachment_schema;
mod plan_schema;
mod wait_schema;

#[derive(Clone, Copy)]
enum SchemaCase {
    None,
    Wait,
    Plan,
    PlanUnchanged,
    PlanRetry,
    OutputAttachments,
    OutputAttachmentsUnchanged,
}

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
        .routing
        .routes
        .get_mut("orchestrator")
        .unwrap()
        .push(config::RouteCandidateConfig {
            profile: "fallback".into(),
            model: None,
            auto: false,
        });
    current.agents.orchestrator.generation.temperature = Some(0.25);
    current.agents.orchestrator.generation.max_tokens = Some(321);
    current
}

fn process_with_read(
    h: &Harness,
    current: &Config,
    storage: &storage::Storage,
    config: &storage::StorageConfig,
    read: Arc<dyn Tool>,
) -> (AgentRuntime, Arc<dyn AgentModel>, EventReceiver) {
    process_with_catalog(h, current, storage, config, read, None)
}

fn process_with_catalog(
    h: &Harness,
    current: &Config,
    storage: &storage::Storage,
    config: &storage::StorageConfig,
    read: Arc<dyn Tool>,
    predecessor: Option<Arc<wait_schema::PredecessorCatalog>>,
) -> (AgentRuntime, Arc<dyn AgentModel>, EventReceiver) {
    let bus = Arc::new(EventBus::new(1024));
    let receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(read).unwrap();
    let credentials =
        Arc::new(FileCredentialStore::open(h._directory.path().join("credentials")).unwrap());
    let model: Arc<dyn AgentModel> = if matches!(h.wire, Wire::Codex) {
        codex_model(current, &credentials, &bus, &h.mock.codex_base_url())
    } else {
        runtime::compose::compose_routed_model(
            current,
            routing::ComposeDeps {
                credential_store: credentials,
                event_bus: Some(bus.clone()),
                env: Arc::new(MapEnv::from_iter([(KEY_ENV, "offline-test-key")])),
                catalog: model::ModelCatalog::new(),
                factory: routing::factory::FactoryOptions::default(),
            },
            Some(test_support::model_catalog_dir()),
        )
        .unwrap()
    };
    let execution: Arc<dyn AgentModel> = match predecessor {
        Some(catalog) => Arc::new(wait_schema::PredecessorModel {
            inner: model.clone(),
            catalog,
        }),
        None => model.clone(),
    };
    let runtime = AgentRuntime::new(bus, Arc::new(executor), execution)
        .with_run_store(runtime::RunStore::open(config, storage.handle()).unwrap())
        .with_system_prompts(contract_prompts())
        .with_compaction(current.compaction.clone());
    runtime.set_thread_scratch(Arc::new(runtime::scratch::ThreadScratch::at(
        h._directory.path().join("attachment-scratch"),
    )));
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
    recover_case(
        changed_settings,
        false,
        false,
        false,
        Wire::ChatCompletions,
        SchemaCase::None,
    )
    .await;
}

#[tokio::test]
async fn planned_tool_schema_boundary_preserves_sent_history_and_reuses_the_new_prefix() {
    for (different_build, changed_tool) in
        [(false, false), (true, false), (true, true), (false, true)]
    {
        recover_case(
            false,
            true,
            different_build,
            changed_tool,
            Wire::ChatCompletions,
            SchemaCase::None,
        )
        .await;
    }
}

struct RecoveryRead {
    path: std::path::PathBuf,
    changed: bool,
}
#[async_trait::async_trait]
impl Tool for RecoveryRead {
    fn name(&self) -> &'static str {
        BulkRead.name()
    }
    fn schema(&self) -> Value {
        if self.changed {
            changed_schema()
        } else {
            BulkRead.schema()
        }
    }
    fn permissions(&self) -> Permissions {
        BulkRead.permissions()
    }
    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        if input["index"] == 7 {
            tools::Read.execute(json!({"path": self.path})).await
        } else {
            BulkRead.execute(input).await
        }
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
    wire: Wire,
    schema_case: SchemaCase,
) {
    let wait_schema = matches!(schema_case, SchemaCase::Wait);
    let output_attachments = matches!(schema_case, SchemaCase::OutputAttachments);
    let plan_schema = matches!(
        schema_case,
        SchemaCase::Plan | SchemaCase::PlanUnchanged | SchemaCase::PlanRetry
    );
    let retry_boundary = matches!(schema_case, SchemaCase::PlanRetry);
    let schema_change = wait_schema
        || output_attachments
        || matches!(schema_case, SchemaCase::Plan | SchemaCase::PlanRetry);
    let mut script = if tools_case {
        vec![
            read_response(0),
            text_response("completed"),
            text_response("followup"),
            text_response("reuse"),
        ]
    } else {
        vec![text_response("completed"), text_response("followup")]
    };
    // Retain all original turns, then read the same path on the successor and
    // check reuse on one more turn after that read.
    script.extend([
        read_response(7),
        text_response("attachment read"),
        text_response("reuse after read"),
    ]);
    if output_attachments {
        script.insert(0, output_attachment_schema::legacy_call());
    }
    if wait_schema {
        // The successor rejects this retired call, but its historical name, input,
        // call ID and error result must still remain byte-for-byte in the prefix.
        script.insert(
            0,
            ScriptedResponse::tool_call(
                "legacy-response",
                MODEL,
                0,
                "legacy-wait-reply",
                "wait_reply",
                [json!({"message_id":"msg-old", "timeout_ms":0}).to_string()],
            ),
        );
    }
    let (response_gate, response_arrived) = mock_openai::ResponseGate::new();
    if plan_schema {
        if schema_change {
            script.insert(0, plan_schema::legacy_call());
        }
        let boundary = if schema_change { 3 } else { 2 };
        script[boundary] = super::plan_cache::read("boundary-plan-read");
        if retry_boundary {
            script.insert(
                boundary,
                super::plan_cache::read("boundary-plan-read").with_gate(response_gate.clone()),
            );
        }
    }
    let h = harness_with_wire(
        wire,
        script,
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.9,
        |_| {},
    );
    let mut current = settings(&h);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("activation.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let predecessor = schema_change.then(|| {
        Arc::new(wait_schema::PredecessorCatalog {
            plan: plan_schema,
            output_attachments,
            ..Default::default()
        })
    });
    let (old, old_model, mut events) = process_with_catalog(
        &h,
        &current,
        &storage,
        &config,
        Arc::new(BulkRead),
        predecessor.clone(),
    );
    let attachment = CacheAttachment::new(&old, "wire-recovery-thread");
    let prompt = attachment.prompt("Keep original task");
    // Current-thread test: pin is installed before the admitted task is polled.
    let run = old
        .delegate_chat(
            "wire-recovery-thread",
            if schema_change {
                Role::Orchestrator
            } else {
                Role::Worker
            },
            prompt.clone(),
            RunConfig {
                conversation: plan_schema,
                interactive: true,
                keep_alive: true,
                ..RunConfig::default()
            },
        )
        .unwrap();
    let pin = runtime::RoutePinRecord {
        chain: if schema_change {
            "orchestrator"
        } else {
            "worker"
        }
        .into(),
        position: 1,
        profile: "fallback".into(),
        model: MODEL.into(),
    };
    // Persist a real fallback pin. Its production creation path is independently
    // covered by compose::tests::checkpoint::recovery_validates_fallback_pin… .
    old_model.restore_route_pins(&run.to_string(), std::slice::from_ref(&pin));
    let mut all_events = through_phase(&mut events, run, AgentRunPhase::Waiting).await;
    if plan_schema {
        old.stop(run, runtime::StopScope::SelfOnly).unwrap();
        all_events.extend(through_phase(&mut events, run, AgentRunPhase::Stopped).await);
        old.wait(run).await.unwrap();
        super::plan_cache::age_record(&config, run);
        old.resume_chat(
            run,
            RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
        all_events.extend(through_phase(&mut events, run, AgentRunPhase::Waiting).await);
    }
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
        .filter(|r| r.path == h.wire.path())
        .collect::<Vec<_>>();
    assert_eq!(
        before.len(),
        if schema_change {
            3
        } else if tools_case {
            2
        } else {
            1
        }
    );
    let after_index = before.len();
    let original = before.last().unwrap().body.clone();
    if changed_settings {
        current.agents.worker.base.generation.temperature = Some(0.75);
    }
    let (mut restarted, model, mut events) = process_with_read(
        &h,
        &current,
        &storage,
        &config,
        Arc::new(RecoveryRead {
            path: attachment.path.clone(),
            changed: changed_tool,
        }),
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
        if let Some(predecessor) = &predecessor {
            candidate.request.tools = predecessor.successor_tools.lock().unwrap().clone();
        }
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
        if (changed_tool || schema_change) && !different_build {
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
        assert_eq!(evidence.boundary, changed_tool || schema_change);
        if wait_schema {
            wait_schema::assert_evidence(&evidence);
        }
        if plan_schema && schema_change {
            plan_schema::assert_evidence(&evidence);
        }
        if changed_tool || schema_change {
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
        let permitted = if changed_tool || schema_change {
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
        expected_wire_tools = wait_schema::wire_tools(h.wire, &permitted.request.tools);
        if schema_change {
            let (saved, consumed) = fixture_db
                .restart_tools_evidence("routed-wire", &run.to_string(), incarnation)
                .unwrap()
                .unwrap();
            if wait_schema {
                wait_schema::assert_evidence(&saved);
            }
            if plan_schema {
                plan_schema::assert_evidence(&saved);
            }
            assert!(
                consumed,
                "schema boundary evidence must be durably consumed"
            );
        }
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
                conversation: plan_schema,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )]),
    };
    if plan_schema {
        // The core renews the trusted thread binding before activation
        // (recovery_startup.rs); a current permit alone is not that binding.
        restarted
            .bind_thread_root("wire-recovery-thread", run)
            .unwrap();
    }
    let result = ({
        let reservation = restarted
            .reserve_automatic_activation(&(paused).claim().manifest_id)
            .unwrap();
        restarted.activate_component(paused, authority, reservation)
    });
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
                .filter(|r| r.path == h.wire.path())
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
    if retry_boundary {
        tokio::task::spawn_blocking(move || response_arrived.recv().unwrap())
            .await
            .unwrap();
        restarted.stop(run, runtime::StopScope::SelfOnly).unwrap();
        all_events.extend(through_phase(&mut events, run, AgentRunPhase::Stopped).await);
        restarted.wait(run).await.unwrap();
        response_gate.release();
        let saved = storage::Database::open(&config)
            .unwrap()
            .run_context(&run.to_string())
            .unwrap()
            .unwrap();
        let descriptor: Value = serde_json::from_str(&saved.config_json).unwrap();
        assert_eq!(descriptor["plan_notice"], "deferred");
        let (fresh, _, fresh_events) = process_with_read(
            &h,
            &current,
            &storage,
            &config,
            Arc::new(RecoveryRead {
                path: attachment.path.clone(),
                changed: changed_tool,
            }),
        );
        restarted = fresh;
        events = fresh_events;
        restarted
            .resume_chat(
                run,
                RunConfig {
                    conversation: true,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    all_events.extend(through_phase(&mut events, run, AgentRunPhase::Waiting).await);
    if tools_case && !plan_schema {
        restarted
            .send_message(run, "After schema boundary".into())
            .unwrap();
        all_events.extend(through_phase(&mut events, run, AgentRunPhase::Waiting).await);
    }
    attachment.assert_readable(&restarted).await;
    restarted
        .send_message(run, "Read the original attachment path".into())
        .unwrap();
    all_events.extend(through_phase(&mut events, run, AgentRunPhase::Waiting).await);
    restarted
        .send_message(run, "Reuse after attachment read".into())
        .unwrap();
    all_events.extend(through_phase(&mut events, run, AgentRunPhase::Waiting).await);
    restarted.cancel(run).unwrap();
    restarted.wait(run).await.unwrap();
    let mut requests = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == h.wire.path())
        .map(|r| r.body)
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        after_index + if tools_case { 2 } else { 1 } + 3 + usize::from(retry_boundary)
    );
    if retry_boundary {
        assert_eq!(
            requests[after_index],
            requests[after_index + 1],
            "interrupted boundary must retry without adding a notice"
        );
        requests.remove(after_index);
    }
    for request in &requests[after_index..] {
        assert_eq!(
            request["tools"], expected_wire_tools,
            "wire tools must exactly match the permitted candidate, including order"
        );
    }
    let after = &requests[after_index];
    if changed_tool || schema_change {
        assert_ne!(original["tools"], after["tools"]);
    } else {
        assert_append_only(h.wire.protocol(), &original, after).unwrap();
    }
    if tools_case {
        assert_append_only(h.wire.protocol(), after, &requests[after_index + 1]).unwrap();
    }
    // Independent assertions, not the mock cache's comparison algorithm.
    let history_key = if matches!(h.wire, Wire::ChatCompletions) {
        "messages"
    } else {
        "input"
    };
    assert!(
        original[history_key]
            .to_string()
            .contains(&attachment.path.display().to_string())
    );
    for pair in requests[after_index..].windows(2) {
        assert_append_only(h.wire.protocol(), &pair[0], &pair[1]).unwrap();
        assert_eq!(pair[1]["prompt_cache_key"], original["prompt_cache_key"]);
    }
    assert!(
        requests.last().unwrap()[history_key]
            .to_string()
            .contains("persistent attachment cache fixture"),
        "the successor must return the contents from the original attachment path"
    );
    let prefix = original[history_key].as_array().unwrap();
    assert_eq!(
        &after[history_key].as_array().unwrap()[..prefix.len()],
        prefix
    );
    // Affinity and already-sent ToolResults are independent wire assertions.
    assert_eq!(after["prompt_cache_key"], original["prompt_cache_key"]);
    if tools_case {
        assert!(
            original[history_key]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "tool" || m["type"] == "function_call_output")
        );
    }
    if wait_schema {
        wait_schema::assert_transition(
            h.wire,
            &requests,
            after_index,
            &old_input.request.tools,
            &predecessor
                .as_ref()
                .unwrap()
                .successor_tools
                .lock()
                .unwrap(),
        );
    }
    if output_attachments {
        output_attachment_schema::assert_transition(
            h.wire,
            &requests,
            after_index,
            &old_input.request.tools,
            &predecessor
                .as_ref()
                .unwrap()
                .successor_tools
                .lock()
                .unwrap(),
        );
    }
    if plan_schema && schema_change {
        plan_schema::assert_transition(
            h.wire,
            &requests,
            after_index,
            &old_input.request.tools,
            &predecessor
                .as_ref()
                .unwrap()
                .successor_tools
                .lock()
                .unwrap(),
            true,
        );
    }
    for key in [
        "model",
        "temperature",
        "max_tokens",
        "max_output_tokens",
        "tool_choice",
        "instructions",
        "reasoning",
        "service_tier",
    ] {
        assert_eq!(after[key], original[key], "changed {key}");
    }
    if plan_schema && !schema_change {
        plan_schema::assert_same_build_notice(h.wire, &requests, after_index);
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
    assert_eq!(usage.len(), requests.len());
    assert!(usage[0].0 > 0);
    for pair in usage[..after_index].windows(2) {
        assert_eq!(pair[1].1, pair[0].0);
    }
    if retry_boundary {
        // The input-derived mock has seen the interrupted boundary request,
        // so its identical replay reuses that new-schema prefix in full.
        assert_eq!(usage[after_index].1, usage[after_index].0);
    } else if changed_tool || schema_change {
        assert!(usage[after_index].1 < usage[after_index - 1].0);
    } else {
        assert_eq!(usage[after_index].1, usage[after_index - 1].0);
    }
    if tools_case {
        assert_eq!(usage[after_index + 1].1, usage[after_index].0);
    }
    for pair in usage[after_index..].windows(2) {
        assert_eq!(
            pair[1].1, pair[0].0,
            "successor must reuse the path-bearing prefix, including the real read result"
        );
    }
}
