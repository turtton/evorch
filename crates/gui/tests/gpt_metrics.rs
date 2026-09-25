use std::sync::Arc;

use config::{Config, ModelEntryConfig, ProviderProfileConfig, ProviderTypeConfig};
use event_bus::{ContextComposition, Event, LifecycleEvent, ProviderEvent, RunActivity};
use gui::model::{provider_settings::ProviderSettingsModel, telemetry::TelemetryOverlay};

const MODEL: &str = "gpt-test";

async fn settings(provider_type: ProviderTypeConfig) -> ProviderSettingsModel {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("models-dev.json"),
        serde_json::json!({"fetched_at": 4_102_444_800_u64, "api": {
            "openai": {"models": {MODEL: {"id": MODEL,
                "limit": {"context": 1_000, "output": 200},
                "cost": {"input": 2.0, "output": 10.0, "cache_read": 0.2}}}},
            "other-carrier": {"models": {MODEL: {"id": MODEL,
                "limit": {"context": 2_000, "output": 400},
                "cost": {"input": 20.0, "output": 100.0}}}}
        }})
        .to_string(),
    )
    .unwrap();
    let mut config = Config::default();
    config.providers.insert(
        "personal".into(),
        ProviderProfileConfig {
            provider_type,
            models: vec![ModelEntryConfig::enabled(MODEL)],
            ..Default::default()
        },
    );
    let mut settings = ProviderSettingsModel::seed_from_config(&config);
    settings.catalog.catalog = Some(Arc::new(
        catalog::ModelCatalog::load_or_refresh(dir.path())
            .await
            .unwrap(),
    ));
    settings
}

fn completed(provider: &str, profile: Option<&str>) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: "request".into(),
        provider: provider.into(),
        profile: profile.map(str::to_owned),
        protocol: "openai-responses".into(),
        model: MODEL.into(),
        streaming: true,
        duration_ms: 2_000,
        input_tokens: 800,
        output_tokens: 100,
        cache_read_tokens: 600,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        run_id: Some("run-1".into()),
    })
}

#[tokio::test]
async fn gpt_metrics_select_openai_despite_conflicting_carrier_metadata() {
    for (provider, provider_type) in [
        ("openai", ProviderTypeConfig::OpenAi),
        ("openai-codex", ProviderTypeConfig::OpenAiCodex),
    ] {
        let settings = settings(provider_type).await;
        // Includes direct clients / persisted Codex events without profile attribution.
        for profile in [Some("personal"), None] {
            let mut telemetry = TelemetryOverlay::new();
            telemetry.apply_event(&completed(provider, profile));
            telemetry.refresh_costs(&settings);
            let metrics = telemetry.thread_metrics(&["run-1".into()]);
            assert!((metrics.cost.unwrap() - 0.00152).abs() < 1e-10);
            assert_eq!(metrics.context_pressure, Some(90));
        }
    }
}

#[tokio::test]
async fn codex_profile_manual_metadata_wins_over_catalog() {
    let mut config = Config::default();
    let mut entry = ModelEntryConfig::enabled(MODEL);
    entry.context_window = Some(2_000);
    entry.input_price = Some(5.0);
    entry.output_price = Some(20.0);
    entry.cache_read_price = Some(0.5);
    config.providers.insert(
        "personal".into(),
        ProviderProfileConfig {
            provider_type: ProviderTypeConfig::OpenAiCodex,
            models: vec![entry],
            ..Default::default()
        },
    );
    let mut configured = ProviderSettingsModel::seed_from_config(&config);
    configured.catalog = settings(ProviderTypeConfig::OpenAiCodex).await.catalog;
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&completed("openai-codex", Some("personal")));
    telemetry.refresh_costs(&configured);
    let metrics = telemetry.thread_metrics(&["run-1".into()]);
    assert!((metrics.cost.unwrap() - 0.0033).abs() < 1e-10);
    assert_eq!(metrics.context_pressure, Some(45));
}

#[tokio::test]
async fn context_pressure_uses_window_resolved_by_runtime() {
    let settings = settings(ProviderTypeConfig::OpenAiCodex).await;
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&Event::new(LifecycleEvent::RunProgress {
        run_id: "run-1".into(),
        activity: RunActivity::Model,
        context: Some(ContextComposition {
            window_tokens: 2_000,
            ..Default::default()
        }),
    }));
    telemetry.apply_event(&completed("openai-codex", Some("personal")));
    telemetry.refresh_costs(&settings);
    assert_eq!(telemetry.row("run-1").unwrap().context_pressure(), Some(45));
}
