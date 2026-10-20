use std::time::{SystemTime, UNIX_EPOCH};

use config::{ProviderProfileConfig, ProviderTypeConfig};

use super::*;

async fn catalog(api: serde_json::Value) -> Arc<ModelCatalog> {
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let data = serde_json::json!({"fetched_at": now, "api": api});
    std::fs::write(dir.path().join("models-dev.json"), data.to_string()).unwrap();
    Arc::new(ModelCatalog::load_or_refresh(dir.path()).await.unwrap())
}

fn profile(provider_type: ProviderTypeConfig) -> ProviderProfileConfig {
    ProviderProfileConfig {
        provider_type,
        ..ProviderProfileConfig::default()
    }
}

#[tokio::test]
async fn subscriptions_are_priced_as_their_api_models_and_plans_are_never_free() {
    // Given: Claude and Kimi subscriptions, a Kimi plan entry at zero that names its API
    // model, a plan model with no API counterpart, and a metered API profile.
    let catalog = catalog(serde_json::json!({
        "anthropic": {"models": {"claude-sonnet-9": {"id": "claude-sonnet-9",
            "cost": {"input": 3.0, "output": 15.0}}}},
        "kimi-code-plan-global": {"models": {
            "kimi-for-coding": {"id": "kimi-for-coding", "cost": {"input": 0.0, "output": 0.0},
                "canonical_model_id": "moonshotai/kimi-k3"},
            "kimi-orphan": {"id": "kimi-orphan", "cost": {"input": 0.0, "output": 0.0}}}},
        "moonshotai": {"models": {"kimi-k3": {"id": "kimi-k3",
            "cost": {"input": 0.6, "output": 2.5}}}},
    }))
    .await;
    let pricing = UsagePricing::new(
        BTreeMap::from([
            (
                "claude".into(),
                profile(ProviderTypeConfig::AnthropicSubscription),
            ),
            ("kimi".into(), profile(ProviderTypeConfig::KimiSubscription)),
            ("api".into(), profile(ProviderTypeConfig::Anthropic)),
        ]),
        Some(catalog),
    );

    // When: each observed model is priced for comparison.
    let price = |profile: &str, model: &str| {
        pricing
            .comparable_pricing("provider", Some(profile), model)
            .map(|(schedule, basis)| (schedule.standard.base.input, basis))
    };

    // Then: subscriptions use the public API prices, the zero plan is unpriced, and the
    // metered profile keeps its own prices.
    assert_eq!(
        price("claude", "claude-sonnet-9"),
        Some((Some(3.0), PriceBasis::ApiEquivalent))
    );
    assert_eq!(
        price("kimi", "kimi-for-coding"),
        Some((Some(0.6), PriceBasis::ApiEquivalent))
    );
    assert_eq!(price("kimi", "kimi-orphan"), None);
    assert_eq!(
        price("api", "claude-sonnet-9"),
        Some((Some(3.0), PriceBasis::Billed))
    );
}
