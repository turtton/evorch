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
                edit_tool: None,
            },
        );
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
        Some(test_support::model_catalog_dir()),
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

async fn assert_rejected(urls: &[(&str, String)], preference: Option<ModelPreference>) -> String {
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
    assert!(matches!(&result, Err(RuntimeError::Model { .. })));
    let reason = result.unwrap_err().to_string();
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
    reason
}

#[tokio::test]
async fn catalog_rate_limit_is_not_model_absence_and_starts_no_worker() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/models"))
        .respond_with(wiremock::ResponseTemplate::new(429))
        .expect(1)
        .mount(&server)
        .await;
    let reason = assert_rejected(&[("primary", server.uri())], None).await;
    assert!(reason.contains("429"), "{reason}");
    assert!(!reason.contains("not advertised"), "{reason}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
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
                reasoning_effort: None,
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
            reasoning_effort: None,
        }),
    )
    .await;
    assert_eq!(server.recorded_requests().len(), 1);
    assert_eq!(server.recorded_requests()[0].path, "/v1/models");
}

#[tokio::test]
async fn unverified_fallback_does_not_block_admission() {
    // Given: primary is advertised, but a configured fallback cannot be verified.
    let primary = StreamingMockOpenAi::spawn_with_models(
        vec![mock_openai::ScriptedResponse::text_stream(
            "reply",
            "gpt-4o",
            ["primary"],
        )],
        WriteMode::default(),
        vec!["gpt-4o".into()],
    );
    let fallback = StreamingMockOpenAi::spawn(vec![]);
    let bus = Arc::new(EventBus::new(64));
    let runtime = runtime(
        &[
            ("primary", primary.base_url()),
            ("fallback", format!("{}/unavailable", fallback.base_url())),
        ],
        &bus,
    );
    let mut events = bus.subscribe();
    // When: running a worker whose selected route is the primary.
    let run = runtime.delegate_background(Role::Worker, "hello".into(), RunConfig::default());
    // Then: the run completes on the primary and the fallback is only reported.
    assert_eq!(
        runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    assert_eq!(runtime.run_result(run).unwrap().as_deref(), Some("primary"));
    let diagnostics = diagnostics_until_sentinel(&bus, &mut events).await;
    assert!(
        diagnostics.iter().any(|event| {
            event.code == "ProviderUnavailable"
                && event.severity == event_bus::DiagnosticSeverity::Warning
                && event.detail.contains("profile=fallback")
        }),
        "{diagnostics:?}"
    );
    assert!(
        !diagnostics
            .iter()
            .any(|event| event.severity == event_bus::DiagnosticSeverity::Error),
        "{diagnostics:?}"
    );
    assert_eq!(fallback.recorded_requests().len(), 1);
    assert_eq!(fallback.recorded_requests()[0].method, "GET");
}

#[tokio::test]
async fn request_fallback_skips_unverified_member() {
    // Given: a failing primary, an unverifiable second member and an advertised third.
    let primary =
        StreamingMockOpenAi::spawn_with_models(vec![], WriteMode::default(), vec!["gpt-4o".into()]);
    let unverified = StreamingMockOpenAi::spawn(vec![]);
    let third = StreamingMockOpenAi::spawn_with_models(
        vec![mock_openai::ScriptedResponse::text_stream(
            "reply",
            "gpt-4o",
            ["third"],
        )],
        WriteMode::default(),
        vec!["gpt-4o".into()],
    );
    let bus = Arc::new(EventBus::new(64));
    let runtime = runtime(
        &[
            ("primary", primary.base_url()),
            (
                "unverified",
                format!("{}/unavailable", unverified.base_url()),
            ),
            ("third", third.base_url()),
        ],
        &bus,
    );
    // When: the primary's completion fails.
    let run = runtime.delegate_background(Role::Worker, "hello".into(), RunConfig::default());
    // Then: fallback passes over the unverified member without sending to it.
    assert_eq!(
        runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    assert_eq!(runtime.run_result(run).unwrap().as_deref(), Some("third"));
    assert!(
        unverified
            .recorded_requests()
            .iter()
            .all(|request| request.method == "GET")
    );
}

async fn diagnostics_until_sentinel(
    bus: &EventBus,
    events: &mut event_bus::bus::EventReceiver,
) -> Vec<event_bus::DiagnosticEvent> {
    bus.emit(Event::new(MessageEvent::MessageDelta {
        delta: "sentinel".into(),
        run_id: None,
    }));
    let mut diagnostics = Vec::new();
    loop {
        match events.recv().await.unwrap().kind {
            EventKind::Message(MessageEvent::MessageDelta {
                delta,
                run_id: None,
            }) if delta == "sentinel" => {
                return diagnostics;
            }
            EventKind::Diagnostic(event) => diagnostics.push(event),
            _ => {}
        }
    }
}
