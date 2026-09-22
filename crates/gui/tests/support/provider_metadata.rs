use std::sync::Arc;

use config::{Config, MetadataSource, ModelEntryConfig, ProviderProfileConfig, ProviderTypeConfig};
use event_bus::{Event, ProviderEvent};
use gui::model::provider_settings::ProviderSettingsModel;

pub async fn catalog(model: &str, carriers: &[&str]) -> catalog::ModelCatalog {
    let dir = tempfile::tempdir().unwrap();
    let mut api = serde_json::Map::new();
    for carrier in carriers {
        api.insert(
            (*carrier).into(),
            serde_json::json!({"models": {
                model: {"id": model, "limit": {"context": 1_048_576, "output": 32_768},
                    "cost": {"input": 2.0}}
            }}),
        );
    }
    if model == "k3" {
        api.insert(
            "unrelated".into(),
            serde_json::json!({"models": {
                model: {"id": model, "limit": {"context": 64_000, "output": 4_000}}
            }}),
        );
    }
    // A future cache timestamp keeps this fixture offline without wall-clock assertions.
    std::fs::write(
        dir.path().join("models-dev.json"),
        serde_json::json!({
            "fetched_at": 4_102_444_800_u64, "api": api
        })
        .to_string(),
    )
    .unwrap();
    catalog::ModelCatalog::load_or_refresh(dir.path())
        .await
        .unwrap()
}

pub async fn settings(
    profile: &str,
    provider_type: ProviderTypeConfig,
    model: &str,
) -> ProviderSettingsModel {
    let mut config = Config::default();
    let mut entry = ModelEntryConfig::enabled(model);
    entry.metadata_source = Some(MetadataSource::ModelsDev);
    config.providers.insert(
        profile.into(),
        ProviderProfileConfig {
            provider_type,
            models: vec![entry],
            ..Default::default()
        },
    );
    let mut settings = ProviderSettingsModel::seed_from_config(&config);
    let carriers = match provider_type {
        ProviderTypeConfig::KimiSubscription => ["kimi-code-plan-global", "kimi-code-plan-cn"],
        _ => ["moonshotai", "moonshotai-cn"],
    };
    settings.catalog.catalog = Some(Arc::new(catalog(model, &carriers).await));
    settings
}

pub fn completed(provider: &str, profile: &str, model: &str) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: "request".into(),
        provider: provider.into(),
        profile: Some(profile.into()),
        protocol: "fixture".into(),
        model: model.into(),
        streaming: true,
        duration_ms: 10,
        input_tokens: 500_000,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        run_id: Some("run-1".into()),
    })
}

pub async fn profile_slug_fixture() -> (ProviderSettingsModel, Event) {
    let dir = tempfile::tempdir().unwrap();
    let mut api = serde_json::Map::new();
    for (carrier, window, input_price) in [
        ("neuralwatt", 900_000, 10.0),
        ("moonshotai", 1_048_576, 2.0),
        ("moonshotai-cn", 1_048_576, 2.0),
        ("vivgrid", 1_000_000, 3.0),
    ] {
        api.insert(
            carrier.into(),
            serde_json::json!({"models": {
                "kimi-k3": {"id": "kimi-k3", "limit": {"context": window, "output": 32_768},
                    "cost": {"input": input_price}}
            }}),
        );
    }
    std::fs::write(
        dir.path().join("models-dev.json"),
        serde_json::json!({"fetched_at": 4_102_444_800_u64, "api": api}).to_string(),
    )
    .unwrap();
    let mut settings = settings(
        "neuralwatt",
        ProviderTypeConfig::OpenAiCompatible,
        "kimi-k3",
    )
    .await;
    settings.catalog.catalog = Some(Arc::new(
        catalog::ModelCatalog::load_or_refresh(dir.path())
            .await
            .unwrap(),
    ));
    let event = Event::new(ProviderEvent::RequestCompleted {
        request_id: "profile-slug".into(),
        provider: "openai-compatible".into(),
        profile: Some("neuralwatt".into()),
        protocol: "fixture".into(),
        model: "kimi-k3".into(),
        streaming: true,
        duration_ms: 10,
        input_tokens: 450_000,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        run_id: Some("run-1".into()),
    });
    (settings, event)
}
