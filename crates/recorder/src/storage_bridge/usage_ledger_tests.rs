use super::*;
use crate::pricing::UsagePricing;
use event_bus::{LifecycleEvent, ProviderEvent, ProviderFailureKind, RequestPurpose};
use storage::usage::UsageStatus;
use storage::{Database, Storage, StorageConfig};

fn fixture() -> (tempfile::TempDir, Storage, Database) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    (dir, storage, db)
}

/// $1/M uncached input, $10/M output, $0.1/M cache read and $2/M cache write.
fn priced() -> crate::pricing::SharedUsagePricing {
    let mut entry = config::ModelEntryConfig::enabled("model");
    entry.input_price = Some(1.0);
    entry.output_price = Some(10.0);
    entry.cache_read_price = Some(0.1);
    entry.cache_write_price = Some(2.0);
    let profile = config::ProviderProfileConfig {
        models: vec![entry],
        ..config::ProviderProfileConfig::default()
    };
    std::sync::Arc::new(std::sync::RwLock::new(UsagePricing::new(
        [("main".to_owned(), profile)].into(),
        None,
    )))
}

fn completed(request_id: &str, model: &str) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        usage_reported: true,
        request_id: request_id.into(),
        provider: "openai".into(),
        profile: Some("main".into()),
        protocol: "openai-chat-completions".into(),
        model: model.into(),
        streaming: true,
        duration_ms: 1_200,
        input_tokens: 1_000_000,
        output_tokens: 100_000,
        cache_read_tokens: 500_000,
        cache_write_tokens: 100_000,
        finish_reason: "stop".into(),
        service_tier: None,
        run_id: Some("run-2".into()),
        reasoning_tokens: Some(40_000),
        purpose: Some(RequestPurpose::Compaction),
    })
}

#[test]
fn completed_attempt_is_recorded_with_run_context_and_recorded_price() {
    // Given: a subagent run whose request streamed a first token.
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session").with_usage_ledger(priced());
    for event in [
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-2".into(),
            parent_run_id: Some("run-1".into()),
            agent_name: "explorer".into(),
            role: "explorer".into(),
        }),
        Event::new(ProviderEvent::FirstTokenObserved {
            request_id: "req-1".into(),
            provider: "openai".into(),
            profile: Some("main".into()),
            protocol: "openai-chat-completions".into(),
            model: "model".into(),
            ttft_ms: 320,
            run_id: Some("run-2".into()),
        }),
    ] {
        bridge.handle_event(&event).unwrap();
    }

    // When: the attempt completes.
    bridge.handle_event(&completed("req-1", "model")).unwrap();

    // Then: one ledger row carries the run tree, latency, purpose and cost.
    let rows = db.usage_requests_between(0, i64::MAX).unwrap();
    assert_eq!(rows.len(), 1);
    let record = &rows[0].record;
    assert_eq!(record.request_id, "req-1");
    assert_eq!(record.role.as_deref(), Some("explorer"));
    assert_eq!(record.parent_run_id.as_deref(), Some("run-1"));
    assert_eq!(record.purpose.as_deref(), Some("compaction"));
    assert_eq!(record.ttft_ms, Some(320));
    assert_eq!(record.reasoning_tokens, Some(40_000));
    assert_eq!(record.status, UsageStatus::Ok);
    // 0.4M uncached input + 0.1M output + 0.5M read + 0.1M write.
    let cost = record.cost_usd.unwrap();
    assert!((cost - (0.4 + 1.0 + 0.05 + 0.2)).abs() < 1e-9, "{cost}");
}

#[test]
fn unpriced_and_failed_attempts_are_recorded_without_cost() {
    // Given: a model with no known price and a failing attempt.
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session").with_usage_ledger(priced());

    // When: both attempts reach the bridge.
    bridge.handle_event(&completed("req-1", "unknown")).unwrap();
    bridge
        .handle_event(&Event::new(ProviderEvent::RequestFailed {
            detail: None,
            request_id: "req-2".into(),
            provider: "openai".into(),
            profile: Some("main".into()),
            protocol: "openai-chat-completions".into(),
            model: "model".into(),
            streaming: true,
            duration_ms: 50,
            failure: ProviderFailureKind::RateLimited,
            run_id: None,
            purpose: None,
        }))
        .unwrap();

    // Then: neither row invents a price, and the failure keeps its kind.
    let rows = db.usage_requests_between(0, i64::MAX).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].record.cost_usd, None);
    assert_eq!(rows[1].record.status, UsageStatus::Failed);
    assert_eq!(rows[1].record.failure.as_deref(), Some("RateLimited"));
    assert_eq!(rows[1].record.cost_usd, None);
}

#[test]
fn unreported_usage_is_not_booked_as_a_known_zero_cost() {
    // Given: a priced model, one request that reported zero tokens and one whose
    // provider reported no usage at all (placeholder zeros).
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session").with_usage_ledger(priced());
    let zero = |request_id: &str, usage_reported| {
        Event::new(ProviderEvent::RequestCompleted {
            usage_reported,
            request_id: request_id.into(),
            provider: "openai".into(),
            profile: Some("main".into()),
            protocol: "openai-chat-completions".into(),
            model: "model".into(),
            streaming: true,
            duration_ms: 10,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: "stop".into(),
            service_tier: None,
            run_id: None,
            reasoning_tokens: None,
            purpose: None,
        })
    };

    // When: both complete.
    bridge.handle_event(&zero("reported", true)).unwrap();
    bridge.handle_event(&zero("unreported", false)).unwrap();

    // Then: the reported zero is a known $0, the unreported one has no cost at all.
    let rows = db.usage_requests_between(0, i64::MAX).unwrap();
    let cost = |id: &str| {
        rows.iter()
            .find(|row| row.record.request_id == id)
            .unwrap()
            .record
            .cost_usd
    };
    assert_eq!(cost("reported"), Some(0.0));
    assert_eq!(cost("unreported"), None);
}

#[test]
fn disabled_metrics_record_no_ledger_rows() {
    // Given: metrics collection turned off.
    let (_dir, storage, db) = fixture();
    let mut bridge = StorageBridge::new(storage.handle(), "session")
        .with_metrics_enabled(false)
        .with_usage_ledger(priced());

    // When: an attempt completes.
    bridge.handle_event(&completed("req-1", "model")).unwrap();

    // Then: the provider event persists, but no ledger row is written.
    assert_eq!(db.events_all_ordered().unwrap().len(), 1);
    assert!(db.usage_requests_between(0, i64::MAX).unwrap().is_empty());
}

#[test]
fn fast_and_large_requests_are_recorded_at_their_configured_rates() {
    // Given: manual prices of $1/M input with a $2/M tier above 500K, and $4/M fast input.
    let (_dir, storage, db) = fixture();
    let mut entry = config::ModelEntryConfig::enabled("model");
    entry.input_price = Some(1.0);
    entry.price_tiers = Some(vec![config::types::provider::PriceTier {
        above_input_tokens: 500_000,
        input: Some(2.0),
        output: None,
        cache_read: None,
        cache_write: None,
    }]);
    entry.fast_price = Some(config::types::provider::ModelPricing {
        input: Some(4.0),
        ..Default::default()
    });
    let profile = config::ProviderProfileConfig {
        models: vec![entry],
        ..config::ProviderProfileConfig::default()
    };
    let pricing = std::sync::Arc::new(std::sync::RwLock::new(UsagePricing::new(
        [("main".to_owned(), profile)].into(),
        None,
    )));
    let mut bridge = StorageBridge::new(storage.handle(), "session").with_usage_ledger(pricing);
    let request = |request_id: &str, input_tokens: u64, service_tier| {
        Event::new(ProviderEvent::RequestCompleted {
            usage_reported: true,
            request_id: request_id.into(),
            provider: "openai".into(),
            profile: Some("main".into()),
            protocol: "openai-chat-completions".into(),
            model: "model".into(),
            streaming: true,
            duration_ms: 10,
            input_tokens,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: "stop".into(),
            service_tier,
            run_id: None,
            reasoning_tokens: None,
            purpose: None,
        })
    };

    // When: a small, a large and a fast request complete.
    for event in [
        request("small", 100_000, None),
        request("large", 1_000_000, None),
        request("fast", 100_000, Some(event_bus::ServiceTier::Priority)),
    ] {
        bridge.handle_event(&event).unwrap();
    }

    // Then: each cost uses its own rate and the fast request keeps its tier label.
    let rows = db.usage_requests_between(0, i64::MAX).unwrap();
    let recorded: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row.record.request_id.as_str(),
                row.record.cost_usd.map(|cost| (cost * 1e6).round() / 1e6),
                row.record.service_tier.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        recorded,
        [
            ("small", Some(0.1), None),
            ("large", Some(2.0), None),
            ("fast", Some(0.4), Some("priority")),
        ]
    );
}
