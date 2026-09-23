use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use providers::provider::codex::tokens::{CodexTokenStore, parse_jwt_claims};
use sandbox::CredentialStore;

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
    provider_type: Option<config::ProviderTypeConfig>,
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
        provider_type
            .map(config::ProviderTypeConfig::models_dev_provider_candidates)
            .unwrap_or(&[])
            .iter()
            .find_map(|slug| catalog.find(slug, &entry.id))
            .or_else(|| provider_id.and_then(|provider| catalog.find(provider, &entry.id)))
            .or_else(|| catalog.find_unique_model(&entry.id))
            .or_else(|| catalog.find_agreeing_model(&entry.id))
    })
}

/// Resolves each limit from manual overrides, presets, then the optional catalog.
pub fn resolve_model_metadata(
    entry: &ModelEntryConfig,
    presets: &BTreeMap<String, ModelPresetConfig>,
    catalog: Option<&ModelCatalog>,
    provider_id: Option<&str>,
    provider_type: Option<config::ProviderTypeConfig>,
) -> ResolvedModelMetadata {
    let preset = entry.preset.as_ref().and_then(|name| presets.get(name));
    let catalog_entry = catalog
        .and_then(|catalog| resolve_catalog_entry(entry, catalog, provider_id, provider_type));
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
    provider_windows: &BTreeMap<String, u64>,
    settings: &mut CompactionSettings,
) {
    let mut configured = BTreeMap::new();
    let mut discovered = BTreeMap::new();
    for (name, profile) in &config.providers {
        for entry in profile.models.iter().filter(|entry| entry.enabled) {
            let resolved = resolve_model_metadata(
                entry,
                &config.model_presets,
                catalog,
                Some(name),
                Some(profile.provider_type),
            );
            let key = format!("{name}/{}", entry.id);
            let provider_window = provider_windows
                .get(&key)
                .copied()
                .filter(|window| *window > 0);
            // Manual and preset values win. Otherwise the provider's own limit is
            // more specific than models.dev's general metadata.
            let window = match resolved.origin {
                MetadataOrigin::Manual | MetadataOrigin::Preset => resolved.context_window,
                MetadataOrigin::Catalog | MetadataOrigin::Default => {
                    provider_window.or(resolved.context_window)
                }
            };
            if let Some(window) = window.filter(|window| *window > 0) {
                let target = match resolved.origin {
                    MetadataOrigin::Manual | MetadataOrigin::Preset => &mut configured,
                    MetadataOrigin::Catalog | MetadataOrigin::Default => &mut discovered,
                };
                target.entry(entry.id.clone()).or_insert(window);
                target.insert(key, window);
            }
        }
    }
    // Explicit [compaction.model_overrides] takes precedence over model settings.
    for (model, window) in configured {
        settings.model_overrides.entry(model).or_insert(window);
    }
    settings.catalog_windows.extend(discovered);
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

/// Read Codex-specific limits from the same /models endpoint used by the settings UI.
/// Network or credential failures leave models.dev/configuration as the fallback.
async fn load_codex_windows(
    config: &Config,
    store: &Arc<dyn CredentialStore>,
    client_version: &str,
) -> BTreeMap<String, u64> {
    let mut windows = BTreeMap::new();
    let codex_profiles: Vec<_> = config
        .providers
        .iter()
        .filter(|(_, profile)| profile.provider_type == config::ProviderTypeConfig::OpenAiCodex)
        .collect();
    if codex_profiles.is_empty() {
        return windows;
    }
    for (name, profile) in codex_profiles {
        let config::CredentialRefConfig::Keyring { account, .. } = &profile.credential else {
            continue;
        };
        let token_store =
            routing::factory::CredentialStoreTokenStore::new(Arc::clone(store), account.clone());
        let tokens = match token_store.load() {
            Ok(Some(tokens)) => tokens,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(profile = %name, %error, "Codex model metadata credentials unavailable");
                continue;
            }
        };
        let claims = match parse_jwt_claims(&tokens.id_token) {
            Ok(claims) => claims,
            Err(error) => {
                tracing::warn!(profile = %name, %error, "Codex model metadata identity unavailable");
                continue;
            }
        };
        match providers::list_codex_models(
            &profile.base_url,
            &providers::ProviderAuth::new(tokens.access_token),
            &claims.chatgpt_account_id,
            client_version,
        )
        .await
        {
            Ok(models) => {
                for model in models {
                    if let Some(window) = model.context_window.filter(|window| *window > 0) {
                        windows.insert(format!("{name}/{}", model.slug), window);
                        windows.insert(
                            format!(
                                "{name}/{}",
                                config::types::provider::fast_variant_id(&model.slug)
                            ),
                            window,
                        );
                    }
                }
            }
            Err(providers::ProviderError::Http { status, .. }) => {
                tracing::warn!(profile = %name, status, "Codex model metadata unavailable")
            }
            Err(error) => {
                tracing::warn!(profile = %name, %error, "Codex model metadata unavailable")
            }
        }
    }
    windows
}

pub(crate) struct ModelResolution {
    config: Config,
    catalog: tokio::sync::OnceCell<Option<ModelCatalog>>,
    credential_store: Option<Arc<dyn CredentialStore>>,
    provider_windows: tokio::sync::OnceCell<BTreeMap<String, u64>>,
}

impl ModelResolution {
    pub(crate) fn new(config: &Config, credential_store: Option<Arc<dyn CredentialStore>>) -> Self {
        Self {
            config: config.clone(),
            catalog: tokio::sync::OnceCell::new(),
            credential_store,
            provider_windows: tokio::sync::OnceCell::new(),
        }
    }

    pub(crate) async fn apply(&self, settings: &mut CompactionSettings) {
        let needs_catalog = self
            .config
            .providers
            .values()
            .any(|profile| profile.models.iter().any(|entry| entry.enabled));
        let (catalog, provider_windows) = tokio::join!(
            self.catalog.get_or_init(|| async {
                if needs_catalog {
                    load_catalog(&catalog::default_cache_dir()).await
                } else {
                    None
                }
            }),
            self.provider_windows.get_or_init(|| async {
                match &self.credential_store {
                    Some(store)
                        if self.config.providers.values().any(|profile| {
                            profile.provider_type == config::ProviderTypeConfig::OpenAiCodex
                        }) =>
                    {
                        let version = providers::CodexCatalogVersionResolver::shared()
                            .resolve()
                            .await;
                        load_codex_windows(&self.config, store, &version.version).await
                    }
                    Some(_) | None => BTreeMap::new(),
                }
            })
        );
        apply_model_windows(&self.config, catalog.as_ref(), provider_windows, settings);
    }
}

#[cfg(test)]
mod tests;
