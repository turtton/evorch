use config::ModelEntryConfig;
use gui::model::model_metadata::MetadataSources;

#[path = "../support/provider_metadata.rs"]
mod provider_metadata;

#[tokio::test]
async fn pricing_uses_profile_slug_when_carrier_prices_differ() {
    // Given: neuralwatt input costs $10/M, unlike moonshotai's $2/M.
    let (settings, event) = provider_metadata::profile_slug_fixture().await;
    let mut overlay = gui::model::telemetry::TelemetryOverlay::new();
    overlay.apply_event(&event);
    // When: estimating usage attributed to the neuralwatt profile.
    let cost = overlay.estimated_cost("run-1", &settings);
    // Then: 450,000 input tokens use neuralwatt's price, not the other carriers'.
    assert_eq!(cost, Some(4.5));
}

#[tokio::test]
async fn model_row_labels_resolve_via_provider_type() {
    // Given: Kimi slug metadata competing with an unrelated carrier.
    let settings =
        provider_metadata::settings("kimi", config::ProviderTypeConfig::KimiSubscription, "k3")
            .await;
    let sources = MetadataSources {
        presets: &settings.model_presets,
        catalog: settings.catalog.catalog.as_deref(),
    };
    // When: resolving labels for the subscription profile.
    let labels = sources.labels(
        &ModelEntryConfig::enabled("k3"),
        "kimi",
        Some(config::ProviderTypeConfig::KimiSubscription),
    );
    // Then: the subscription context window is displayed.
    assert_eq!(labels[0], "1048576 ctx (models.dev)");
}

#[tokio::test]
async fn pricing_resolves_via_provider_type() {
    // Given: catalog-only input pricing on the subscription slugs.
    let settings =
        provider_metadata::settings("kimi", config::ProviderTypeConfig::KimiSubscription, "k3")
            .await;
    let mut overlay = gui::model::telemetry::TelemetryOverlay::new();
    overlay.apply_event(&provider_metadata::completed(
        "kimi-subscription",
        "kimi",
        "k3",
    ));
    // When: estimating the profile's billed usage.
    let cost = overlay.estimated_cost("run-1", &settings);
    // Then: 500,000 input tokens at $2/M cost $1.
    assert_eq!(cost, Some(1.0));
}
