use std::collections::BTreeMap;

use catalog::ModelCatalog;
use config::{ModelEntryConfig, ModelPresetConfig};
use runtime::model_resolve::{
    CatalogMatch, MetadataOrigin, resolve_catalog_match, resolve_model_metadata,
};

/// Prices and limits borrowed from the public API model a Cursor ID is a variant of.
const API_EQUIVALENT_ORIGIN: &str = "models.dev, API-equivalent";
const CURSOR_DOCS_ORIGIN: &str = "Cursor docs";

pub struct MetadataSources<'a> {
    pub presets: &'a BTreeMap<String, ModelPresetConfig>,
    pub catalog: Option<&'a ModelCatalog>,
}

impl MetadataSources<'_> {
    pub fn resolve(
        &self,
        entry: &ModelEntryConfig,
        provider: &str,
        provider_type: Option<config::ProviderTypeConfig>,
    ) -> runtime::model_resolve::ResolvedModelMetadata {
        resolve_model_metadata(
            entry,
            self.presets,
            self.catalog,
            Some(provider),
            provider_type,
        )
    }

    pub fn labels(
        &self,
        entry: &ModelEntryConfig,
        provider: &str,
        provider_type: Option<config::ProviderTypeConfig>,
    ) -> [String; 4] {
        let resolved = self.resolve(entry, provider, provider_type);
        let preset = entry
            .preset
            .as_ref()
            .and_then(|name| self.presets.get(name));
        let preset_origin = format!("Preset: {}", entry.preset.as_deref().unwrap_or_default());
        let context_origin = match resolved.origin {
            MetadataOrigin::Manual => "manual override",
            MetadataOrigin::Preset => &preset_origin,
            MetadataOrigin::Catalog => "models.dev",
            MetadataOrigin::ApiEquivalent => API_EQUIVALENT_ORIGIN,
            MetadataOrigin::ProviderDocs => CURSOR_DOCS_ORIGIN,
            MetadataOrigin::Default => "default",
        };
        let matched = self.catalog.and_then(|catalog| {
            resolve_catalog_match(entry, catalog, Some(provider), provider_type)
        });
        let model = matched.map(|(model, _)| model);
        let docs_pricing =
            runtime::model_resolve::cursor_docs_pricing(entry, self.catalog, provider_type)
                .map(|schedule| schedule.standard.base);
        let catalog_origin = match matched {
            Some((_, CatalogMatch::ApiEquivalent)) => API_EQUIVALENT_ORIGIN,
            _ => "models.dev",
        };
        let context_hint = match resolved.origin {
            MetadataOrigin::Default => " - set preset or metadata_ref",
            MetadataOrigin::Manual
            | MetadataOrigin::Preset
            | MetadataOrigin::Catalog
            | MetadataOrigin::ApiEquivalent
            | MetadataOrigin::ProviderDocs => "",
        };
        let output_origin = if preset.and_then(|p| p.max_output_tokens).is_some() {
            preset_origin.as_str()
        } else if resolved.max_output_tokens.is_some() {
            catalog_origin
        } else {
            "default"
        };
        let price = |preset_value: Option<f64>,
                     docs_value: Option<f64>,
                     catalog_value: Option<f64>,
                     kind| {
            let (value, origin) = match (preset_value, docs_value, catalog_value) {
                (Some(value), _, _) => (Some(value), preset_origin.as_str()),
                (None, Some(value), _) => (Some(value), CURSOR_DOCS_ORIGIN),
                (None, None, Some(value)) => (Some(value), catalog_origin),
                (None, None, None) => (None, "default"),
            };
            match value {
                Some(value) => format!("${value}/M {kind} ({origin})"),
                None => format!("Unknown {kind} price ({origin})"),
            }
        };
        [
            format!(
                "{} ctx ({context_origin}){context_hint}",
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
                docs_pricing.and_then(|prices| prices.input),
                model.and_then(|m| m.input_price),
                "input",
            ),
            price(
                preset.and_then(|p| p.output_price_per_million_usd),
                docs_pricing.and_then(|prices| prices.output),
                model.and_then(|m| m.output_price),
                "output",
            ),
        ]
    }
}
