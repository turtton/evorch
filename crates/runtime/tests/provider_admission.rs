use std::sync::Arc;

use config::{Config, ProviderProfileConfig, RouteCandidateConfig};
use event_bus::{Event, EventBus, EventKind, MessageEvent};
use mock_openai::{StreamingMockOpenAi, WriteMode};
use routing::{ComposeDeps, MapEnv};
use runtime::{AgentRuntime, ModelPreference, Role, RunConfig, RuntimeError};

#[tokio::test]
async fn advertised_model_runs_after_catalog_admission() {
    // Given: an advertised model with a real streaming completion.
    let server = StreamingMockOpenAi::spawn_with_models(
        vec![mock_openai::ScriptedResponse::text_stream(
            "reply",
            "gpt-4o",
            ["admitted"],
        )],
        WriteMode::default(),
        vec!["gpt-4o".into()],
    );
    let bus = Arc::new(EventBus::new(64));
    let runtime = runtime(&[("primary", server.base_url())], &bus);
    // When: starting and waiting through the public runtime.
    let run = runtime.delegate_background(Role::Worker, "hello".into(), RunConfig::default());
    assert_eq!(
        runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    // Then: discovery preceded completion and the admitted run produced its result.
    assert_eq!(
        runtime.run_result(run).unwrap().as_deref(),
        Some("admitted")
    );
    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/v1/models");
    assert_eq!(requests[1].path, "/v1/chat/completions");
}

fn runtime(urls: &[(&str, String)], bus: &Arc<EventBus>) -> AgentRuntime {
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    for (name, url) in urls {
        config.providers.insert(
            (*name).into(),
            ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAiCompatible,
                api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
                base_url: url.clone(),
                credential: config::CredentialRefConfig::Env { var: "KEY".into() },
                models: vec![config::types::provider::ModelEntryConfig::enabled("gpt-4o")],
                excluded_models: vec![],
                default_model: "gpt-4o".into(),
            },
        );
    }
    config.routing.routes.insert(
        "worker".into(),
        urls.iter()
            .map(|(name, _)| RouteCandidateConfig {
                profile: (*name).into(),
                model: None,
            })
            .collect(),
    );
    let model = runtime::compose::compose_routed_model(
        &config,
        ComposeDeps {
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(
                    directory.path().join("credentials"),
                )
                .unwrap(),
            ),
            event_bus: Some(Arc::clone(bus)),
            env: Arc::new(MapEnv::from_iter([("KEY", "fixture-key")])),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions::default(),
        },
    )
    .unwrap();
    AgentRuntime::new(
        Arc::clone(bus),
        Arc::new(tools::ToolExecutor::with_standard_tools(
            Arc::clone(bus),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )),
        model,
    )
}

async fn assert_rejected(urls: &[(&str, String)], preference: Option<ModelPreference>) {
    let bus = Arc::new(EventBus::new(64));
    let runtime = runtime(urls, &bus);
    let mut events = bus.subscribe();
    // When: admission is requested through the public runtime, not complete().
    let run = runtime.delegate_background(
        Role::Worker,
        "hello".into(),
        RunConfig {
            model_preference: preference,
            ..Default::default()
        },
    );
    let result = runtime.wait(run).await;
    // Then: rejection leaves no registered worker/run or lifecycle-start event.
    assert!(
        runtime.list_agents().is_empty(),
        "failed admission registered a worker"
    );
    assert!(matches!(result, Err(RuntimeError::Model { .. })));
    bus.emit(Event::new(MessageEvent::MessageDelta {
        delta: String::new(),
        run_id: None,
    }));
    loop {
        match events.recv().await.unwrap().kind {
            EventKind::Message(_) => break,
            EventKind::Lifecycle(event) => panic!("admission emitted lifecycle: {event:?}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn unknown_profile_emits_one_correlated_provider_unavailable_without_starting_run() {
    // Given: a healthy configured provider and an explicit absent profile.
    let server =
        StreamingMockOpenAi::spawn_with_models(vec![], WriteMode::default(), vec!["gpt-4o".into()]);
    let bus = Arc::new(EventBus::new(64));
    let runtime = runtime(&[("primary", server.base_url())], &bus);
    let mut events = bus.subscribe();
    // When: admission runs through the public runtime with a reserved run ID.
    let run = runtime.delegate_background(
        Role::Worker,
        "hello".into(),
        RunConfig {
            model_preference: Some(ModelPreference {
                profile: "absent".into(),
                model: Some("gpt-4o".into()),
            }),
            ..Default::default()
        },
    );
    let result = runtime.wait(run).await;
    // Then: one correlated error is emitted without registration or lifecycle/completion.
    assert!(matches!(result, Err(RuntimeError::Model { .. })));
    assert!(runtime.list_agents().is_empty());
    bus.emit(Event::new(MessageEvent::MessageDelta {
        delta: String::new(),
        run_id: None,
    }));
    let mut diagnostics = Vec::new();
    loop {
        match events.recv().await.unwrap().kind {
            EventKind::Message(_) => break,
            EventKind::Diagnostic(event) => diagnostics.push(event),
            EventKind::Lifecycle(event) => panic!("admission emitted lifecycle: {event:?}"),
            _ => {}
        }
    }
    assert_eq!(diagnostics.len(), 1);
    let event = &diagnostics[0];
    assert_eq!(event.severity, event_bus::DiagnosticSeverity::Error);
    assert_eq!(
        event.code,
        event_bus::event::diagnostic_codes::PROVIDER_UNAVAILABLE
    );
    assert_eq!(event.run_id.as_deref(), Some(run.to_string().as_str()));
    // An absent explicit profile fails before contacting unrelated configured providers.
    assert!(server.recorded_requests().is_empty());
}

#[tokio::test]
async fn unavailable_selected_provider_creates_no_worker_or_completion() {
    // Given: an unavailable catalog endpoint.
    let server = StreamingMockOpenAi::spawn(vec![]);
    assert_rejected(
        &[("primary", format!("{}/unavailable", server.base_url()))],
        None,
    )
    .await;
    assert_eq!(server.recorded_requests().len(), 1);
    assert_eq!(server.recorded_requests()[0].method, "GET");
}

#[tokio::test]
async fn unadvertised_selected_model_creates_no_worker_or_completion() {
    // Given: configuration lists gpt-4o but the endpoint advertises another model.
    let server = StreamingMockOpenAi::spawn(vec![]);
    assert_rejected(
        &[("primary", server.base_url())],
        Some(ModelPreference {
            profile: "primary".into(),
            model: Some("gpt-4o".into()),
        }),
    )
    .await;
    assert_eq!(server.recorded_requests().len(), 1);
    assert_eq!(server.recorded_requests()[0].path, "/v1/models");
}

#[tokio::test]
async fn unavailable_fallback_blocks_primary_before_registration() {
    // Given: primary is advertised, but a configured fallback cannot be verified.
    let primary =
        StreamingMockOpenAi::spawn_with_models(vec![], WriteMode::default(), vec!["gpt-4o".into()]);
    let fallback = StreamingMockOpenAi::spawn(vec![]);
    assert_rejected(
        &[
            ("primary", primary.base_url()),
            ("fallback", format!("{}/unavailable", fallback.base_url())),
        ],
        None,
    )
    .await;
    for server in [&primary, &fallback] {
        assert_eq!(server.recorded_requests().len(), 1);
        assert_eq!(server.recorded_requests()[0].method, "GET");
    }
}
