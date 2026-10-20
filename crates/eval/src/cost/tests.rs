use config::{ProviderProfileConfig, ProviderTypeConfig};

use super::*;
use crate::test_support::catalog;

fn request(profile: &str, model: &str, input: u64, output: u64) -> RequestUsage {
    RequestUsage {
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
