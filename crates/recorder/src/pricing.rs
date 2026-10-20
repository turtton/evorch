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

/// Where a comparable price came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceBasis {
    /// The provider's own prices.
    Billed,
    /// A subscription priced as the public API model it serves.
    ApiEquivalent,
}

/// models.dev providers whose public API prices a subscription's models, or
/// `None` for providers that bill by usage.
pub fn api_equivalent_providers(
    provider_type: config::ProviderTypeConfig,
) -> Option<&'static [&'static str]> {
    match provider_type {
        config::ProviderTypeConfig::AnthropicSubscription => Some(&["anthropic"]),
        config::ProviderTypeConfig::OpenAiCodex => Some(&["openai"]),
        config::ProviderTypeConfig::KimiSubscription => Some(&["moonshotai", "moonshotai-cn"]),
        _ => None,
    }
}

/// The API model a subscription entry serves: an explicit reference to one of
/// `providers`, the same id there, or the canonical id of the plan's entry.
fn api_catalog_entry<'a>(
    entry: &ModelEntryConfig,
    provider_type: config::ProviderTypeConfig,
    providers: &[&str],
    catalog: &'a ModelCatalog,
    profile: &str,
) -> Option<&'a catalog::ModelMetadata> {
    let in_api = |model: &str| {
        providers
            .iter()
            .find_map(|provider| catalog.find(provider, model))
    };
    let referenced = entry
        .metadata_ref
        .as_deref()
        .and_then(|reference| reference.split_once('/'))
        .filter(|(provider, _)| providers.contains(provider))
        .and_then(|(provider, model)| catalog.find(provider, model));
    let (model_id, _) = config::types::provider::parse_model_speed(&entry.id);
    referenced.or_else(|| in_api(model_id)).or_else(|| {
        let plan = runtime::model_resolve::resolve_catalog_entry(
            entry,
            catalog,
            Some(profile),
            Some(provider_type),
        )?;
        let (provider, model) = plan.canonical_model_id.as_deref()?.split_once('/')?;
        catalog.find(provider, model).or_else(|| in_api(model))
    })
}

fn is_free(schedule: &PriceSchedule) -> bool {
    let set_is_free = |set: &PriceSet| {
        std::iter::once(set.base)
            .chain(set.tiers.iter().map(PriceTier::pricing))
            .flat_map(|prices| {
                [
                    prices.input,
                    prices.output,
                    prices.cache_read,
                    prices.cache_write,
                ]
            })
            .flatten()
            .all(|price| price == 0.0)
    };
    set_is_free(&schedule.standard) && schedule.fast.as_ref().is_none_or(set_is_free)
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

    /// Like [`Self::pricing`], except that a subscription is priced as the
    /// public API model it serves. A plan whose every known price is zero is
    /// unpriced rather than free.
    pub fn comparable_pricing(
        &self,
        provider: &str,
        profile: Option<&str>,
        model: &str,
    ) -> Option<(PriceSchedule, PriceBasis)> {
        let name = profile.unwrap_or(provider);
        let profile_config = self.entries.get(name);
        let provider_type = profile_config
            .map(|profile| profile.provider_type)
            .or(fallback_provider_type(provider));
        let Some((provider_type, providers)) = provider_type.and_then(|provider_type| {
            api_equivalent_providers(provider_type).map(|providers| (provider_type, providers))
        }) else {
            return self
                .pricing(provider, profile, model)
                .map(|schedule| (schedule, PriceBasis::Billed));
        };
        let fallback = ModelEntryConfig::enabled(model);
        let entry = profile_config
            .and_then(|profile| profile.models.iter().find(|entry| entry.id == model))
            .unwrap_or(&fallback);
        let catalog = self
            .catalog
            .as_deref()
            .and_then(|catalog| api_catalog_entry(entry, provider_type, providers, catalog, name))
            .map(catalog_schedule);
        entry
            .pricing_for(catalog)
            .filter(|schedule| !is_free(schedule))
            .map(|schedule| (schedule, PriceBasis::ApiEquivalent))
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

#[cfg(test)]
mod tests;
