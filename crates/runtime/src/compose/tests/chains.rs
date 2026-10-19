use std::collections::VecDeque;
use std::time::Duration;

use super::fallback::fixture;
use super::*;

/// Fails with the queued errors first, then succeeds.
struct FlakyClient {
    failures: Mutex<VecDeque<ProviderError>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait]
impl ProviderClient for FlakyClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_use: true,
            reasoning: false,
        }
    }

    async fn send(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        self.failures
            .lock()
            .unwrap()
            .pop_front()
            .map_or_else(|| Ok(response()), Err)
    }

    async fn stream(
        &self,
        _auth: &ProviderAuth,
        _request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        Err(ProviderError::Request("stream unsupported".to_string()))
    }
}

fn flaky_single_member(
    failures: Vec<ProviderError>,
) -> (RoutedModel, Arc<Mutex<Vec<ChatRequest>>>) {
    let (mut model, _) = fixture(vec![None]);
    let requests = Arc::new(Mutex::new(Vec::new()));
    model.providers.get_mut("profile-0").unwrap().client = Arc::new(FlakyClient {
        failures: Mutex::new(failures.into()),
        requests: requests.clone(),
    });
    (model, requests)
}

fn rate_limited(seconds: u64) -> ProviderError {
    ProviderError::RateLimited {
        retry_after: Some(Duration::from_secs(seconds)),
    }
}

#[tokio::test(start_paused = true)]
async fn short_retry_after_waits_once_then_retries_the_exhausted_pool() {
    // Given: the only member is rate limited with a Retry-After under the 30s limit.
    let (model, requests) = flaky_single_member(vec![rate_limited(5)]);
    let started = tokio::time::Instant::now();
    // When
    let result = complete(&model, "run").await;
    // Then: it waited for the Retry-After and the retry bypassed the cooldown.
    assert_eq!(result, Ok(response()));
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(started.elapsed() >= Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn long_retry_after_exhausts_without_waiting() {
    // Given: the Retry-After exceeds the configured wait limit.
    let (model, requests) = flaky_single_member(vec![rate_limited(60)]);
    let started = tokio::time::Instant::now();
    // When
    let result = complete(&model, "run").await;
    // Then: the cooling member is skipped and the cycle ends at once.
    assert!(result.is_err());
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn wait_happens_at_most_once_per_request() {
    // Given: the member is rate limited again after the first wait.
    let (model, requests) = flaky_single_member(vec![rate_limited(5), rate_limited(5)]);
    // When
    let result = complete(&model, "run").await;
    // Then
    assert!(result.is_err());
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn pins_export_and_seed_a_fresh_model_for_the_same_run() {
    // Given: a run that fell back to the second member.
    let (model, _) = fixture(vec![Some(ProviderError::Timeout), None]);
    assert_eq!(complete(&model, "run-1").await, Ok(response()));
    let pins = model.route_pins("run-1");
    assert_eq!(
        pins,
        [crate::RoutePinRecord {
            chain: "worker".into(),
            position: 1,
            profile: "profile-1".into(),
            model: "model-1".into(),
        }]
    );
    // When: a restarted model is seeded with the snapshot's pins.
    let (restarted, requests) = fixture(vec![None, None]);
    restarted.restore_route_pins("run-1", &pins);
    assert_eq!(complete(&restarted, "run-1").await, Ok(response()));
    assert_eq!(complete(&restarted, "run-2").await, Ok(response()));
    // Then: the restored run resumes on its pin; a new run starts from the head.
    assert_eq!(requests[1].lock().unwrap().len(), 1);
    assert_eq!(requests[0].lock().unwrap().len(), 1);
}

#[tokio::test]
async fn replacing_the_model_carries_pins_over() {
    // Given: a switchable model whose run fell back to the second member.
    let (first, _) = fixture(vec![Some(ProviderError::Timeout), None]);
    assert_eq!(complete(&first, "run-1").await, Ok(response()));
    let switchable = SwitchableModel::new(Arc::new(first));
    let (second, requests) = fixture(vec![None, None]);
    // When: a reconfigure swaps in a new model.
    switchable.replace(Arc::new(second));
    let result = switchable
        .complete(
            &AgentInvocationContext {
                run_id: "run-1".into(),
                ..Default::default()
            },
            Role::Worker,
            &[],
            &[],
        )
        .await;
    // Then: the run stays on its pinned member.
    assert_eq!(result, Ok(response()));
    assert!(requests[0].lock().unwrap().is_empty());
    assert_eq!(requests[1].lock().unwrap().len(), 1);
}

// The configured route is deliberately called oracle: only the affinity role
// key may change, never this user-defined identifier.
type Requests = Arc<Mutex<Vec<ChatRequest>>>;
fn advisor_model(errors: Vec<Option<ProviderError>>) -> (RoutedModel, Vec<Requests>) {
    let (mut model, requests) = fixture(errors);
    model.agents.roles.advisor.models = vec![config::ChainEntryConfig {
        route: "oracle".into(),
        reasoning_effort: None,
    }];
    let routing = RoutingConfig {
        routes: BTreeMap::from([(
            "oracle".into(),
            (0..2)
                .map(|index| RouteCandidateConfig {
                    profile: format!("profile-{index}"),
                    model: None,
                    auto: false,
                })
                .collect(),
        )]),
        ..Default::default()
    };
    let mut catalog = model.router.catalog().clone();
    for index in 0..2 {
        let mut entry = catalog.get(&format!("model-{index}")).unwrap().clone();
        entry.capabilities.tool_calling = true;
        catalog.merge_models_dev(vec![entry]);
    }
    model.router = Router::new(
        model
            .providers
            .values()
            .map(|provider| provider.profile.clone())
            .collect(),
        &routing,
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
            .iter()
            .map(|(name, provider)| (name.clone(), Ok(provider.profile.models.clone())))
            .collect(),
    ));
    (model, requests)
}

#[tokio::test]
async fn legacy_oracle_fallback_snapshot_resumes_on_the_same_model() {
    let invocation = AgentInvocationContext {
        run_id: "old-run".into(),
        ..Default::default()
    };
    let (model, _) = advisor_model(vec![Some(ProviderError::Timeout), None]);
    assert_eq!(
        model.complete(&invocation, Role::Advisor, &[], &[]).await,
        Ok(response())
    );
    let mut snapshot = serde_json::to_value(model.route_pins("old-run")).unwrap();
    snapshot[0]["chain"] = serde_json::json!("oracle");
    assert_eq!(snapshot[0]["position"], 1);
    let pins: Vec<crate::RoutePinRecord> = serde_json::from_value(snapshot).unwrap();

    let (restarted, requests) = advisor_model(vec![None, None]);
    restarted.restore_route_pins("old-run", &pins);
    assert_eq!(
        restarted.selected_model_for_run("old-run", Role::Advisor, None),
        "profile-1/model-1"
    );
    assert_eq!(
        restarted
            .complete(&invocation, Role::Advisor, &[], &[])
            .await,
        Ok(response())
    );
    assert!(requests[0].lock().unwrap().is_empty());
    assert_eq!(requests[1].lock().unwrap().len(), 1);
    assert_eq!(restarted.route_pins("old-run")[0].chain, "advisor");
    assert_eq!(restarted.agents.roles.advisor.models[0].route, "oracle");
}

#[test]
fn legacy_chain_key_only_normalizes_the_role_segment() {
    for (old, new) in [
        ("oracle", "advisor"),
        ("oracle/custom", "advisor/custom"),
        ("worker/oracle", "worker/oracle"),
        ("advisor/oracle", "advisor/oracle"),
        ("oracle-custom", "oracle-custom"),
    ] {
        assert_eq!(normalize_chain_key(old), new);
    }
}

#[tokio::test]
async fn persisted_oracle_fallback_chat_uses_its_pin_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("pins.sqlite3"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let make_runtime = |model: Arc<RoutedModel>| {
        let bus = Arc::new(EventBus::new(128));
        crate::AgentRuntime::new(bus.clone(), Arc::new(tools::ToolExecutor::new(bus)), model)
            .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap())
    };
    let (model, _) = advisor_model(vec![Some(ProviderError::Timeout), None]);
    let first = make_runtime(Arc::new(model));
    let run = first.delegate_background(
        Role::Advisor,
        "original".into(),
        crate::RunConfig {
            name: Some("chat:Oracle:thread".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        first.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    let database = storage::Database::open(&config).unwrap();
    let mut record = database.run_context(&run.to_string()).unwrap().unwrap();
    let mut descriptor: crate::restore::RunRestoreDescriptor =
        serde_json::from_str(&record.config_json).unwrap();
    assert_eq!(descriptor.route_pins[0].position, 1);
    assert_eq!(descriptor.route_pins[0].model, "model-1");
    descriptor.role = "Oracle".into();
    descriptor.route_pins[0].chain = "oracle".into();
    record.role = "Oracle".into();
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    drop(first);

    let (model, requests) = advisor_model(vec![None, None]);
    let model = Arc::new(model);
    let restarted = make_runtime(model.clone());
    let continued = restarted
        .delegate_chat(
            "thread",
            Role::Advisor,
            "followup".into(),
            crate::RunConfig::default(),
        )
        .unwrap();
    assert_eq!(
        restarted.wait(continued).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    assert!(requests[0].lock().unwrap().is_empty());
    assert_eq!(requests[1].lock().unwrap().len(), 1);
    let pins = model.route_pins(&continued.to_string());
    assert_eq!(pins[0].chain, "advisor");
    assert_eq!(pins[0].model, "model-1");
    assert_eq!(model.agents.roles.advisor.models[0].route, "oracle");
}
