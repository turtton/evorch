use super::*;
use event_bus::{EventKind, MessageEvent};

#[test]
fn live_pressure_uses_new_model_window_with_previous_usage_baseline() {
    for profile in [Some("remote".to_owned()), None] {
        // Given: completed model A usage and distinct windows for both models.
        let mut config = Config::default();
        let mut previous = ModelEntryConfig::enabled("model");
        previous.context_window = Some(1000);
        config.providers.insert(
            "local".into(),
            ProviderProfileConfig {
                models: vec![previous],
                ..Default::default()
            },
        );
        let mut entry = ModelEntryConfig::enabled("other");
        entry.context_window = Some(2000);
        config.providers.insert(
            profile.as_deref().unwrap_or("other-vendor").into(),
            ProviderProfileConfig {
                models: vec![entry],
                ..Default::default()
            },
        );
        let settings = ProviderSettingsModel::seed_from_config(&config);
        let mut overlay = TelemetryOverlay::new();
        overlay.apply_event(&completed(800));
        overlay.apply_event(&completed(100));
        overlay.refresh_costs(&settings);
        let billed_usage = overlay.row("run-1").unwrap().usage;

        // When: model B starts on a new provider/profile and streams 100 tokens.
        overlay.apply_event(&Event::new(ProviderEvent::RequestStarted {
            request_id: "next".into(),
            provider: "other-vendor".into(),
            profile: profile.clone(),
            protocol: "fixture".into(),
            model: "other".into(),
            streaming: true,
            run_id: Some("run-1".into()),
        }));
        overlay.apply_event(&Event::new(MessageEvent::MessageDelta {
            delta: "x".repeat(400),
            run_id: Some("run-1".into()),
        }));
        overlay.refresh_costs(&settings);

        // Then: latest input/cache baseline + live output uses B's denominator.
        let row = overlay.row("run-1").unwrap();
        assert_eq!(row.context_pressure(), Some(25)); // (100 + 200 + 100 + 100) / 2000
        assert_eq!(row.usage, billed_usage);
        assert_eq!(row.output_tokens, 100);

        let mut completion = completed(600);
        if let EventKind::Provider(ProviderEvent::RequestCompleted {
            provider,
            profile: completed_profile,
            model,
            ..
        }) = &mut completion.kind
        {
            *provider = "other-vendor".into();
            *completed_profile = profile;
            *model = "other".into();
        }
        overlay.apply_event(&completion);
        overlay.refresh_costs(&settings);
        assert_eq!(overlay.row("run-1").unwrap().context_pressure(), Some(90));
    }
}
