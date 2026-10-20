use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use catalog::ModelCatalog;
use config::ModelEntryConfig;
use config::types::provider::{ModelPricing, ModelSpeed, PriceSchedule, PriceSet, PriceTier};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

pub fn speed(service_tier: Option<event_bus::ServiceTier>) -> ModelSpeed {
    match service_tier {
        Some(event_bus::ServiceTier::Priority) => ModelSpeed::Fast,
        None => ModelSpeed::Standard,
    }
}

/// Direct clients and saved Codex observations can lack a profile.
pub fn fallback_provider_type(provider: &str) -> Option<config::ProviderTypeConfig> {
    match provider {
        "openai" => Some(config::ProviderTypeConfig::OpenAi),
        "openai-codex" => Some(config::ProviderTypeConfig::OpenAiCodex),
        _ => None,
    }
}

/// Static TOML prices first, then the catalog entry the profile resolves to.
pub fn resolve_model_pricing(
    entry: Option<&ModelEntryConfig>,
    provider_type: Option<config::ProviderTypeConfig>,
    catalog: Option<&ModelCatalog>,
    profile: &str,
    model: &str,
) -> Option<PriceSchedule> {
    let fallback = ModelEntryConfig::enabled(model);
    let entry = entry.unwrap_or(&fallback);
    let catalog = catalog
        .and_then(|catalog| {
            runtime::model_resolve::resolve_catalog_entry(
                entry,
                catalog,
                Some(profile),
                provider_type,
            )
        })
        .map(catalog_schedule);
    entry.pricing_for(catalog)
}

fn catalog_schedule(model: &catalog::ModelMetadata) -> PriceSchedule {
    PriceSchedule {
        standard: PriceSet {
            base: ModelPricing {
                input: model.input_price,
                output: model.output_price,
                cache_read: model.cache_read_price,
                cache_write: model.cache_write_price,
            },
            tiers: price_tiers(&model.price_tiers),
        },
        fast: model.fast_pricing.as_ref().map(|fast| PriceSet {
            base: prices(fast.prices),
            tiers: price_tiers(&fast.tiers),
        }),
    }
}

fn prices(prices: catalog::Prices) -> ModelPricing {
    ModelPricing {
        input: prices.input,
        output: prices.output,
        cache_read: prices.cache_read,
        cache_write: prices.cache_write,
    }
}

fn price_tiers(tiers: &[catalog::PriceTier]) -> Vec<PriceTier> {
    tiers
        .iter()
        .map(|tier| PriceTier {
            above_input_tokens: tier.above_input_tokens,
            input: tier.prices.input,
            output: tier.prices.output,
            cache_read: tier.prices.cache_read,
            cache_write: tier.prices.cache_write,
        })
        .collect()
}

/// Thread-safe price snapshot shared with the storage bridge, which records
/// each request's cost at the rates in force when it completes.
pub type SharedUsagePricing = Arc<RwLock<UsagePricing>>;

/// Provider profiles and catalog captured from the provider settings.
#[derive(Debug, Clone, Default)]
pub struct UsagePricing {
    entries: BTreeMap<String, config::ProviderProfileConfig>,
    catalog: Option<Arc<ModelCatalog>>,
}

impl UsagePricing {
    pub fn new(
        entries: BTreeMap<String, config::ProviderProfileConfig>,
        catalog: Option<Arc<ModelCatalog>>,
    ) -> Self {
        Self { entries, catalog }
    }

    /// Price schedule for one observed model, or `None` when every price is unknown.
    pub fn pricing(
        &self,
        provider: &str,
        profile: Option<&str>,
        model: &str,
    ) -> Option<PriceSchedule> {
        let name = profile.unwrap_or(provider);
        let profile_config = self.entries.get(name);
        let provider_type = profile_config
            .map(|profile| profile.provider_type)
            .or(fallback_provider_type(provider));
        let entry = profile_config
            .and_then(|profile| profile.models.iter().find(|entry| entry.id == model));
        resolve_model_pricing(entry, provider_type, self.catalog.as_deref(), name, model)
    }

    /// Whether a refresh would change any price, comparing the catalog by identity.
    pub fn same_source(&self, other: &Self) -> bool {
        self.entries == other.entries
            && match (&self.catalog, &other.catalog) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }
}

fn tokens(value: u64) -> f64 {
    Duration::from_secs(value).as_secs_f64()
}

impl TokenUsage {
    pub fn cache_hit_rate(&self) -> f64 {
        let total = tokens(self.input);
        if total == 0.0 {
            0.0
        } else {
            (tokens(self.cache_read) / total * 100.0).min(100.0)
        }
    }

    /// Cost of one request, at the tier its input size and speed select.
    pub fn request_cost(&self, schedule: Option<&PriceSchedule>, speed: ModelSpeed) -> Option<f64> {
        self.estimated_cost(schedule.map(|schedule| schedule.rates_for(self.input, speed)))
    }

    pub fn estimated_cost(&self, pricing: Option<ModelPricing>) -> Option<f64> {
        let pricing = pricing?;
        if self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_write == 0 {
            return Some(0.0);
        }
        let mut cost = 0.0;
        let mut has_known_price = false;
        for (count, price) in [
            (
                self.input
                    .saturating_sub(self.cache_read)
                    .saturating_sub(self.cache_write),
                pricing.input,
            ),
            (self.output, pricing.output),
            (self.cache_read, pricing.cache_read),
            (self.cache_write, pricing.cache_write),
        ] {
            if count > 0
                && let Some(price) = price
            {
                has_known_price = true;
                cost += tokens(count) * price / 1_000_000.0;
            }
        }
        (has_known_price && cost.is_finite()).then_some(cost)
    }
}
