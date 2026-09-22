use super::*;
use crate::compaction::policy::{CompactionSettings, resolve_window};
use config::{Config, MetadataSource, ModelEntryConfig, ModelPresetConfig, ProviderProfileConfig};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

const MODEL: &str = "deepseek-v4-flash-0731";

async fn catalog_fixture() -> catalog::ModelCatalog {
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let data = serde_json::json!({"fetched_at": now, "api": {
        "deepseek": {"models": {MODEL: {"id": MODEL,
            "limit": {"context": 64000, "output": 8000}}}}
    }});
    std::fs::write(dir.path().join("models-dev.json"), data.to_string()).unwrap();
    catalog::ModelCatalog::load_or_refresh(dir.path())
        .await
        .unwrap()
}

fn presets() -> BTreeMap<String, ModelPresetConfig> {
    BTreeMap::from([(
        "small".into(),
        ModelPresetConfig {
            context_window: Some(32000),
            max_output_tokens: Some(4000),
            ..ModelPresetConfig::default()
        },
    )])
}

#[tokio::test]
async fn manual_override_beats_preset_and_catalog() {
    let catalog = catalog_fixture().await;
    let mut entry = ModelEntryConfig::enabled(MODEL);
    entry.context_window = Some(16000);
    entry.preset = Some("small".into());

    let resolved =
        resolve_model_metadata(&entry, &presets(), Some(&catalog), Some("deepseek"), None);

    assert_eq!(resolved.context_window, Some(16000));
    assert_eq!(resolved.origin, MetadataOrigin::Manual);
    assert_eq!(resolved.max_output_tokens, Some(4000));
}

#[tokio::test]
async fn preset_beats_catalog() {
    let catalog = catalog_fixture().await;
    let mut entry = ModelEntryConfig::enabled(MODEL);
    entry.preset = Some("small".into());

    let resolved = resolve_model_metadata(&entry, &presets(), Some(&catalog), None, None);

    assert_eq!(resolved.context_window, Some(32000));
    assert_eq!(resolved.origin, MetadataOrigin::Preset);
}

#[tokio::test]
async fn catalog_used_when_no_preset() {
    let catalog = catalog_fixture().await;
    let entry = ModelEntryConfig::enabled(MODEL);

    let resolved =
        resolve_model_metadata(&entry, &presets(), Some(&catalog), Some("deepseek"), None);

    assert_eq!(resolved.context_window, Some(64000));
    assert_eq!(resolved.max_output_tokens, Some(8000));
    assert_eq!(resolved.origin, MetadataOrigin::Catalog);
}

#[test]
fn no_source_returns_none_and_falls_back_to_default() {
    let entry = ModelEntryConfig::enabled(MODEL);

    let resolved = resolve_model_metadata(&entry, &BTreeMap::new(), None, None, None);

    assert_eq!(resolved.context_window, None);
    assert_eq!(resolved.max_output_tokens, None);
    assert_eq!(resolved.origin, MetadataOrigin::Default);
}

fn configured(entry: ModelEntryConfig) -> Config {
    Config {
        providers: BTreeMap::from([(
            "custom-profile".into(),
            ProviderProfileConfig {
                models: vec![entry],
                default_model: MODEL.into(),
                ..ProviderProfileConfig::default()
            },
        )]),
        model_presets: presets(),
        ..Config::default()
    }
}

#[tokio::test]
async fn compaction_window_uses_resolved_metadata() {
    let catalog = catalog_fixture().await;
    let config = configured(ModelEntryConfig::enabled(MODEL));
    let mut settings = CompactionSettings::default();
    settings.model_overrides.insert(MODEL.into(), 100000);

    apply_model_windows(&config, Some(&catalog), &mut settings);

    assert_eq!(resolve_window(&settings, MODEL, None).0, 64000);
    assert_eq!(
        resolve_window(&settings, &format!("custom-profile/{MODEL}"), None).0,
        64000
    );
    assert_eq!(resolve_window(&settings, "unknown", None).0, 200000);
}

#[tokio::test]
async fn catalog_fetch_failure_keeps_existing_behavior() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let catalog = load_catalog(file.path()).await;
    let config = configured(ModelEntryConfig::enabled(MODEL));
    let mut settings = CompactionSettings::default();
    settings.model_overrides.insert(MODEL.into(), 48000);

    apply_model_windows(&config, catalog.as_ref(), &mut settings);

    assert!(catalog.is_none());
    assert_eq!(resolve_window(&settings, MODEL, None).0, 48000);
    assert_eq!(resolve_window(&settings, "unknown", None).0, 200000);
}

#[tokio::test]
async fn explicit_sources_auto_resolve_catalog() {
    let catalog = catalog_fixture().await;
    for source in [MetadataSource::Manual, MetadataSource::ProviderDefault] {
        let mut entry = ModelEntryConfig::enabled(MODEL);
        entry.metadata_source = Some(source);

        let resolved = resolve_model_metadata(&entry, &presets(), Some(&catalog), None, None);

        assert_eq!(resolved.context_window, Some(64000));
        assert_eq!(resolved.origin, MetadataOrigin::Catalog);
    }
}

#[path = "auto_catalog_tests.rs"]
mod auto_catalog_tests;

async fn limits_catalog(api: serde_json::Value) -> catalog::ModelCatalog {
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let data = serde_json::json!({"fetched_at": now, "api": api});
    std::fs::write(dir.path().join("models-dev.json"), data.to_string()).unwrap();
    catalog::ModelCatalog::load_or_refresh(dir.path())
        .await
        .unwrap()
}

#[tokio::test]
async fn kimi_subscription_entry_resolves_via_slug_candidates_to_1m() {
    // Given: subscription の両 slug が同じ上限を持ち、profile 名とは異なる。
    let catalog = limits_catalog(serde_json::json!({
        "kimi-code-plan-global": {"models": {"k3": {"id": "k3",
            "limit": {"context": 1_048_576, "output": 65_536}}}},
        "kimi-code-plan-cn": {"models": {"k3": {"id": "k3",
            "limit": {"context": 1_048_576, "output": 65_536}}}}
    }))
    .await;
    let config: Config = serde_json::from_value(serde_json::json!({
        "providers": {"kimi": {"type": "kimi-subscription", "models": ["k3"]}}
    }))
    .unwrap();
    let mut settings = CompactionSettings::default();

    // When: profile のモデル上限を適用する。
    apply_model_windows(&config, Some(&catalog), &mut settings);

    // Then: bare ID と compaction 用の profile/model の両方に 1M を設定する。
    assert_eq!(settings.model_overrides.get("k3"), Some(&1_048_576));
    assert_eq!(settings.model_overrides.get("kimi/k3"), Some(&1_048_576));
}

#[tokio::test]
async fn metadata_ref_selects_catalog_model_without_changing_entry_id() {
    let catalog = catalog_fixture().await;
    let mut entry = ModelEntryConfig::enabled("local-alias");
    entry.metadata_source = Some(MetadataSource::ModelsDev);
    entry.metadata_ref = Some(format!("deepseek/{MODEL}"));

    let resolved = resolve_model_metadata(&entry, &presets(), Some(&catalog), None, None);

    assert_eq!(resolved.context_window, Some(64000));
    assert_eq!(entry.id, "local-alias");
}
