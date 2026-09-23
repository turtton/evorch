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

    apply_model_windows(&config, Some(&catalog), &BTreeMap::new(), &mut settings);

    assert_eq!(resolve_window(&settings, MODEL, None).0, 100000);
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

    apply_model_windows(&config, catalog.as_ref(), &BTreeMap::new(), &mut settings);

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
    apply_model_windows(&config, Some(&catalog), &BTreeMap::new(), &mut settings);

    // Then: bare ID と compaction 用の profile/model の両方に 1M を設定する。
    assert_eq!(settings.catalog_windows.get("k3"), Some(&1_048_576));
    assert_eq!(settings.catalog_windows.get("kimi/k3"), Some(&1_048_576));
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

#[tokio::test]
async fn provider_window_beats_catalog_but_not_manual_or_compaction_override() {
    let catalog = limits_catalog(serde_json::json!({
        "openai": {"models": {"gpt-a": {"id": "gpt-a",
            "limit": {"context": 1_050_000, "output": 128_000}}}}
    }))
    .await;
    let mut config: Config = serde_json::from_value(serde_json::json!({
        "providers": {"codex": {"type": "openai-codex", "models": [
            {"id": "gpt-a"}, {"id": "gpt-b", "context_window": 40000}
        ], "default_model": "gpt-a"}}
    }))
    .unwrap();
    let provider_windows = BTreeMap::from([
        ("codex/gpt-a".to_owned(), 272_000),
        ("codex/gpt-b".to_owned(), 272_000),
    ]);
    let mut settings = CompactionSettings::default();
    settings
        .model_overrides
        .insert("codex/gpt-a".into(), 300_000);
    apply_model_windows(&config, Some(&catalog), &provider_windows, &mut settings);
    assert_eq!(settings.model_overrides["codex/gpt-a"], 300_000);
    assert_eq!(settings.model_overrides["codex/gpt-b"], 40_000);
    let mut settings = CompactionSettings::default();
    apply_model_windows(&config, Some(&catalog), &provider_windows, &mut settings);
    assert_eq!(settings.catalog_windows["codex/gpt-a"], 272_000);
    assert_eq!(
        resolve_window(&settings, "codex/gpt-a", None),
        (272_000, event_bus::WindowSource::Catalog)
    );
    config.providers.get_mut("codex").unwrap().models[0].context_window = Some(500_000);
    let mut settings = CompactionSettings::default();
    apply_model_windows(&config, Some(&catalog), &provider_windows, &mut settings);
    assert_eq!(settings.model_overrides["codex/gpt-a"], 500_000);
}

#[tokio::test]
async fn codex_models_response_supplies_runtime_window_for_base_and_fast() {
    use providers::provider::codex::tokens::TokenBundle;
    use sandbox::CredentialStore;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("client_version", "0.156.1"))
        .and(header("authorization", "Bearer test-access"))
        .and(header("chatgpt-account-id", "test-account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"slug": "gpt-6-astra", "context_window": 272000,
                "max_context_window": 872000, "service_tiers": [{"id": "priority"}] }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(sandbox::FileCredentialStore::open(dir.path()).unwrap());
    let bundle = TokenBundle {
        access_token: "test-access".into(),
        refresh_token: "unused".into(),
        id_token: "header.eyJleHAiOjE4NDQ2NzQ0MDczNzA5NTUxNjE1LCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoidGVzdC1hY2NvdW50In19.signature".into(),
    };
    store
        .set(
            "test-key",
            &sandbox::Secret::from(serde_json::to_string(&bundle).unwrap()),
        )
        .unwrap();
    let mut config = Config::default();
    config.providers.insert(
        "codex".into(),
        ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAiCodex,
            base_url: server.uri(),
            credential: config::CredentialRefConfig::Keyring {
                service: "test".into(),
                account: "test-key".into(),
            },
            models: vec![
                ModelEntryConfig::enabled("gpt-6-astra"),
                ModelEntryConfig::enabled("gpt-6-astra+fast"),
            ],
            default_model: "gpt-6-astra".into(),
            ..Default::default()
        },
    );
    let store: std::sync::Arc<dyn CredentialStore> = store;
    let windows = load_codex_windows(&config, &store, "0.156.1").await;
    assert_eq!(windows["codex/gpt-6-astra"], 272_000);
    assert_eq!(windows["codex/gpt-6-astra+fast"], 272_000);
    let mut settings = CompactionSettings::default();
    apply_model_windows(&config, None, &windows, &mut settings);
    assert_eq!(settings.catalog_windows["codex/gpt-6-astra"], 272_000);
    assert_eq!(settings.catalog_windows["codex/gpt-6-astra+fast"], 272_000);
}
