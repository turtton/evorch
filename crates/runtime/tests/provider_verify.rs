use std::collections::BTreeSet;
use std::sync::Arc;

use config::{Config, ProviderProfileConfig, RouteCandidateConfig};
use event_bus::{EventBus, EventKind};
use mock_openai::StreamingMockOpenAi;
use routing::{ComposeDeps, MapEnv};
use runtime::compose::{RoutedModel, compose_routed_model};
use runtime::{AgentInvocationContext, AgentModel, ModelPreference, Role, RuntimeError};
use sandbox::credential::FileCredentialStore;

fn model(urls: &[(&str, String)], bus: Arc<EventBus>) -> Arc<RoutedModel> {
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    for (name, url) in urls {
        let profile = ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAiCompatible,
            api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
            base_url: url.clone(),
            credential: config::CredentialRefConfig::Env { var: "KEY".into() },
            models: vec![config::types::provider::ModelEntryConfig::enabled("gpt-4o")],
            excluded_models: vec![],
            default_model: "gpt-4o".into(),
            edit_tool: None,
        };
        config.providers.insert((*name).into(), profile);
    }
    config.routing.routes.insert(
        "worker".into(),
        urls.iter()
            .map(|(name, _)| RouteCandidateConfig {
                profile: (*name).into(),
                model: None,
                auto: false,
            })
            .collect(),
    );
    compose_routed_model(
        &config,
        ComposeDeps {
            credential_store: Arc::new(
                FileCredentialStore::open(directory.path().join("credentials")).unwrap(),
            ),
            event_bus: Some(bus),
            env: Arc::new(MapEnv::from_iter([("KEY", "fixture-key")])),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions::default(),
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap()
}

#[tokio::test]
async fn verified_candidates_include_fallbacks_after_double_ping() {
    // Given: two independent candidate endpoints.
    let primary = StreamingMockOpenAi::spawn(vec![]);
    let fallback = StreamingMockOpenAi::spawn(vec![]);
    let model = model(
        &[
            ("primary", primary.base_url()),
            ("fallback", fallback.base_url()),
        ],
        Arc::new(EventBus::new(16)),
    );
    // When: concurrent verification and a repeated check share one composition.
    let (first, second) = tokio::join!(model.verify_candidates(), model.verify_candidates());
    // Then: both candidates are verified with exactly one ping per endpoint.
    let expected = BTreeSet::from(["primary".to_owned(), "fallback".to_owned()]);
    assert_eq!(first, expected);
    assert_eq!(second, expected);
    for server in [&primary, &fallback] {
        let requests = server.recorded_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/v1/models");
    }
}

#[tokio::test]
async fn unreachable_profile_marks_unverified() {
    // Given: a reserved, non-listening port.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let model = model(
        &[(
            "dead",
            format!("http://{}/v1", socket.local_addr().unwrap()),
        )],
        Arc::new(EventBus::new(16)),
    );
    // When: candidates are checked.
    let verified = model.verify_candidates().await;
    // Then: the unreachable profile is excluded.
    assert_eq!(verified, BTreeSet::new());
}

#[tokio::test]
async fn streaming_route_is_rejected_before_any_completion_request() {
    // Given: a routed provider whose catalog is unavailable.
    let server = StreamingMockOpenAi::spawn(vec![]);
    let bus = Arc::new(EventBus::new(16));
    let model = model(
        &[("fallback", format!("{}/missing", server.base_url()))],
        Arc::clone(&bus),
    );
    let mut receiver = bus.subscribe();
    // When: automatic routing reaches the streaming surface without a prior verification call.
    let result = model
        .complete_streaming(
            &AgentInvocationContext::default(),
            Role::Worker,
            &[],
            &[],
            &bus,
        )
        .await;
    // Then: verification blocks streaming and publishes just one diagnostic.
    assert!(matches!(result, Err(RuntimeError::Model { .. })));
    assert!(
        matches!(receiver.recv().await.unwrap().kind, EventKind::Diagnostic(event)
        if event.code == event_bus::event::diagnostic_codes::PROVIDER_UNAVAILABLE)
    );
    bus.emit(event_bus::Event::new(
        event_bus::MessageEvent::MessageDelta {
            delta: String::new(),
            run_id: None,
        },
    ));
    assert!(matches!(
        receiver.recv().await.unwrap().kind,
        EventKind::Message(_)
    ));
    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
}

#[tokio::test]
async fn unverified_fallback_never_spawned_and_emits_provider_unavailable() {
    // Given: primary verifies, fallback's models path returns 500.
    let primary = StreamingMockOpenAi::spawn(vec![]);
    let fallback = StreamingMockOpenAi::spawn(vec![]);
    let bus = Arc::new(EventBus::new(16));
    let model = model(
        &[
            ("primary", primary.base_url()),
            ("fallback", format!("{}/unavailable", fallback.base_url())),
        ],
        Arc::clone(&bus),
    );
    assert_eq!(
        model.verify_candidates().await,
        BTreeSet::from(["primary".into()])
    );
    let mut receiver = bus.subscribe();
    let invocation = AgentInvocationContext {
        attempts: None,
        run_id: "verify-run".into(),
        model_preference: Some(ModelPreference {
            profile: "fallback".into(),
            model: None,
            reasoning_effort: None,
        }),
        ..Default::default()
    };
    // When: explicitly selecting the failed fallback through the non-streaming surface.
    let result = model.complete(&invocation, Role::Worker, &[], &[]).await;
    // Then: no completion request and exactly one correlated diagnostic.
    assert!(matches!(result, Err(RuntimeError::Model { .. })));
    let EventKind::Diagnostic(event) = receiver.recv().await.unwrap().kind else {
        panic!("diagnostic expected")
    };
    assert_eq!(
        event.code,
        event_bus::event::diagnostic_codes::PROVIDER_UNAVAILABLE
    );
    assert_eq!(event.severity, event_bus::DiagnosticSeverity::Error);
    assert_eq!(event.run_id.as_deref(), Some("verify-run"));
    assert!(event.detail.contains("fallback"));
    assert!(event.detail.contains("gpt-4o"));
    assert!(event.detail.contains("500"));
    bus.emit(event_bus::Event::new(
        event_bus::MessageEvent::MessageDelta {
            delta: String::new(),
            run_id: None,
        },
    ));
    assert!(matches!(
        receiver.recv().await.unwrap().kind,
        EventKind::Message(_)
    ));
    assert_eq!(fallback.recorded_requests().len(), 1);
    assert_eq!(fallback.recorded_requests()[0].method, "GET");
}

#[tokio::test]
async fn identical_endpoint_and_auth_share_verification() {
    // Given: two profiles using identical endpoint credentials.
    let server = StreamingMockOpenAi::spawn(vec![]);
    let model = model(
        &[
            ("primary", server.base_url()),
            ("fallback", server.base_url()),
        ],
        Arc::new(EventBus::new(16)),
    );
    // When: checking all candidates.
    let verified = model.verify_candidates().await;
    // Then: both profiles share a single authenticated probe.
    assert_eq!(
        verified,
        BTreeSet::from(["primary".into(), "fallback".into()])
    );
    assert_eq!(server.recorded_requests().len(), 1);
}

/// An ID token for account `account` that expires far in the future.
const FRESH_ID_TOKEN: &str = "e30.eyJleHAiOjQwMDAwMDAwMDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2NvdW50In19.sig";

async fn mount_codex_catalog(server: &wiremock::MockServer, access_token: &str) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/models"))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {access_token}"),
        ))
        .and(wiremock::matchers::header("chatgpt-account-id", "account"))
        .and(wiremock::matchers::query_param(
            "client_version",
            providers::CODEX_MODELS_FALLBACK_VERSION,
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"models": [{"slug": "gpt-4o"}]})),
        )
        .expect(1)
        .mount(server)
        .await;
}

fn codex_model(
    server: &wiremock::MockServer,
    store: Arc<FileCredentialStore>,
    id_token: &str,
) -> Arc<RoutedModel> {
    use sandbox::credential::CredentialStore;
    store
        .set(
            "codex",
            &sandbox::credential::Secret::from(
                serde_json::json!({
                    "access_token": "access-token",
                    "refresh_token": "refresh-token",
                    "id_token": id_token,
                })
                .to_string(),
            ),
        )
        .unwrap();
    let mut config = Config::default();
    config.providers.insert(
        "codex".into(),
        ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAiCodex,
            api_protocol: config::ApiProtocolConfig::OpenAiCodexResponses,
            base_url: server.uri(),
            credential: config::CredentialRefConfig::Keyring {
                service: "evorch".into(),
                account: "codex".into(),
            },
            models: vec![config::types::provider::ModelEntryConfig::enabled("gpt-4o")],
            excluded_models: vec![],
            default_model: "gpt-4o".into(),
            edit_tool: None,
        },
    );
    compose_routed_model(
        &config,
        ComposeDeps {
            credential_store: store,
            event_bus: None,
            env: Arc::new(MapEnv::from_iter([] as [(&str, &str); 0])),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions {
                auth_base_url_override: Some(server.uri()),
                codex_client_version: Some(providers::CodexClientVersion::Fixed(
                    providers::CodexCatalogVersion {
                        version: providers::CODEX_MODELS_FALLBACK_VERSION.into(),
                        warning: None,
                    },
                )),
                ..Default::default()
            },
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap()
}

#[tokio::test]
async fn codex_verification_uses_stored_oauth_and_account() {
    // Given: a Codex endpoint requiring its catalog headers and stored, unexpired credentials.
    let server = wiremock::MockServer::start().await;
    mount_codex_catalog(&server, "access-token").await;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    let model = codex_model(&server, store, FRESH_ID_TOKEN);
    // When: checking the Codex profile.
    let verified = model.verify_candidates().await;
    // Then: OAuth-authenticated discovery verifies it.
    assert_eq!(verified, BTreeSet::from(["codex".into()]));
}

#[tokio::test]
async fn codex_verification_refreshes_expired_credentials() {
    // Given: stored Codex credentials whose ID token has expired, as on an idle fallback profile.
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/oauth/token"))
        .and(wiremock::matchers::body_partial_json(
            serde_json::json!({"grant_type": "refresh_token", "refresh_token": "refresh-token"}),
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "fresh-access",
                "refresh_token": "fresh-refresh",
                "id_token": FRESH_ID_TOKEN,
            })),
        )
        .expect(1)
        .mount(&server)
        .await;
    mount_codex_catalog(&server, "fresh-access").await;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    let expired = "e30.eyJleHAiOjEsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2NvdW50In19.sig";
    let model = codex_model(&server, Arc::clone(&store), expired);
    // When: checking the Codex profile.
    let verified = model.verify_candidates().await;
    // Then: discovery uses the refreshed token and keeps the rotated bundle for requests.
    assert_eq!(verified, BTreeSet::from(["codex".into()]));
    use sandbox::credential::CredentialStore;
    let stored = store.get("codex").unwrap().unwrap();
    assert!(stored.expose().contains("fresh-refresh"));
}
