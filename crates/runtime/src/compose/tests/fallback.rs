use super::*;
use event_bus::{EventKind, ProviderEvent};

mod streaming;

type Requests = Arc<Mutex<Vec<ChatRequest>>>;

fn fixture(errors: Vec<Option<ProviderError>>) -> (RoutedModel, Vec<Requests>) {
    let (mut model, _) = routed_model(Ok(response()), "model-a", None);
    let mut catalog = ModelCatalog::builtin();
    let mut profiles = Vec::new();
    let mut candidates = Vec::new();
    let mut recordings = Vec::new();
    model.providers.clear();
    for (index, error) in errors.into_iter().enumerate() {
        let name = format!("profile-{index}");
        let model_id = format!("model-{index}");
        let mut profile = profile(&model_id, &[&model_id]);
        profile.name = name.clone();
        catalog.merge_discovered(profile.models.clone());
        profiles.push(profile.clone());
        candidates.push(RouteCandidateConfig {
            profile: name.clone(),
            model: None,
        });
        let requests = Arc::new(Mutex::new(Vec::new()));
        model.providers.insert(
            name,
            ComposedProvider {
                profile,
                client: Arc::new(StubClient {
                    result: error.map_or_else(|| Ok(response()), Err),
                    requests: requests.clone(),
                }),
                auth: ProviderAuth::new(format!("secret-{index}")),
            },
        );
        recordings.push(requests);
    }
    model.router = Router::new(
        profiles,
        &RoutingConfig {
            routes: BTreeMap::from([("worker".into(), candidates)]),
        },
        catalog,
    )
    .unwrap();
    model.tool_router = model
        .router
        .clone()
        .requiring_capability(Capability::ToolCalling);
    model.verification = tokio::sync::OnceCell::new_with(Some(
        model
            .providers
            .keys()
            .map(|name| (name.clone(), Ok(Vec::new())))
            .collect(),
    ));
    (model, recordings)
}

#[tokio::test]
async fn fallback_repins_affinity_and_preserves_request_when_primary_fails() {
    // Given
    let (model, requests) = fixture(vec![Some(ProviderError::Timeout), None]);
    // When
    assert_eq!(complete(&model, "session").await, Ok(response()));
    // Then
    assert_eq!(
        model
            .resolve("session", &LogicalModelId::from("worker"), false)
            .unwrap()
            .profile,
        "profile-1"
    );
    let mut first = requests[0].lock().unwrap()[0].clone();
    first.model = "model-1".into();
    assert_eq!(requests[1].lock().unwrap()[0], first);
    assert_eq!(complete(&model, "session").await, Ok(response()));
    assert_eq!(requests[0].lock().unwrap().len(), 1);
    assert_eq!(requests[1].lock().unwrap().len(), 2);
}

#[tokio::test]
async fn fallback_emits_from_to_on_streaming_and_nonstreaming_paths() {
    for streaming in [false, true] {
        // Given: a router bus and a runtime bus must not duplicate the event.
        let (mut model, _) = fixture(vec![
            Some(ProviderError::RateLimited { retry_after: None }),
            None,
        ]);
        let bus = Arc::new(EventBus::new(32));
        let mut rx = bus.subscribe();
        model.router = model.router.with_event_bus(Some(bus.clone()));
        model.event_bus = Some(bus.clone());
        let invocation = AgentInvocationContext {
            run_id: "session".into(),
            category: None,
            model_preference: None,
        };
        // When
        let result = model
            .complete_request(
                &invocation,
                Role::Worker,
                &[],
                &[],
                streaming.then_some(bus.as_ref()),
            )
            .await;
        // Then
        assert_eq!(result, Ok(response()));
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("fallback event")
            .unwrap();
        assert!(
            matches!(event.kind, EventKind::Provider(ProviderEvent::FallbackTriggered {
            from_provider, from_model, to_provider, to_model, session_id, ..
        }) if from_provider == "profile-0" && from_model.as_deref() == Some("model-0")
            && to_provider == "profile-1" && to_model == "model-1" && session_id == "session")
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn fallback_classifies_direct_and_exhausted_failures() {
    for status in [400, 401, 402, 403, 404, 408, 429, 500, 503, 599] {
        for wrapped in [false, true] {
            // Given
            let error = ProviderError::Http {
                status,
                body: "failure".into(),
            };
            let error = if wrapped {
                ProviderError::RetriesExhausted {
                    attempts: 3,
                    last: Box::new(error),
                }
            } else {
                error
            };
            let (model, requests) = fixture(vec![Some(error), None]);
            // When
            let result = complete(&model, "session").await;
            // Then
            // Auth failures and exhausted quota leave this candidate unusable, so try another.
            let eligible = matches!(status, 401 | 402 | 403 | 408 | 429 | 500..=599);
            assert_eq!(
                result.is_ok(),
                eligible,
                "status={status}, wrapped={wrapped}"
            );
            assert_eq!(requests[1].lock().unwrap().len(), usize::from(eligible));
        }
    }
}

#[tokio::test]
async fn exhaustion_lists_every_attempt_with_redacted_errors() {
    // Given
    let (model, requests) = fixture(vec![
        Some(ProviderError::Timeout),
        Some(ProviderError::Transport {
            message: "bad secret-1".into(),
        }),
        Some(ProviderError::Http {
            status: 503,
            body: "unavailable".into(),
        }),
    ]);
    // When
    let error = complete(&model, "session").await.unwrap_err().to_string();
    // Then
    for (index, requests) in requests.iter().enumerate() {
        assert!(
            error.contains(&format!("profile=profile-{index} model=model-{index}:")),
            "{error}"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
    assert!(error.contains("timed out") && error.contains("bad ***") && error.contains("503"));
    assert!(!error.contains("secret-1"));
}

#[tokio::test]
async fn explicit_preference_never_falls_back_on_timeout() {
    // Given
    let (model, requests) = fixture(vec![Some(ProviderError::Timeout), None]);
    let invocation = AgentInvocationContext {
        run_id: "session".into(),
        category: None,
        model_preference: Some(crate::ModelPreference {
            profile: "profile-0".into(),
            model: None,
        }),
    };
    // When
    let result = model.complete(&invocation, Role::Worker, &[], &[]).await;
    // Then
    assert!(result.is_err());
    assert!(requests[1].lock().unwrap().is_empty());
}

#[tokio::test]
async fn fallback_exhausts_without_attempting_other_logical_routes() {
    // Given
    let (mut model, requests) = fixture(vec![
        Some(ProviderError::Timeout),
        Some(ProviderError::Timeout),
        Some(ProviderError::Timeout),
    ]);
    model.router = Router::new(
        model
            .providers
            .values()
            .map(|p| p.profile.clone())
            .collect(),
        &RoutingConfig {
            routes: BTreeMap::from([
                (
                    "worker".into(),
                    vec![RouteCandidateConfig {
                        profile: "profile-0".into(),
                        model: None,
                    }],
                ),
                (
                    "other".into(),
                    vec![
                        RouteCandidateConfig {
                            profile: "profile-1".into(),
                            model: None,
                        },
                        RouteCandidateConfig {
                            profile: "profile-2".into(),
                            model: None,
                        },
                    ],
                ),
            ]),
        },
        model.router.catalog().clone(),
    )
    .unwrap();
    // When
    let error = complete(&model, "session").await.unwrap_err().to_string();
    // Then
    assert!(
        error.contains("profile=profile-0 model=model-0:"),
        "{error}"
    );
    assert_eq!(requests[0].lock().unwrap().len(), 1);
    for index in [1, 2] {
        assert!(requests[index].lock().unwrap().is_empty());
        assert!(!error.contains(&format!("profile-{index}")), "{error}");
    }
}
