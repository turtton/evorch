use std::collections::BTreeMap;

use catalog::ModelCatalog;
use config::{MetadataSource, ModelEntryConfig, ModelPresetConfig};
use runtime::model_resolve::{MetadataOrigin, resolve_model_metadata};

pub struct MetadataSources<'a> {
    pub presets: &'a BTreeMap<String, ModelPresetConfig>,
    pub catalog: Option<&'a ModelCatalog>,
}

impl MetadataSources<'_> {
    pub fn labels(&self, entry: &ModelEntryConfig, provider: &str) -> [String; 4] {
        let resolved = resolve_model_metadata(entry, self.presets, self.catalog, Some(provider));
        let preset = entry
            .preset
            .as_ref()
            .and_then(|name| self.presets.get(name));
        let preset_origin = format!("Preset: {}", entry.preset.as_deref().unwrap_or_default());
        let context_origin = match resolved.origin {
            MetadataOrigin::Manual => "manual override",
            MetadataOrigin::Preset => &preset_origin,
            MetadataOrigin::Catalog => "models.dev",
            MetadataOrigin::Default => "default",
        };
        let model = match entry.metadata_source {
            Some(MetadataSource::Manual | MetadataSource::ProviderDefault) => None,
            None | Some(MetadataSource::ModelsDev) => self.catalog.and_then(|catalog| {
                let reference = entry.metadata_ref.as_deref().unwrap_or(&entry.id);
                let (provider, model) = entry
                    .metadata_ref
                    .as_deref()
                    .and_then(|value| value.split_once('/'))
                    .unwrap_or((provider, reference));
                catalog.find(provider, model)
            }),
        };
        let output_origin = if preset.and_then(|p| p.max_output_tokens).is_some() {
            preset_origin.as_str()
        } else if resolved.max_output_tokens.is_some() {
            "models.dev"
        } else {
            "default"
        };
        let price = |preset_value: Option<f64>, catalog_value: Option<f64>, kind| {
            let (value, origin) = match (preset_value, catalog_value) {
                (Some(value), _) => (Some(value), preset_origin.as_str()),
                (None, Some(value)) => (Some(value), "models.dev"),
                (None, None) => (None, "default"),
            };
            match value {
                Some(value) => format!("${value}/M {kind} ({origin})"),
                None => format!("Unknown {kind} price ({origin})"),
            }
        };
        [
            format!(
                "{} ctx ({context_origin})",
                resolved
                    .context_window
                    .map_or_else(|| "Unknown".into(), |v| v.to_string())
            ),
            format!(
                "{} max output ({output_origin})",
                resolved
                    .max_output_tokens
                    .map_or_else(|| "Unknown".into(), |v| v.to_string())
            ),
            price(
                preset.and_then(|p| p.input_price_per_million_usd),
                model.and_then(|m| m.input_price),
                "input",
            ),
            price(
                preset.and_then(|p| p.output_price_per_million_usd),
                model.and_then(|m| m.output_price),
                "output",
            ),
        ]
    }
}
