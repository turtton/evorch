use std::{collections::BTreeMap, sync::Arc, time::Duration};

use mock_openai::{ScriptedResponse, StreamingMockOpenAi, WriteMode};
use model::{ApiProtocol, ModelCatalog, ProviderType};
use providers::{ProviderAuth, ToolSpec, provider::openai_compatible::OpenAiCompatibleClient};
use routing::{ComposedProvider, ComposedProviders, CredentialRef, ProviderProfile, Router};
use runtime::{AgentInvocationContext, AgentModel, ModelPreference, Role, RoutedModel};
use serde_json::json;

#[tokio::test]
async fn tools_reach_wire_when_preferred_model_is_absent_from_catalog() {
    // Given: an unknown model explicitly selected on a tool-capable HTTP provider.
    let catalog = ModelCatalog::new();
    assert!(catalog.get("custom-tool-model").is_none());
    let server = StreamingMockOpenAi::spawn_with_models(
        vec![ScriptedResponse::text_stream(
            "reply",
            "custom-tool-model",
            ["done"],
        )],
        WriteMode::default(),
        vec!["custom-tool-model".into()],
    );
    let profile = ProviderProfile {
        name: "local".into(),
        provider_type: ProviderType::OpenAiCompatible,
        api_protocol: ApiProtocol::OpenAiCompletions,
        base_url: server.base_url(),
        credential: CredentialRef::Env {
            var: "TEST_KEY".into(),
        },
        models: vec!["custom-tool-model".into()],
        default_model: "custom-tool-model".into(),
    };
    let router = Router::new(
        vec![profile.clone()],
        &config::RoutingConfig::default(),
        catalog,
    )
    .expect("valid router");
    let client =
        OpenAiCompatibleClient::new(server.base_url(), "local", Duration::from_secs(5), None)
            .expect("HTTP client");
    let model = RoutedModel::new(
        ComposedProviders {
            router,
            providers: BTreeMap::from([(
                "local".into(),
                ComposedProvider {
                    profile,
                    client: Arc::new(client),
                    auth: ProviderAuth::new("test-key"),
                },
            )]),
        },
        config::AgentsConfig::default(),
    );

    // When: the real composition adapter sends a request with a tool definition.
    model
        .complete(
            &AgentInvocationContext {
                category: None,
                run_id: "unknown-tools".into(),
                model_preference: Some(ModelPreference {
                    profile: "local".into(),
                    model: Some("custom-tool-model".into()),
                }),
            },
            Role::Worker,
            &[],
            &[ToolSpec {
                name: "read".into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
            }],
        )
        .await
        .expect("completion succeeds");

    // Then: tools survive catalog uncertainty all the way to the HTTP body.
    let recorded = server.recorded_requests();
    assert_eq!(recorded[0].method, "GET");
    assert_eq!(recorded[0].path, "/v1/models");
    let requests: Vec<_> = recorded
        .iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .collect();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty()),
        "catalog-unknown model must receive a non-empty tools array: {}",
        requests[0].body
    );
    assert_eq!(requests[0].body["tools"][0]["function"]["name"], "read");
}
