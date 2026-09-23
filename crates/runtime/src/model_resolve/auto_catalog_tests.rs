use super::*;

async fn fixture(models: &[(&str, &str)]) -> catalog::ModelCatalog {
    let dir = tempfile::tempdir().unwrap();
    let mut api = serde_json::Map::new();
    for (provider, id) in models {
        api.insert(
            (*provider).into(),
            serde_json::json!({"models": {
                *id: {"id": id, "limit": {"context": 64000, "output": 8000}}
            }}),
        );
    }
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
async fn unknown_model_auto_resolves_via_catalog() {
    // Given: a custom provider profile with no metadata settings.
    let catalog = catalog_fixture().await;
    let entry = ModelEntryConfig::enabled(MODEL);
    // When: resolving independently of the profile name.
    let result = resolve_model_metadata(&entry, &presets(), Some(&catalog), Some("Crof"), None);
    // Then: use catalog limits.
    assert_eq!(result.origin, MetadataOrigin::Catalog);
    assert_eq!(result.context_window, Some(64000));
    assert_eq!(result.max_output_tokens, Some(8000));
}

#[tokio::test]
async fn deepseek_model_resolves_to_provider_prefixed_catalog_entry() {
    // Given: only a namespaced catalog ID, with no direct model match.
    let catalog = fixture(&[("deepseek", "deepseek/deepseek-v4-flash")]).await;
    let entry = ModelEntryConfig::enabled("deepseek-v4-flash");
    assert!(catalog.find_by_model_id(&entry.id).is_none());
    // When: resolving a model returned by a custom provider.
    let result = resolve_model_metadata(&entry, &presets(), Some(&catalog), Some("Crof"), None);
    // Then: infer the namespaced model.
    assert_eq!(result.origin, MetadataOrigin::Catalog);
    assert_eq!(result.context_window, Some(64000));
}

#[tokio::test]
async fn ambiguous_catalog_match_returns_none() {
    // Given: duplicate IDs/suffixes with disagreeing limits remain unresolved.
    for ids in [["shared", "shared"], ["first/shared", "second/shared"]] {
        let catalog = limits_catalog(serde_json::json!({
            "first": {"models": {ids[0]: {"id": ids[0],
                "limit": {"context": 64000, "output": 8000}}}},
            "second": {"models": {ids[1]: {"id": ids[1],
                "limit": {"context": 128000, "output": 8000}}}}
        }))
        .await;
        let entry = ModelEntryConfig::enabled("shared");
        // When: no provider can disambiguate the model.
        let result = resolve_model_metadata(&entry, &presets(), Some(&catalog), None, None);
        // Then: refuse to guess.
        assert_eq!(result.context_window, None);
        assert_eq!(result.max_output_tokens, None);
        assert_eq!(result.origin, MetadataOrigin::Default);
    }
}

#[tokio::test]
async fn agreeing_duplicates_resolve_without_any_provider_hint() {
    // Given: カタログの両 provider が同じ上限を持ち、profile 名とは一致しない。
    let catalog = limits_catalog(serde_json::json!({
        "moonshotai": {"models": {"kimi-k3": {"id": "kimi-k3",
            "limit": {"context": 1_048_576, "output": 65_536}}}},
        "moonshotai-cn": {"models": {"kimi-k3": {"id": "kimi-k3",
            "limit": {"context": 1_048_576, "output": 65_536}}}}
    }))
    .await;
    let config: Config = serde_json::from_value(serde_json::json!({
        "providers": {"neuralwatt": {"type": "openai-compatible",
            "models": [{"id": "kimi-k3", "metadata_source": "models-dev"}]}}
    }))
    .unwrap();
    let mut settings = CompactionSettings::default();

    // When: provider hint に依存せずモデル上限を適用する。
    apply_model_windows(&config, Some(&catalog), &BTreeMap::new(), &mut settings);

    // Then: 両方のキーが合意した 1M の上限になる。
    assert_eq!(settings.catalog_windows.get("kimi-k3"), Some(&1_048_576));
    assert_eq!(
        settings.catalog_windows.get("neuralwatt/kimi-k3"),
        Some(&1_048_576)
    );
}

#[tokio::test]
async fn disagreeing_duplicates_stay_unresolved() {
    // Given: 同じモデル ID でも context window が異なり、slug も一致しない。
    let catalog = limits_catalog(serde_json::json!({
        "moonshotai": {"models": {"kimi-k3": {"id": "kimi-k3",
            "limit": {"context": 1_048_576, "output": 65_536}}}},
        "moonshotai-cn": {"models": {"kimi-k3": {"id": "kimi-k3",
            "limit": {"context": 262_144, "output": 65_536}}}}
    }))
    .await;
    let config: Config = serde_json::from_value(serde_json::json!({
        "providers": {"neuralwatt": {"type": "openai-compatible",
            "models": [{"id": "kimi-k3", "metadata_source": "models-dev"}]}}
    }))
    .unwrap();
    let mut settings = CompactionSettings::default();

    // When: 合意できないモデル上限を適用する。
    apply_model_windows(&config, Some(&catalog), &BTreeMap::new(), &mut settings);

    // Then: override を書かず既定値へのフォールバックを維持する。
    assert!(!settings.catalog_windows.contains_key("kimi-k3"));
    assert!(!settings.catalog_windows.contains_key("neuralwatt/kimi-k3"));
}

#[tokio::test]
async fn catalog_unknown_keeps_default() {
    // Given: similar names must not match arbitrary substrings or versions.
    let catalog = fixture(&[("deepseek", "deepseek/deepseek-v4-flash")]).await;
    for id in ["unknown", "flash", "deepseek-v4", ""] {
        let entry = ModelEntryConfig::enabled(id);
        // When: looking up an absent model.
        let result = resolve_model_metadata(&entry, &presets(), Some(&catalog), Some("Crof"), None);
        // Then: keep the caller's defaults.
        assert_eq!(result.context_window, None);
        assert_eq!(result.origin, MetadataOrigin::Default);
    }
}

#[tokio::test]
async fn manual_and_preset_still_take_priority_over_auto_catalog() {
    // Given: overrides differ from the inferred catalog limits.
    let catalog = catalog_fixture().await;
    for (manual, window, origin) in [
        (Some(16000), 16000, MetadataOrigin::Manual),
        (None, 32000, MetadataOrigin::Preset),
    ] {
        let mut entry = ModelEntryConfig::enabled(MODEL);
        entry.context_window = manual;
        entry.preset = Some("small".into());
        // When: catalog is automatically available through another provider.
        let result = resolve_model_metadata(&entry, &presets(), Some(&catalog), Some("Crof"), None);
        // Then: explicit configuration still wins for both limits.
        assert_eq!(result.context_window, Some(window));
        assert_eq!(result.origin, origin);
        assert_eq!(result.max_output_tokens, Some(4000));
    }
}
