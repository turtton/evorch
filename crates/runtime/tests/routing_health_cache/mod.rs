//! Another run's auth failure must not move a waiting run or invalidate its cache.
use super::*;

#[tokio::test]
async fn another_runs_auth_failure_preserves_primary_pin_and_cached_prefix() {
    // The extra response lets the OLD implementation finish A: regression detection
    // is a wire assertion, never mock script exhaustion.
    let fallback = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            text_response("B fallback"),
            text_response("unexpected A fallback"),
        ],
        vec![MODEL.into()],
    );
    let mut harness = harness_with_wire(
        Wire::Responses,
        vec![
            text_response("A first reply"),
            text_response("A second reply"),
        ],
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.5,
        |config| {
            let mut profile = config.providers["local"].clone();
            profile.base_url = fallback.base_url();
            config.providers.insert("fallback".into(), profile);
            config.routing.routes.insert(
                "backup".into(),
                vec![config::RouteCandidateConfig {
                    profile: "fallback".into(),
                    model: None,
                    auto: false,
                }],
            );
            config.agents.worker.base.models = vec![
                config::ChainEntryConfig {
                    route: "worker".into(),
                    reasoning_effort: None,
                },
                config::ChainEntryConfig {
                    route: "backup".into(),
                    reasoning_effort: None,
                },
            ];
        },
    );
    let config = storage::StorageConfig {
        db_path: harness._directory.path().join("routing-health.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    harness.runtime = harness
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let chat = || RunConfig {
        interactive: true,
        keep_alive: true,
        ..Default::default()
    };
    let a = harness
        .runtime
        .delegate_chat("health-A", Role::Worker, "A first turn".into(), chat())
        .unwrap();
    let mut events = through_phase(&mut harness.receiver, a, AgentRunPhase::Waiting).await;
    harness.mock.reject_responses_unauthorized(0, 1);
    let b = harness
        .runtime
        .delegate_chat("health-B", Role::Worker, "B first turn".into(), chat())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, b, AgentRunPhase::Waiting).await);
    assert!(events.iter().any(|event| matches!(&event.kind,
        EventKind::Provider(ProviderEvent::CooldownStarted { run_id: Some(run), failure: event_bus::ProviderFailureKind::Auth, .. }) if run == &b.to_string())));
    harness
        .runtime
        .send_message(a, "A second turn".into())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, a, AgentRunPhase::Waiting).await);
    for run in [a, b] {
        harness.runtime.cancel(run).unwrap();
        harness.runtime.wait(run).await.unwrap();
    }
    let requests = |mock: &StreamingMockOpenAi| {
        mock.recorded_requests()
            .into_iter()
            .filter(|r| r.path == Wire::Responses.path())
            .map(|r| r.body)
            .collect::<Vec<_>>()
    };
    let primary = requests(&harness.mock);
    let backup = requests(&fallback);
    assert_eq!(primary.len(), 3, "A must return to primary after B's 401");
    assert_eq!(backup.len(), 1, "only B uses fallback");
    assert_eq!(primary[0]["prompt_cache_key"], a.to_string());
    assert_eq!(primary[1]["prompt_cache_key"], b.to_string());
    assert_eq!(primary[2]["prompt_cache_key"], a.to_string());
    assert_eq!(backup[0]["prompt_cache_key"], b.to_string());
    assert_append_only(Wire::Responses.protocol(), &primary[0], &primary[2]).unwrap();
    for key in [
        "instructions",
        "model",
        "tools",
        "temperature",
        "top_p",
        "max_output_tokens",
        "reasoning",
        "prompt_cache_key",
    ] {
        assert_eq!(primary[0].get(key), primary[2].get(key), "{key}");
    }
    assert!(!events.iter().any(|event| matches!(&event.kind,
        EventKind::Provider(ProviderEvent::FallbackTriggered { session_id, .. }) if session_id == &a.to_string())));
    assert!(!events.iter().any(|event| matches!(&event.kind,
        EventKind::Compaction(CompactionEvent::Compacted { run_id, reason: event_bus::CompactionReason::ModelSwitch, .. }) if run_id == &a.to_string())));
    let usage: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id: Some(run),
                input_tokens,
                cache_read_tokens,
                ..
            }) if run == &a.to_string() => Some((*input_tokens, *cache_read_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 2);
    assert!(usage[0].0 > 0 && usage[0].1 == 0, "{usage:?}");
    assert!(
        usage[1].1 >= usage[0].0,
        "input-derived cache must reuse A's prefix: {usage:?}"
    );
    assert!(!events.iter().any(|event| matches!(&event.kind,
        EventKind::Diagnostic(diagnostic) if diagnostic.code == "CacheRegression")));
}
