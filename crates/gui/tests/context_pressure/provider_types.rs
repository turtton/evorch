use gui::model::telemetry::TelemetryOverlay;

#[path = "../support/provider_metadata.rs"]
mod provider_metadata;

#[tokio::test]
async fn pressure_uses_profile_slug_when_carrier_limits_disagree() {
    // Given: neuralwatt has a distinct window among disagreeing carriers.
    let (settings, event) = provider_metadata::profile_slug_fixture().await;
    let mut overlay = TelemetryOverlay::new();
    overlay.apply_event(&event);
    // When: the GUI resolves the latest request's context window.
    overlay.refresh_costs(&settings);
    // Then: (450000 * 100 + 900000 / 2) / 900000 = 50, not other carriers' 43 or 45.
    assert_eq!(
        overlay
            .row("run-1")
            .unwrap()
            .context_pressure_label()
            .as_deref(),
        Some("50%")
    );
}

#[tokio::test]
async fn kimi_subscription_pressure_uses_slug_candidate_window() {
    // Given: agreeing Kimi slugs and a conflicting unrelated carrier.
    let settings =
        provider_metadata::settings("kimi", config::ProviderTypeConfig::KimiSubscription, "k3")
            .await;
    let mut overlay = TelemetryOverlay::new();
    // When: usage for the configured profile is resolved.
    overlay.apply_event(&provider_metadata::completed(
        "kimi-subscription",
        "kimi",
        "k3",
    ));
    overlay.refresh_costs(&settings);
    // Then: the subscription window determines pressure.
    assert_eq!(
        overlay
            .row("run-1")
            .unwrap()
            .context_pressure_label()
            .as_deref(),
        Some("48%")
    );
}

#[tokio::test]
async fn openai_compatible_profile_pressure_uses_agreeing_duplicates() {
    // Given: duplicate models.dev carriers with agreeing limits.
    let settings = provider_metadata::settings(
        "neuralwatt",
        config::ProviderTypeConfig::OpenAiCompatible,
        "kimi-k3",
    )
    .await;
    let mut overlay = TelemetryOverlay::new();
    // When: usage for the compatible profile is resolved.
    overlay.apply_event(&provider_metadata::completed(
        "openai-compatible",
        "neuralwatt",
        "kimi-k3",
    ));
    overlay.refresh_costs(&settings);
    // Then: duplicate carriers do not hide the context window.
    assert_eq!(
        overlay
            .row("run-1")
            .unwrap()
            .context_pressure_label()
            .as_deref(),
        Some("48%")
    );
}
