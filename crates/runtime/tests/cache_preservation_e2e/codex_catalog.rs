//! Catalog TTL refresh is not a prompt-cache boundary for an established client.
use super::*;
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{ProviderAuth, ProviderClient};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[tokio::test]
async fn codex_catalog_refresh_keeps_the_established_wire_prefix_and_cache_affinity() {
    let ticks = Arc::new(AtomicU64::new(0));
    let clock = ticks.clone();
    let cache = Arc::new(providers::CodexCatalogCache::with_clock(Arc::new(
        move || Duration::from_secs(clock.load(Ordering::Relaxed)),
    )));
    let gate = Arc::new(GatedRead {
        started: Notify::new(),
        release: Notify::new(),
        child_started: Notify::new(),
    });
    let mut harness = harness_with_catalog_executor_and_cache(
        Wire::Codex,
        MODEL,
        Some(json!({
            "use_responses_lite":true, "default_reasoning_level":"low",
            "supported_reasoning_levels":[{"effort":"low"}],
            "supports_reasoning_summary_parameter":true,
            "support_verbosity":true, "default_verbosity":"low", "service_tiers":[]
        })),
        vec![read_response(0), text_response("done")],
        1_000_000,
        gate.clone(),
        Vec::new(),
        None,
        0.5,
        |_| {},
        |_, _| {},
        cache.clone(),
    );
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Read and finish".into(),
        RunConfig::default(),
    );
    gate.started.notified().await; // Admission listed A; first generation completed; next is gated.
    let catalog_gets = || {
        harness
            .mock
            .recorded_requests()
            .iter()
            .filter(|r| r.method == "GET" && r.path.starts_with("/backend-api/codex/models"))
            .count()
    };
    assert_eq!(catalog_gets(), 1, "admission and metadata shared A");
    harness.mock.set_codex_model_capabilities(
        MODEL,
        json!({
            "use_responses_lite":false, "default_reasoning_level":"xhigh",
            "supported_reasoning_levels":[{"effort":"xhigh"}],
            "supports_reasoning_summary_parameter":false,
            "support_verbosity":false, "default_verbosity":"high",
            "service_tiers":[{"id":"priority"}]
        }),
    );
    ticks.store(300, Ordering::Relaxed); // Exact TTL boundary, no wall-clock wait.
    let store = Arc::new(InMemoryTokenStore::new());
    store
        .save(
            &serde_json::from_value::<TokenBundle>(mock_openai::codex_auth::token_bundle(
                "offline-access",
                "offline-refresh",
                "acc",
                mock_openai::codex_auth::FAR_FUTURE_EXP,
            ))
            .unwrap(),
        )
        .unwrap();
    let refreshed = CodexClient::with_config(
        CodexConfig {
            base_url: harness.mock.codex_base_url(),
            ..Default::default()
        },
        store,
    )
    .unwrap()
    .with_catalog_cache(cache.clone());
    assert_eq!(
        refreshed.list_models(&ProviderAuth::new("")).await.unwrap(),
        Some(vec![MODEL.into()])
    );
    let models = providers::list_codex_models_with_cache(
        &cache,
        &harness.mock.codex_base_url(),
        &ProviderAuth::new("offline-access"),
        "acc",
    )
    .await
    .unwrap();
    assert!(!models[0].capabilities.use_responses_lite);
    assert_eq!(
        models[0].capabilities.default_reasoning_level.as_deref(),
        Some("xhigh")
    );
    assert_eq!(
        catalog_gets(),
        2,
        "a different client fetched B with unchanged identity"
    );
    gate.release.notify_one();
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    let requests: Vec<_> = harness
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == Wire::Codex.path())
        .collect();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(request.codex_responses_lite);
        assert_eq!(request.body["reasoning"]["effort"], "low");
        assert_eq!(request.body["reasoning"]["summary"], "auto");
        assert_eq!(request.body["text"]["verbosity"], "low");
        assert!(request.body.get("service_tier").is_none());
        assert!(request.body.get("instructions").is_none());
        assert!(request.body.get("tools").is_none());
    }
    let first = &requests[0].body;
    let next = &requests[1].body;
    assert_eq!(first["input"][0]["type"], "additional_tools");
    assert_eq!(first["input"][1]["role"], "developer");
    // Independent wire checks protect instructions and tool order, not only cached usage.
    assert_eq!(first["input"][0], next["input"][0]);
    assert_eq!(first["input"][1], next["input"][1]);
    assert_eq!(first["prompt_cache_key"], next["prompt_cache_key"]);
    assert_append_only(Wire::Codex.protocol(), first, next).unwrap();
    let mut settings = first.clone();
    let mut next_settings = next.clone();
    settings.as_object_mut().unwrap().remove("input");
    next_settings.as_object_mut().unwrap().remove("input");
    assert_eq!(settings, next_settings);
    let usage = completed_requests(&events, run);
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].1, 0);
    assert!(
        usage[1].1 >= usage[0].0,
        "input-derived cache must reuse the entire sent prefix: {usage:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Compaction(_)))
    );
    assert_eq!(
        catalog_gets(),
        2,
        "the original wire snapshot never refetched B"
    );
}
