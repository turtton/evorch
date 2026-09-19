use super::policy::{CompactionSettings, ThresholdDecision, resolve_window, threshold_decision};

#[test]
fn catalog_window_triggers_at_24000_when_window_is_32000() {
    // Given: the default threshold and a 32K catalog entry.
    let settings = CompactionSettings::default();
    // When: resolving the selected model's window.
    let (window, source) = resolve_window(&settings, "small", Some(32_000));
    // Then: the boundary is inclusive at 24K, not 150K.
    assert_eq!(source, event_bus::WindowSource::Catalog);
    assert_eq!(
        threshold_decision(&settings, 23_999, window),
        ThresholdDecision::BelowThreshold
    );
    assert_eq!(
        threshold_decision(&settings, 24_000, window),
        ThresholdDecision::Trigger
    );
}

#[test]
fn override_wins_when_catalog_window_exists() {
    // Given: distinct configured and catalog windows.
    let mut settings = CompactionSettings::default();
    settings.model_overrides.insert("small".into(), 64_000);
    // When: resolving the selected model.
    let resolved = resolve_window(&settings, "small", Some(32_000));
    // Then: the explicit override wins.
    assert_eq!(resolved, (64_000, event_bus::WindowSource::Override));
}

#[test]
fn default_window_is_200000_when_catalog_is_missing() {
    // Given: no per-model metadata.
    let settings = CompactionSettings::default();
    // When: resolving an unknown model.
    let resolved = resolve_window(&settings, "unknown", None);
    // Then: the unchanged default applies.
    assert_eq!(resolved, (200_000, event_bus::WindowSource::Default));
}
