use std::{collections::BTreeMap, sync::Arc, time::Duration};

use mock_openai::{ScriptedResponse, StreamingMockOpenAi, WriteMode};
use model::{ApiProtocol, ModelCatalog, ProviderType};
use providers::provider::{
    anthropic::{AnthropicClient, AnthropicConfig},
    openai_compatible::OpenAiCompatibleClient,
};
use providers::{ContentBlock, Message, ProviderAuth, ToolSpec};
use routing::{ComposedProvider, ComposedProviders, CredentialRef, ProviderProfile, Router};
use runtime::{AgentInvocationContext, AgentModel, ModelPreference, Role, RoutedModel};
use serde_json::json;

#[path = "benchmark_wire/cache.rs"]
mod cache;

#[tokio::test]
async fn benchmark_preserves_http_payload_and_rejects_cross_protocol_before_request() {
    let server = StreamingMockOpenAi::spawn_with_models(
        vec![
            ScriptedResponse::text_stream("a", "model-a", ["done"]),
            ScriptedResponse::text_stream("b", "model-b", ["done"]),
        ],
        WriteMode::default(),
        vec!["model-a".into(), "model-b".into()],
    );
    let profile = ProviderProfile {
        name: "local".into(),
        provider_type: ProviderType::OpenAiCompatible,
        api_protocol: ApiProtocol::OpenAiCompletions,
        base_url: server.base_url(),
        credential: CredentialRef::Env {
            var: "TEST_KEY".into(),
        },
        models: vec!["model-a".into(), "model-b".into()],
        default_model: "model-a".into(),
    };
    let alternate = ProviderProfile {
        name: "alternate".into(),
        provider_type: ProviderType::Anthropic,
        api_protocol: ApiProtocol::AnthropicMessages,
        ..profile.clone()
    };
    let mut catalog = ModelCatalog::new();
    catalog.merge_discovered(vec!["model-a".into(), "model-b".into()]);
    let router = Router::new(
        vec![profile.clone(), alternate.clone()],
        &config::RoutingConfig {
            routes: BTreeMap::from([(
                "worker".into(),
                vec![config::RouteCandidateConfig {
                    profile: "local".into(),
                    model: Some("model-a".into()),
                }],
            )]),
        },
        catalog,
    )
    .unwrap();
    let client =
        OpenAiCompatibleClient::new(server.base_url(), "local", Duration::from_secs(30), None)
            .unwrap();
    let alternate_client = AnthropicClient::new(AnthropicConfig {
        base_url: server.base_url(),
        ..Default::default()
    })
    .unwrap();
    let mut agents = config::AgentsConfig::default();
    agents.worker.base.generation = config::GenerationOverridesConfig {
        temperature: Some(0.25),
        max_tokens: Some(321),
        ..Default::default()
    };
    let model = Arc::new(RoutedModel::new(
        ComposedProviders {
            router,
            providers: BTreeMap::from([
                (
                    "local".into(),
                    ComposedProvider {
                        profile,
                        client: Arc::new(client),
                        auth: ProviderAuth::new("test-key"),
                    },
                ),
                (
                    "alternate".into(),
                    ComposedProvider {
                        profile: alternate,
                        client: Arc::new(alternate_client),
                        auth: ProviderAuth::new("test-key"),
                    },
                ),
            ]),
        },
        agents,
    ));
    let invocation = AgentInvocationContext {
        run_id: "trial".into(),
        ..Default::default()
    };
    let messages = vec![Message {
        role: providers::Role::User,
        content: vec![ContentBlock::Text {
            text: "frozen delegation input".into(),
        }],
    }];
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "frozen tool".into(),
        input_schema: json!({"type":"object"}),
    }];
    let captured = model
        .benchmark_settings(&invocation, Role::Worker, &tools)
        .unwrap();
    for candidate in ["model-a", "model-b"] {
        let mut settings = captured.clone();
        settings.preference.model = Some(candidate.into());
        model
            .clone()
            .freeze_for_benchmark(settings)
            .unwrap()
            .complete(&invocation, Role::Worker, &messages, &tools)
            .await
            .unwrap();
    }
    let recorded = server.recorded_requests();
    let requests: Vec<_> = recorded
        .iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .collect();
    assert_eq!(requests.len(), 2);
    let mut expected = requests[0].body.clone();
    expected["model"] = json!("model-b");
    assert_eq!(
        requests[1].body, expected,
        "serialized input, tools/order and generation remain unchanged"
    );
    assert_eq!(expected["temperature"], json!(0.25));
    assert_eq!(expected["max_tokens"], json!(321));
    let mut incompatible = captured;
    incompatible.preference = ModelPreference {
        profile: "alternate".into(),
        model: Some("model-a".into()),
    };
    assert!(model.freeze_for_benchmark(incompatible).is_err());
    assert_eq!(
        server.recorded_requests().len(),
        recorded.len(),
        "cross-protocol trial sends no HTTP request"
    );
}
