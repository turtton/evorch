use std::collections::BTreeMap;
use std::path::Path;

use catalog::{ModelCatalog, ModelMetadata};
use config::{Config, MetadataSource, ModelEntryConfig, ModelPresetConfig};

use crate::compaction::policy::CompactionSettings;

/// Highest-priority source contributing the resolved context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataOrigin {
    Manual,
    Preset,
    Catalog,
    Default,
}

/// Model limits; absent values retain the caller's defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelMetadata {
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub origin: MetadataOrigin,
}

/// Shared catalog selection for runtime limits and GUI prices, without modifying configuration.
pub fn resolve_catalog_entry<'a>(
    entry: &ModelEntryConfig,
    catalog: &'a ModelCatalog,
    provider_id: Option<&str>,
) -> Option<&'a ModelMetadata> {
    let reference = match entry.metadata_source {
        Some(MetadataSource::ModelsDev) | None => entry.metadata_ref.as_deref(),
        Some(MetadataSource::Manual | MetadataSource::ProviderDefault) => None,
    };
    let explicit = reference.and_then(|reference| {
        reference
            .split_once('/')
            .and_then(|(provider, model)| {
                catalog
                    .find(provider, model)
                    .or_else(|| catalog.find(provider, reference))
            })
            .or_else(|| catalog.find_unique_model(reference))
    });
    explicit.or_else(|| {
        provider_id
            .and_then(|provider| catalog.find(provider, &entry.id))
            .or_else(|| catalog.find_unique_model(&entry.id))
    })
}

/// Resolves each limit from manual overrides, presets, then the optional catalog.
pub fn resolve_model_metadata(
    entry: &ModelEntryConfig,
    presets: &BTreeMap<String, ModelPresetConfig>,
    catalog: Option<&ModelCatalog>,
    provider_id: Option<&str>,
) -> ResolvedModelMetadata {
    let preset = entry.preset.as_ref().and_then(|name| presets.get(name));
    let catalog_entry =
        catalog.and_then(|catalog| resolve_catalog_entry(entry, catalog, provider_id));
    let (context_window, origin) = if let Some(window) = entry.context_window {
        (Some(window), MetadataOrigin::Manual)
    } else if let Some(window) = preset.and_then(|preset| preset.context_window) {
        (Some(window), MetadataOrigin::Preset)
    } else if let Some(window) = catalog_entry.and_then(|model| model.context_window) {
        (Some(window), MetadataOrigin::Catalog)
    } else {
        (None, MetadataOrigin::Default)
    };
    ResolvedModelMetadata {
        context_window,
        max_output_tokens: preset
            .and_then(|preset| preset.max_output_tokens)
            .or_else(|| catalog_entry.and_then(|model| model.max_output_tokens)),
        origin,
    }
}

pub(crate) fn apply_model_windows(
    config: &Config,
    catalog: Option<&ModelCatalog>,
    settings: &mut CompactionSettings,
) {
    let mut windows = BTreeMap::new();
    for (name, profile) in &config.providers {
        for entry in profile.models.iter().filter(|entry| entry.enabled) {
            if let Some(window) =
                resolve_model_metadata(entry, &config.model_presets, catalog, Some(name))
                    .context_window
                    .filter(|window| *window > 0)
            {
                windows.entry(entry.id.clone()).or_insert(window);
                windows.insert(format!("{name}/{}", entry.id), window);
            }
        }
    }
    settings.model_overrides.extend(windows);
}

pub(crate) async fn load_catalog(cache_dir: &Path) -> Option<ModelCatalog> {
    match ModelCatalog::load_or_refresh(cache_dir).await {
        Ok(catalog) => Some(catalog),
        Err(error) => {
            tracing::warn!(%error, "model catalog unavailable; retaining configured defaults");
            None
        }
    }
}

pub(crate) struct ModelResolution {
    config: Config,
    catalog: tokio::sync::OnceCell<Option<ModelCatalog>>,
}

impl ModelResolution {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
            catalog: tokio::sync::OnceCell::new(),
        }
    }

    pub(crate) async fn apply(&self, settings: &mut CompactionSettings) {
        let needs_catalog = self
            .config
            .providers
            .values()
            .any(|profile| profile.models.iter().any(|entry| entry.enabled));
        let catalog = self
            .catalog
            .get_or_init(|| async {
                if needs_catalog {
                    load_catalog(&catalog::default_cache_dir()).await
                } else {
                    None
                }
            })
            .await;
        apply_model_windows(&self.config, catalog.as_ref(), settings);
    }
}

#[cfg(test)]
mod tests;
