use std::time::{SystemTime, UNIX_EPOCH};

use config::{ModelEntryConfig, ProviderProfileConfig, ProviderTypeConfig};

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

#[tokio::test]
async fn cursor_models_are_priced_from_docs_or_their_api_equivalent() {
    // Given: a Cursor profile with a TOML price override, and a catalog that lists the
    // public API models but no `cursor` provider.
    let catalog = catalog(serde_json::json!({
        "anthropic": {"models": {"claude-opus-5-5": {"id": "claude-opus-5-5",
            "limit": {"context": 1000000, "output": 64000},
            "cost": {"input": 4.0, "output": 20.0}}}},
        "openai": {"models": {"gpt-5": {"id": "gpt-5",
            "cost": {"input": 1.25, "output": 10.0}}}},
        "google": {"models": {"gemini-3.1-pro": {"id": "gemini-3.1-pro",
            "cost": {"input": 2.0, "output": 12.0}}}},
    }))
    .await;
    let mut cursor = profile(ProviderTypeConfig::Cursor);
    cursor.models = vec![ModelEntryConfig {
        input_price: Some(9.0),
        ..ModelEntryConfig::enabled("gpt-5-high")
    }];
    let pricing = UsagePricing::new(BTreeMap::from([("cursor".into(), cursor)]), Some(catalog));
    let price = |model: &str| {
        pricing
            .comparable_pricing("cursor", Some("cursor"), model)
            .map(|(schedule, basis)| {
                (
                    schedule.standard.base.input,
                    schedule.standard.base.output,
                    basis,
                )
            })
    };

    // When/Then: suffixed IDs resolve to their API model, Cursor's own models to the docs
    // table (the `-fast` ID taking the Fast lane), TOML prices win, and unknown IDs stay
    // unpriced.
    let api = PriceBasis::ApiEquivalent;
    assert_eq!(
        price("claude-opus-5-5-high"),
        Some((Some(4.0), Some(20.0), api))
    );
    assert_eq!(
        price("claude-opus-5-5-thinking-xhigh"),
        Some((Some(4.0), Some(20.0), api))
    );
    assert_eq!(
        price("gemini-3.1-pro-low"),
        Some((Some(2.0), Some(12.0), api))
    );
    assert_eq!(
        price("gpt-5-medium-fast"),
        Some((Some(1.25), Some(10.0), api))
    );
    assert_eq!(price("composer-2.5"), Some((Some(0.5), Some(2.5), api)));
    assert_eq!(
        price("composer-2.5-fast"),
        Some((Some(3.0), Some(15.0), api))
    );
    assert_eq!(price("gpt-5-high"), Some((Some(9.0), Some(10.0), api)));
    assert_eq!(price("mystery-model-high"), None);
}

#[tokio::test]
async fn cursor_ids_match_catalog_spellings_and_prefer_exact_names() {
    // Given: a catalog spelled as models.dev spells it (dotted OpenAI versions, a
    // `-preview` Gemini, a dotted Claude) that also lists `o3-mini-high` as a model of
    // its own next to `o3-mini`.
    let cost = |input: f64| serde_json::json!({"input": input, "output": input * 10.0});
    let catalog = catalog(serde_json::json!({
        "anthropic": {"models": {"claude-sonnet-4.5": {"id": "claude-sonnet-4.5", "cost": cost(3.0)}}},
        "openai": {"models": {
            "gpt-5.5": {"id": "gpt-5.5", "cost": cost(5.0)},
            "o3-mini": {"id": "o3-mini", "cost": cost(1.1)},
            "o3-mini-high": {"id": "o3-mini-high", "cost": cost(7.0)},
        }},
        "google": {"models": {"gemini-3.1-pro-preview": {"id": "gemini-3.1-pro-preview", "cost": cost(2.0)}}},
    }))
    .await;
    let pricing = UsagePricing::new(
        BTreeMap::from([("cursor".into(), profile(ProviderTypeConfig::Cursor))]),
        Some(catalog),
    );
    let input = |model: &str| {
        pricing
            .comparable_pricing("cursor", Some("cursor"), model)
            .and_then(|(schedule, _)| schedule.standard.base.input)
    };

    // When/Then: separator and preview spellings resolve, while a catalog model whose
    // own name carries the suffix beats the base model it would be stripped to.
    assert_eq!(input("gpt-5-5-high"), Some(5.0));
    assert_eq!(input("claude-sonnet-4-5-thinking-high"), Some(3.0));
    assert_eq!(input("gemini-3.1-pro-low"), Some(2.0));
    assert_eq!(input("o3-mini-high"), Some(7.0));
    assert_eq!(input("o3-mini-low"), Some(1.1));
}
