//! Missing routes are rejected at use time, not during composition.

use std::collections::BTreeMap;
use std::sync::Arc;

use config::{CategoryBindingConfig, Config, RouteCandidateConfig};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::{ComposeDeps, MapEnv, factory::FactoryOptions};
use runtime::compose::compose_routed_model;
use runtime::{AgentInvocationContext, AgentModel, ModelPreference, Role, RuntimeError};
use sandbox::credential::FileCredentialStore;

fn configured(mock: &StreamingMockOpenAi) -> Config {
    let mut config = Config {
        providers: BTreeMap::from([(
            "local".into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAiCompatible,
                api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
                base_url: mock.base_url(),
                credential: config::CredentialRefConfig::Env {
                    var: "MISSING_ROUTE_KEY".into(),
                },
                models: vec![config::ModelEntryConfig::enabled("gpt-4o")],
                excluded_models: vec![],
                default_model: "gpt-4o".into(),
            },
        )]),
        ..Config::default()
    };
    config.agents.worker.base.logical_model = Some("coding".into());
    config
        .agents
        .worker
        .categories
        .insert("quick".into(), CategoryBindingConfig::default());
    config
}

fn deps(directory: &tempfile::TempDir) -> ComposeDeps {
    ComposeDeps {
        credential_store: Arc::new(FileCredentialStore::open(directory.path()).unwrap()),
        event_bus: None,
        env: Arc::new(MapEnv::from_iter([("MISSING_ROUTE_KEY", "test-secret")])),
        catalog: model::ModelCatalog::new(),
        factory: FactoryOptions::default(),
    }
}

fn assert_missing_route(error: RuntimeError, category: Option<&str>) {
    let RuntimeError::Model { reason } = error else {
        panic!("expected a model error: {error:?}");
    };
    assert!(
        reason.contains("no route configured for logical model `coding`"),
        "{reason}"
    );
    assert!(reason.contains("role=worker"), "{reason}");
    assert!(
        reason.contains("add a [[routing.routes.coding]] entry"),
        "{reason}"
    );
    if let Some(category) = category {
        assert!(reason.contains(&format!("category={category}")), "{reason}");
    } else {
        assert!(!reason.contains("category="), "{reason}");
    }
    assert!(reason.chars().count() <= 500);
    assert!(!reason.contains("test-secret"));
}

#[tokio::test]
async fn missing_worker_route_composes_but_complete_fails_before_any_http_request() {
    for partial_routes in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mock = StreamingMockOpenAi::spawn(vec![]);
        let mut config = configured(&mock);
        if partial_routes {
            config.routing.routes.insert(
                "orchestrator".into(),
                vec![RouteCandidateConfig {
                    profile: "local".into(),
                    model: None,
                }],
            );
        }
        let model = compose_routed_model(&config, deps(&directory))
            .expect("missing routes must not block startup");
        assert_eq!(
            model.selected_model(Role::Worker, None),
            "unresolved:coding"
        );
        if partial_routes {
            assert_eq!(
                model.selected_model(Role::Orchestrator, None),
                "local/gpt-4o"
            );
        }
        for category in [None, Some("quick")] {
            let invocation = AgentInvocationContext {
                run_id: "missing-route".into(),
                category: category.map(str::to_owned),
                model_preference: None,
            };
            let error = model
                .complete(&invocation, Role::Worker, &[], &[])
                .await
                .expect_err("worker must have an explicit route");
            assert_missing_route(error, category);
            assert!(
                mock.recorded_requests().is_empty(),
                "no catalog or completion HTTP calls"
            );
        }
    }
}

#[tokio::test]
async fn admission_reports_the_missing_logical_route_and_invocation_context() {
    let directory = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn(vec![]);
    let model = compose_routed_model(&configured(&mock), deps(&directory)).unwrap();
    for category in [None, Some("quick")] {
        let error = model
            .admit(
                &AgentInvocationContext {
                    run_id: "missing-admission-route".into(),
                    category: category.map(str::to_owned),
                    model_preference: None,
                },
                Role::Worker,
            )
            .await
            .expect_err("admission must reject the missing route");
        assert!(
            mock.recorded_requests().is_empty(),
            "no catalog or completion HTTP calls"
        );
        assert_missing_route(error, category);
    }
}

#[tokio::test]
async fn unavailable_candidate_reports_existing_route_before_any_http_request() {
    // Given: the route exists, but its only model is absent from the profile and catalog.
    let directory = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn(vec![]);
    let mut config = configured(&mock);
    config.routing.routes.insert(
        "coding".into(),
        vec![RouteCandidateConfig {
            profile: "local".into(),
            model: Some("ghost-model".into()),
        }],
    );
    let model = compose_routed_model(&config, deps(&directory)).unwrap();
    for category in [None, Some("quick")] {
        // When: completion attempts to resolve the configured route.
        let error = model
            .complete(
                &AgentInvocationContext {
                    run_id: "unavailable-route-candidate".into(),
                    category: category.map(str::to_owned),
                    model_preference: None,
                },
                Role::Worker,
                &[],
                &[],
            )
            .await
            .expect_err("the route has no eligible candidate");

        // Then: the error distinguishes an unusable route from a missing route, without HTTP.
        let RuntimeError::Model { reason } = error else {
            panic!("expected a model error: {error:?}");
        };
        assert!(!reason.contains("no route configured"), "{reason}");
        assert!(
            reason.contains("route for logical model `coding`"),
            "{reason}"
        );
        assert!(reason.contains("no available candidate"), "{reason}");
        assert!(reason.contains("role=worker"), "{reason}");
        assert!(
            reason.contains("check the candidate profiles/models under [[routing.routes.coding]]"),
            "{reason}"
        );
        if let Some(category) = category {
            assert!(reason.contains(&format!("category={category}")), "{reason}");
        } else {
            assert!(!reason.contains("category="), "{reason}");
        }
        assert!(reason.chars().count() <= 500);
        assert!(!reason.contains("test-secret"));
        assert!(
            mock.recorded_requests().is_empty(),
            "no catalog or completion HTTP calls"
        );
    }
}

#[tokio::test]
async fn explicit_model_preference_remains_authoritative_without_routes() {
    let directory = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn(vec![ScriptedResponse::text_stream(
        "preferred",
        "gpt-4o",
        ["done"],
    )]);
    let model = compose_routed_model(&configured(&mock), deps(&directory)).unwrap();
    model
        .complete(
            &AgentInvocationContext {
                run_id: "preferred-without-route".into(),
                category: Some("quick".into()),
                model_preference: Some(ModelPreference {
                    profile: "local".into(),
                    model: None,
                }),
            },
            Role::Worker,
            &[],
            &[],
        )
        .await
        .expect("explicit selection bypasses logical routing");
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|request| request.path == "/v1/chat/completions")
            .count(),
        1
    );
}
