use super::*;
use crate::compaction::policy::resolve_window;

#[tokio::test]
async fn reload_between_capture_and_resolution_keeps_policy_and_metadata_together() {
    let bus = Arc::new(EventBus::new(16));
    let model = Arc::new(crate::compose::UnconfiguredModel);
    let mut config = config::Config::default();
    let mut entry = config::ModelEntryConfig::enabled("fixture-model");
    entry.metadata_source = Some(config::MetadataSource::Manual);
    entry.context_window = Some(200_000);
    config.providers.insert(
        "local".into(),
        config::ProviderProfileConfig {
            models: vec![entry],
            ..Default::default()
        },
    );
    let cache = Arc::new(providers::CodexCatalogCache::default());
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
        .with_model_resolution_and_catalog_cache(
            &config,
            None,
            Some(test_support::model_catalog_dir()),
            cache.clone(),
        );
    let mut captured = loop_shared(&Arc::downgrade(&runtime.shared), None).unwrap();

    config.providers.get_mut("local").unwrap().models[0].context_window = Some(400_000);
    config.compaction.threshold = 0.5;
    runtime.reload_model_settings(&config);
    let mut current = loop_shared(&Arc::downgrade(&runtime.shared), None).unwrap();

    // Resolve only after publication: neither the old pending resolution nor
    // its policy may be substituted with the new generation.
    captured.resolve_compaction().await;
    current.resolve_compaction().await;
    assert_eq!(
        resolve_window(&captured.compaction, "local/fixture-model", None),
        (200_000, event_bus::WindowSource::Override)
    );
    assert_eq!(captured.compaction.threshold, 0.75);
    assert_eq!(
        resolve_window(&current.compaction, "local/fixture-model", None),
        (400_000, event_bus::WindowSource::Override)
    );
    assert_eq!(current.compaction.threshold, 0.5);
    assert!(Arc::ptr_eq(&cache, &runtime.codex_catalog_cache().unwrap()));
}
