use super::*;

#[test]
fn benchmark_rejects_generation_fields_omitted_by_wire_adapters() {
    for protocol in [
        model::ApiProtocol::OpenAiCodexResponses,
        model::ApiProtocol::AnthropicMessages,
        model::ApiProtocol::OpenAiCompletions,
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
            .unwrap();
        match protocol {
            model::ApiProtocol::AnthropicMessages => {
                settings.generation.reasoning_effort = Some("high".into())
            }
            model::ApiProtocol::OpenAiCompletions => settings.generation.top_p = Some(0.5),
            // Fixture already has temperature/max_tokens, neither reaches Codex wire.
            _ => {}
        }
        assert!(model.freeze_for_benchmark(settings).is_err());
        assert!(requests.lock().unwrap().is_empty());
    }
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
        model_preference: Some(crate::ModelPreference {
            profile: "local".into(),
            model: Some("custom".into()),
        }),
        ..Default::default()
    };
    let settings = model
        .benchmark_settings(&invocation, Role::Worker, &[])
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
