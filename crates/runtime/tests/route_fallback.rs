use std::sync::Arc;

use event_bus::{EventBus, EventKind, ProviderEvent};
use runtime::{AgentInvocationContext, AgentModel, Role};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn http_failure_switches_provider_and_keeps_session_on_fallback() {
    // Given: real HTTP adapters, a failing primary and a healthy fallback.
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;
    for server in [&primary, &secondary] {
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list", "data": [{"id": "gpt-4o", "object": "model"}]
            })))
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .expect(1)
        .mount(&primary)
        .await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "fallback", "object": "chat.completion", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "recovered"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))).expect(2).mount(&secondary).await;
    let mut config = config::Config::default();
    for (name, server) in [("primary", &primary), ("secondary", &secondary)] {
        config.providers.insert(
            name.into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAiCompatible,
                api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
                base_url: server.uri(),
                credential: config::CredentialRefConfig::Env { var: "KEY".into() },
                models: vec![config::types::provider::ModelEntryConfig::enabled("gpt-4o")],
                default_model: "gpt-4o".into(),
                ..Default::default()
            },
        );
    }
    config.routing.routes.insert(
        "worker".into(),
        ["primary", "secondary"]
            .map(|name| config::RouteCandidateConfig {
                profile: name.into(),
                model: None,
            })
            .to_vec(),
    );
    let directory = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let model = runtime::compose::compose_routed_model(
        &config,
        routing::ComposeDeps {
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(
                    directory.path().join("credentials"),
                )
                .unwrap(),
            ),
            event_bus: Some(bus.clone()),
            env: Arc::new(routing::MapEnv::from_iter([("KEY", "fixture-key")])),
            catalog: model::ModelCatalog::builtin(),
            factory: Default::default(),
        },
    )
    .unwrap();
    let invocation = AgentInvocationContext {
        run_id: "http-session".into(),
        ..Default::default()
    };
    // When
    let first = model
        .complete(&invocation, Role::Worker, &[], &[])
        .await
        .unwrap();
    let second = model
        .complete(&invocation, Role::Worker, &[], &[])
        .await
        .unwrap();
    // Then
    assert_eq!(first, second);
    assert!(
        matches!(&first.message.content[0], providers::ContentBlock::Text { text } if text == "recovered")
    );
    let fallback = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let EventKind::Provider(event @ ProviderEvent::FallbackTriggered { .. }) =
                receiver.recv().await.unwrap().kind
            {
                break event;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(fallback, ProviderEvent::FallbackTriggered { from_provider, to_provider, .. }
        if from_provider == "primary" && to_provider == "secondary")
    );
}
