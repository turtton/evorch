use super::*;
use crate::benchmark::{BenchmarkCheckpoint, BenchmarkRecorder, BenchmarkSelector};

#[derive(Default)]
struct EditCheckpoint(Mutex<Option<BenchmarkCheckpoint>>);

#[async_trait]
impl BenchmarkRecorder for EditCheckpoint {
    async fn capture(&self, checkpoint: &BenchmarkCheckpoint) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(checkpoint.clone());
        Ok(())
    }
}

#[tokio::test]
async fn benchmark_instruction_list_is_narrowed_but_checkpoint_keeps_all_edit_tools() {
    for apply_patch in [false, true] {
        let (mut model, _) = routed_model(Ok(response()), "mock-model", None);
        model.agents.orchestrator.models = vec![config::ChainEntryConfig {
            route: "worker".into(),
            reasoning_effort: None,
        }];
        model
            .providers
            .get_mut("local")
            .unwrap()
            .profile
            .edit_tool
            .profile = Some(if apply_patch {
            config::EditToolConfig::ApplyPatch
        } else {
            config::EditToolConfig::Edit
        });
        let mut delegation = response();
        delegation.message.content = vec![ContentBlock::ToolUse {
            id: "child".into(),
            name: "delegate".into(),
            input: serde_json::json!({"target":{"role":"worker"}, "prompt":"evaluate"}),
        }];
        delegation.finish_reason = FinishReason::ToolUse;
        let requests =
            super::switch::QueueClient::install(&mut model, "local", vec![Ok(delegation)], false);
        let bus = Arc::new(EventBus::new(1024));
        let root = tempfile::tempdir().unwrap();
        let executor = tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        );
        executor.set_default_cwd(root.path().to_path_buf());
        let recorder = Arc::new(EditCheckpoint::default());
        let runtime = AgentRuntime::new(bus, Arc::new(executor), Arc::new(model))
            .with_project_rules(Arc::new(crate::RulesSource::new(
                crate::ProjectTrust::Approved,
                crate::RulesSettings::from(&config::RulesConfig::default()),
                None,
                Some(root.path().to_path_buf()),
                None,
            )))
            .with_benchmark_recorder(
                BenchmarkSelector {
                    role: Role::Worker,
                    category: None,
                    occurrence: 1,
                },
                recorder.clone(),
            )
            .unwrap();
        let run = runtime.delegate_background(
            Role::Orchestrator,
            "benchmark".into(),
            crate::RunConfig::default(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), runtime.wait(run))
            .await
            .unwrap()
            .unwrap();
        let checkpoint = recorder
            .0
            .lock()
            .unwrap()
            .clone()
            .expect("child checkpoint");
        let names = checkpoint
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        for name in ["apply_patch", "edit", "write"] {
            assert!(names.contains(&name), "{names:?}");
        }
        let instructions = checkpoint
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .find(|text| text.starts_with("Benchmark execution constraints:"))
            .unwrap();
        let list = instructions
            .split("available tools are ")
            .nth(1)
            .unwrap()
            .split('.')
            .next()
            .unwrap()
            .split(", ")
            .collect::<Vec<_>>();
        assert_eq!(list.contains(&"apply_patch"), apply_patch);
        for name in ["edit", "write"] {
            assert_eq!(list.contains(&name), !apply_patch);
        }
        let requests = requests.lock().unwrap();
        let child_requests = requests
            .iter()
            .filter(|request| {
                request.observation.as_ref().is_some_and(|observation| {
                    observation.run_id == checkpoint.origin_run_id.to_string()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(child_requests.len(), 1);
        let names = child_requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names.contains(&"apply_patch"), apply_patch);
        for name in ["edit", "write"] {
            assert_eq!(names.contains(&name), !apply_patch);
        }
    }
}

#[tokio::test]
async fn benchmark_rejects_generation_fields_omitted_by_wire_adapters() {
    for protocol in [
        model::ApiProtocol::OpenAiCodexResponses,
        model::ApiProtocol::AnthropicMessages,
        model::ApiProtocol::OpenAiCompletions,
        model::ApiProtocol::OpenAiResponses,
    ] {
        let (mut model, requests) = routed_model(Ok(response()), "custom", None);
        model
            .providers
            .get_mut("local")
            .unwrap()
            .profile
            .api_protocol = protocol;
        let model = Arc::new(model);
        let mut settings = model
            .benchmark_settings(&AgentInvocationContext::default(), Role::Worker, &[])
            .await
            .unwrap();
        match protocol {
            model::ApiProtocol::AnthropicMessages => {
                settings.preference.reasoning_effort = Some("high".into())
            }
            model::ApiProtocol::OpenAiCompletions | model::ApiProtocol::OpenAiResponses => {
                settings.generation.top_p = Some(0.5)
            }
            // Fixture already has temperature/max_tokens, neither reaches Codex wire.
            _ => {}
        }
        assert!(model.freeze_for_benchmark(settings).is_err());
        assert!(requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn openai_responses_benchmark_replays_every_generation_field() {
    // Given: an OpenAI Responses profile, whose wire keeps temperature, the output
    // limit, reasoning effort and service tier, captured with all four set.
    let (mut model, requests) = routed_model(Ok(response()), "model-a", None);
    let profile = &mut model.providers.get_mut("local").unwrap().profile;
    profile.provider_type = ProviderType::OpenAi;
    profile.api_protocol = model::ApiProtocol::OpenAiResponses;
    let model = Arc::new(model);
    let mut settings = model
        .benchmark_settings(&AgentInvocationContext::default(), Role::Worker, &[])
        .await
        .unwrap();
    settings.preference.reasoning_effort = Some("high".into());
    settings.service_tier = Some(providers::ServiceTier::Priority);
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: String::new(),
        input_schema: serde_json::json!({}),
    }];
    // When: the frozen candidate replays a request.
    let frozen = model.freeze_for_benchmark(settings).unwrap();
    frozen
        .complete(
            &AgentInvocationContext::default(),
            Role::Worker,
            &[],
            &tools,
        )
        .await
        .unwrap();
    // Then: every recorded generation field reaches the provider request.
    let recorded = requests.lock().unwrap();
    assert_eq!(recorded[0].model, "model-a");
    assert_eq!(recorded[0].temperature, Some(0.25));
    assert_eq!(recorded[0].max_tokens, Some(321));
    assert_eq!(recorded[0].reasoning_effort.as_deref(), Some("high"));
    assert_eq!(
        recorded[0].service_tier,
        Some(providers::ServiceTier::Priority)
    );
}

#[tokio::test]
async fn frozen_benchmark_request_changes_only_model_selection() {
    let (model, requests) = routed_model(Ok(response()), "model-a", Some("model-b"));
    let model = Arc::new(model);
    let invocation = AgentInvocationContext::default();
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "read input".into(),
        input_schema: serde_json::json!({"type":"object"}),
    }];
    let messages = vec![Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "captured input".into(),
        }],
    }];
    let captured = model
        .benchmark_settings(&invocation, Role::Worker, &tools)
        .await
        .unwrap();
    model
        .complete(&invocation, Role::Worker, &messages, &tools)
        .await
        .unwrap();
    for candidate in ["model-b", "model-a"] {
        let mut settings = captured.clone();
        settings.preference.model = Some(candidate.into());
        let frozen = model.clone().freeze_for_benchmark(settings).unwrap();
        frozen
            .complete(&invocation, Role::Worker, &messages, &tools)
            .await
            .unwrap();
    }
    let requests = requests.lock().unwrap();
    for request in &requests[1..] {
        assert_eq!(request.messages, requests[0].messages);
        assert_eq!(request.tools, requests[0].tools);
        assert_eq!(request.temperature, Some(0.25));
        assert_eq!(request.max_tokens, Some(321));
        assert_eq!(request.reasoning_effort, requests[0].reasoning_effort);
        assert_eq!(request.service_tier, requests[0].service_tier);
    }
    assert_eq!(requests[0].model, "model-b");
    assert_eq!(requests[1].model, "model-b");
    assert_eq!(requests[2].model, "model-a");
    drop(requests);
    let mut invalid = captured;
    invalid.preference.model = Some("not-configured".into());
    assert!(model.freeze_for_benchmark(invalid).is_err());
}

#[tokio::test]
async fn benchmark_never_silently_strips_tools() {
    let (mut model, requests) = routed_model(Ok(response()), "custom", None);
    let mut catalog = ModelCatalog::new();
    catalog.merge_discovered(vec!["custom".into()]);
    catalog.merge_models_dev(vec![catalog.get("custom").unwrap().clone()]);
    model.router = Router::new(
        vec![profile("custom", &["custom"])],
        &RoutingConfig::default(),
        catalog,
    )
    .unwrap();
    let model = Arc::new(model);
    let invocation = AgentInvocationContext {
        attempts: None,
        model_preference: Some(crate::ModelPreference {
            profile: "local".into(),
            model: Some("custom".into()),
            reasoning_effort: None,
        }),
        ..Default::default()
    };
    let settings = model
        .benchmark_settings(&invocation, Role::Worker, &[])
        .await
        .unwrap();
    let frozen = model.freeze_for_benchmark(settings).unwrap();
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: String::new(),
        input_schema: serde_json::json!({}),
    }];
    assert!(
        frozen
            .complete(&invocation, Role::Worker, &[], &tools)
            .await
            .is_err()
    );
    assert!(requests.lock().unwrap().is_empty());
}
