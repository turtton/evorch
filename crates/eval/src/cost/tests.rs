use config::{ProviderProfileConfig, ProviderTypeConfig};

use super::*;
use crate::test_support::catalog;

fn request(profile: &str, model: &str, input: u64, output: u64) -> RequestUsage {
    RequestUsage {
        usage_reported: true,
        provider: profile.into(),
        profile: Some(profile.into()),
        model: model.into(),
        run_id: None,
        role: None,
        purpose: Some("agent".into()),
        failed: false,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        service_tier: None,
    }
}

fn profile(provider_type: ProviderTypeConfig) -> ProviderProfileConfig {
    ProviderProfileConfig {
        provider_type,
        ..ProviderProfileConfig::default()
    }
}

/// Prices in USD per million tokens: an API model with a fast mode and a
/// context tier, and the API model a Claude subscription serves.
async fn table() -> PriceTable {
    let catalog = catalog(serde_json::json!({
        "openai": {"models": {"gpt-9": {"id": "gpt-9",
            "cost": {"input": 1.0, "output": 10.0, "cache_read": 0.1,
                "tiers": [{"input": 2.0, "output": 20.0, "tier": {"type": "context", "size": 200000}}]},
            "experimental": {"modes": {"fast": {"cost": {"input": 4.0, "output": 40.0}}}}}}},
        "anthropic": {"models": {"claude-sonnet-9": {"id": "claude-sonnet-9",
            "cost": {"input": 3.0, "output": 15.0}}}},
    }))
    .await;
    PriceTable::new(
        BTreeMap::from([
            ("api".into(), profile(ProviderTypeConfig::OpenAi)),
            (
                "claude".into(),
                profile(ProviderTypeConfig::AnthropicSubscription),
            ),
            (
                "local".into(),
                profile(ProviderTypeConfig::OpenAiCompatible),
            ),
        ]),
        Some(catalog),
    )
}

fn close(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-9
}

#[tokio::test]
async fn requests_are_repriced_per_speed_tier_and_subscription_from_one_table() {
    // Given: standard, cached, large-context and fast API requests, a subscription request,
    // a failed request without tokens, and two requests to a model without any price.
    let table = table().await;
    let mut cached = request("api", "gpt-9", 100_000, 0);
    cached.cache_read_tokens = 50_000;
    let mut fast = request("api", "gpt-9", 100_000, 0);
    fast.service_tier = Some("priority".into());
    let mut failed = request("local", "mystery", 0, 0);
    failed.failed = true;
    let requests = [
        request("api", "gpt-9", 100_000, 100_000),
        cached,
        request("api", "gpt-9", 300_000, 0),
        fast,
        request("claude", "claude-sonnet-9", 1_000_000, 0),
        failed,
        request("local", "mystery", 10, 1),
        request("local", "mystery", 10, 1),
    ];

    // When: the requests are costed together.
    let cost = table.cost(&requests);

    // Then: each request uses its own rate; the subscription is an API equivalent; the
    // unpriced model is listed once with both requests and never counted as free.
    let api = (0.1 + 1.0) + (0.05 + 0.005) + 0.6 + 0.4;
    assert!(close(cost.usd, api + 3.0), "{cost:?}");
    assert!(close(cost.api_equivalent_usd, 3.0), "{cost:?}");
    assert!(cost.is_api_equivalent());
    assert!(!cost.is_complete());
    assert_eq!(
        cost.unpriced,
        vec![Unpriced {
            provider: "local".into(),
            profile: Some("local".into()),
            model: "mystery".into(),
            requests: 2,
        }]
    );
}

#[test]
fn evaluation_requests_are_overhead_and_tokens_split_cached_input() {
    // Given: an agent request with cached input and the evaluation's own requests.
    let mut agent = request("api", "gpt-9", 1_000, 10);
    agent.cache_read_tokens = 600;
    agent.cache_write_tokens = 100;
    let mut simulated = request("api", "gpt-9", 1, 1);
    simulated.purpose = Some("user_simulation".into());
    let mut judging = simulated.clone();
    judging.purpose = Some("judging".into());

    // When / Then: only the agent's request is not overhead, and its input is split.
    assert!(!agent.is_overhead());
    assert!(simulated.is_overhead() && judging.is_overhead());
    let tokens = Tokens::of([&agent]);
    assert_eq!(
        (tokens.input_uncached, tokens.cache_read, tokens.cache_write),
        (300, 600, 100)
    );
    assert_eq!(tokens.input(), 1_000);
    assert!(close(tokens.cache_hit_rate(), 60.0));
}

fn unreported(profile: &str, model: &str) -> RequestUsage {
    RequestUsage {
        usage_reported: false,
        ..request(profile, model, 0, 0)
    }
}

// Given: a priced model with one request that reported no usage and one that reported zero
// When: repriced from the table / Then: the gap makes the cost incomplete and is named, while a
// reported zero (and a failed request) stay a known $0 that does not
#[tokio::test]
async fn unreported_requests_make_the_cost_incomplete_but_reported_zeros_do_not() {
    let table = table().await;

    let gap = table.cost(&[unreported("api", "gpt-9")]);
    assert_eq!(gap.usd, 0.0);
    assert!(!gap.is_complete());
    assert!(gap.unpriced.is_empty(), "the model has a price");
    assert_eq!(gap.unreported.len(), 1);
    assert_eq!(
        (gap.unreported[0].model.as_str(), gap.unreported[0].requests),
        ("gpt-9", 1)
    );

    let mut failed = request("api", "gpt-9", 0, 0);
    failed.failed = true;
    let zero = table.cost(&[request("api", "gpt-9", 0, 0), failed]);
    assert!(zero.is_complete(), "{zero:?}");

    let mut merged = table.cost(&[request("api", "gpt-9", 100_000, 0)]);
    assert!(merged.is_complete() && close(merged.usd, 0.1));
    merged.add(&gap);
    merged.add(&table.cost(&[unreported("api", "gpt-9")]));
    assert!(!merged.is_complete());
    assert!(close(merged.usd, 0.1), "unknown cost adds nothing");
    assert_eq!(merged.unreported[0].requests, 2);
}

// Given: the same unreported completion as a provider event and as a ledger record
// When: both inputs are normalized / Then: both carry the gap, and both reported-zero forms do not
#[test]
fn event_and_record_inputs_both_keep_the_unreported_flag() {
    let completed = |usage_reported| {
        EventKind::Provider(ProviderEvent::RequestCompleted {
            usage_reported,
            request_id: "r".into(),
            provider: "openai".into(),
            profile: Some("api".into()),
            protocol: "fixture".into(),
            model: "gpt-9".into(),
            streaming: true,
            duration_ms: 1,
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
    assert!(
        !RequestUsage::from_event(&completed(false))
            .unwrap()
            .usage_reported
    );
    assert!(
        RequestUsage::from_event(&completed(true))
            .unwrap()
            .usage_reported
    );

    let record = |usage_reported| UsageRequestRecord {
        usage_reported,
        request_id: "r".into(),
        at_ns: 0,
        provider: "openai".into(),
        profile: Some("api".into()),
        model: "gpt-9".into(),
        run_id: None,
        parent_run_id: None,
        role: None,
        purpose: None,
        status: UsageStatus::Ok,
        failure: None,
        finish_reason: None,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        ttft_ms: None,
        duration_ms: 1,
        cost_usd: None,
        service_tier: None,
    };
    assert!(!RequestUsage::from_record(&record(false)).usage_reported);
    assert!(RequestUsage::from_record(&record(true)).usage_reported);
}

// Given: a request serialized by an older version (no usage_reported key) and an unreported one
// When: round-tripped through JSON / Then: the old one reads as reported and the gap survives
#[test]
fn request_usage_serde_keeps_the_gap_and_defaults_old_data_to_reported() {
    let gap = unreported("api", "gpt-9");
    let json = serde_json::to_value(&gap).unwrap();
    assert_eq!(json["usage_reported"], false);
    assert_eq!(serde_json::from_value::<RequestUsage>(json).unwrap(), gap);
    let reported = serde_json::to_value(request("api", "gpt-9", 1, 1)).unwrap();
    assert!(reported.get("usage_reported").is_none());
    assert!(
        serde_json::from_value::<RequestUsage>(reported)
            .unwrap()
            .usage_reported
    );
}
