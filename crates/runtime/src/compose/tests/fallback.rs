use super::*;
use event_bus::{EventKind, ProviderEvent};

mod streaming;

type Requests = Arc<Mutex<Vec<ChatRequest>>>;

pub(super) fn fixture(errors: Vec<Option<ProviderError>>) -> (RoutedModel, Vec<Requests>) {
    fixture_with_efforts(errors, &[])
}

pub(super) fn fixture_with_efforts(
    errors: Vec<Option<ProviderError>>,
    efforts: &[Option<&str>],
) -> (RoutedModel, Vec<Requests>) {
    fixture_with(errors, efforts, &[])
}

/// Like [`fixture_with_efforts`], also giving `profile-{i}`'s model the catalog window
/// `windows[i]` (with tool calling) when present.
pub(super) fn fixture_with(
    errors: Vec<Option<ProviderError>>,
    efforts: &[Option<&str>],
    windows: &[u64],
) -> (RoutedModel, Vec<Requests>) {
    let (mut model, _) = routed_model(Ok(response()), "model-a", None);
    let mut catalog = ModelCatalog::new();
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
        if let Some(window) = windows.get(index) {
            let mut entry = catalog.get(&model_id).unwrap().clone();
            entry.context_window = *window;
            entry.capabilities.tool_calling = true;
            catalog.merge_models_dev(vec![entry]);
        }
        profiles.push(profile.clone());
        candidates.push(RouteCandidateConfig {
            profile: name.clone(),
            model: None,
            auto: false,
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
    // Without efforts every profile shares the `worker` pool; with them each profile is
    // its own chain entry carrying that entry's effort.
    let routes = if efforts.is_empty() {
        BTreeMap::from([("worker".into(), candidates)])
    } else {
        model.agents.worker.base.models = (0..candidates.len())
            .map(|index| config::ChainEntryConfig {
                route: format!("route-{index}"),
                reasoning_effort: efforts.get(index).copied().flatten().map(str::to_owned),
            })
            .collect();
        candidates
            .into_iter()
            .enumerate()
            .map(|(index, candidate)| (format!("route-{index}"), vec![candidate]))
            .collect()
    };
    model.router = Router::new(
        profiles,
        &RoutingConfig {
            cooldown: Default::default(),
            fallback: Default::default(),
            routes,
        },
        catalog,
    )
    .unwrap()
    .with_health(Arc::clone(&model.health));
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
async fn fallback_uses_each_candidates_reasoning_effort() {
    // Given: the failing primary and its fallback configure different efforts.
    let (model, requests) = fixture_with_efforts(
        vec![Some(ProviderError::Timeout), None],
        &[Some("xhigh"), Some("low")],
    );
    // When
    assert_eq!(complete(&model, "session").await, Ok(response()));
    // Then: the fallback request uses its own chain entry's effort.
    assert_eq!(
        requests[0].lock().unwrap()[0].reasoning_effort.as_deref(),
        Some("xhigh")
    );
    assert_eq!(
        requests[1].lock().unwrap()[0].reasoning_effort.as_deref(),
        Some("low")
    );
}

#[tokio::test]
async fn quota_failure_keeps_new_runs_off_the_profile() {
    // Given: the primary reports a payment/quota failure and its fallback succeeds.
    let (mut model, requests) = fixture(vec![
        Some(ProviderError::Http {
            status: 402,
            body: "quota exceeded".into(),
        }),
        None,
    ]);
    let bus = Arc::new(EventBus::new(32));
    let mut rx = bus.subscribe();
    model.event_bus = Some(bus);
    // When: one run falls back, then an unrelated run starts.
    assert_eq!(complete(&model, "run-1").await, Ok(response()));
    assert_eq!(complete(&model, "run-2").await, Ok(response()));
    // Then: the second run never contacts the cooling profile.
    assert_eq!(requests[0].lock().unwrap().len(), 1);
    assert_eq!(requests[1].lock().unwrap().len(), 2);
    let mut cooldowns = Vec::new();
    // Events are emitted before `complete` returns; the timeout only ends the drain.
    while let Ok(Ok(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(10), rx.recv()).await
    {
        if let EventKind::Provider(ProviderEvent::CooldownStarted {
            profile,
            model,
            failure,
            cooldown_ms,
            run_id,
        }) = event.kind
        {
            cooldowns.push((profile, model, failure, cooldown_ms, run_id));
        }
    }
    assert_eq!(
        cooldowns,
        [(
            "profile-0".to_owned(),
            None,
            event_bus::ProviderFailureKind::Quota,
            Some(1_800_000),
            Some("run-1".to_owned()),
        )]
    );
}

#[tokio::test]
async fn fallback_repins_affinity_and_preserves_request_when_primary_fails() {
    // Given
    let (model, requests) = fixture(vec![Some(ProviderError::Timeout), None]);
    // When
    assert_eq!(complete(&model, "session").await, Ok(response()));
    // Then
    let (_, choice) = model
        .resolve_binding("session", Role::Worker, None, false)
        .unwrap();
    assert_eq!(
        choice.chain.member(choice.position).target.profile,
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
        // Given: the runtime bus receives each fallback exactly once.
        let (mut model, requests) = fixture(vec![
            Some(ProviderError::RateLimited { retry_after: None }),
            None,
        ]);
        let bus = Arc::new(EventBus::new(32));
        let mut rx = bus.subscribe();
        model.event_bus = Some(bus.clone());
        let invocation = AgentInvocationContext {
            turn: Some(providers::RequestTurnContext {
                origin: Some("human".into()),
                prompt_index: Some(7),
                turn_index: Some(7),
                parent_subagent_id: Some("parent-run".into()),
                ..providers::RequestTurnContext::unattributed(true)
            }),
            run_id: "session".into(),
            category: None,
            model_preference: None,
            purpose: Default::default(),
        };
        // When
        let result = model
            .complete_request(
                &invocation,
                Role::Worker,
                &[],
                &[],
                streaming.then_some(bus.as_ref()),
                None,
                None,
            )
            .await;
        // Then
        assert_eq!(result, Ok(response()));
        for provider_requests in requests {
            for request in provider_requests.lock().unwrap().iter() {
                assert_eq!(request.observation.as_ref().unwrap().turn, invocation.turn);
            }
        }
        let cooldown = rx.recv().await.unwrap();
        assert!(
            matches!(cooldown.kind, EventKind::Provider(ProviderEvent::CooldownStarted {
            profile, model, ..
        }) if profile == "profile-0" && model.as_deref() == Some("model-0"))
        );
        let event = rx.recv().await.unwrap();
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
            let eligible = matches!(status, 400 | 401 | 402 | 403 | 408 | 429 | 500..=599);
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
    // Then: each member is tried once, and the origin's transient timeout is retried last.
    for (index, requests) in requests.iter().enumerate() {
        assert!(
            error.contains(&format!("profile=profile-{index} model=model-{index}:")),
            "{error}"
        );
        assert_eq!(
            requests.lock().unwrap().len(),
            if index == 0 { 2 } else { 1 }
        );
    }
    assert!(error.contains("timed out") && error.contains("bad ***") && error.contains("503"));
    assert!(!error.contains("secret-1"));
}

#[tokio::test]
async fn explicit_preference_never_falls_back_on_timeout() {
    // Given
    let (model, requests) = fixture(vec![Some(ProviderError::Timeout), None]);
    let invocation = AgentInvocationContext {
        turn: None,
        run_id: "session".into(),
        category: None,
        model_preference: Some(crate::ModelPreference {
            profile: "profile-0".into(),
            model: None,
            reasoning_effort: None,
        }),
        purpose: Default::default(),
    };
    // When
    let result = model.complete(&invocation, Role::Worker, &[], &[]).await;
    // Then
    assert!(result.is_err());
    assert!(requests[1].lock().unwrap().is_empty());
}

#[tokio::test]
async fn explicit_preference_never_falls_back_on_http_400() {
    let (model, requests) = fixture(vec![
        Some(ProviderError::Http {
            status: 400,
            body: "model not available for client".into(),
        }),
        None,
    ]);
    let invocation = AgentInvocationContext {
        turn: None,
        run_id: "session".into(),
        category: None,
        model_preference: Some(crate::ModelPreference {
            profile: "profile-0".into(),
            model: None,
            reasoning_effort: None,
        }),
        purpose: Default::default(),
    };
    let result = model.complete(&invocation, Role::Worker, &[], &[]).await;
    assert!(result.is_err());
    assert_eq!(requests[0].lock().unwrap().len(), 1);
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
            cooldown: Default::default(),
            fallback: Default::default(),
            routes: BTreeMap::from([
                (
                    "worker".into(),
                    vec![RouteCandidateConfig {
                        profile: "profile-0".into(),
                        model: None,
                        auto: false,
                    }],
                ),
                (
                    "other".into(),
                    vec![
                        RouteCandidateConfig {
                            profile: "profile-1".into(),
                            model: None,
                            auto: false,
                        },
                        RouteCandidateConfig {
                            profile: "profile-2".into(),
                            model: None,
                            auto: false,
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
    // The transient origin is retried once at the end of the cycle; other routes never run.
    assert_eq!(requests[0].lock().unwrap().len(), 2);
    for index in [1, 2] {
        assert!(requests[index].lock().unwrap().is_empty());
        assert!(!error.contains(&format!("profile-{index}")), "{error}");
    }
}
