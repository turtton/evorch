//! Costs repriced at report time from one price table for every run (ADR 0036
//! §8). Recorded `cost_usd` values are ignored.

use std::collections::BTreeMap;
use std::sync::Arc;

use catalog::ModelCatalog;
use event_bus::{EventKind, ProviderEvent, RequestPurpose};
use recorder::pricing::{PriceBasis, TokenUsage, UsagePricing, speed};
use serde::{Deserialize, Serialize};
use storage::usage::{UsageRequestRecord, UsageStatus};

/// One provider attempt, as the usage ledger or the judge's event bus saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUsage {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    pub failed: bool,
    /// All input tokens, cached ones included.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// `false` when the provider reported no usage: the token counts above are placeholder
    /// zeros and the request's cost is unknown, not $0.
    #[serde(default = "reported", skip_serializing_if = "is_reported")]
    pub usage_reported: bool,
}

const fn reported() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_reported(value: &bool) -> bool {
    *value
}

impl RequestUsage {
    pub fn from_record(record: &UsageRequestRecord) -> Self {
        Self {
            provider: record.provider.clone(),
            profile: record.profile.clone(),
            model: record.model.clone(),
            run_id: record.run_id.clone(),
            role: record.role.clone(),
            purpose: record.purpose.clone(),
            failed: record.status == UsageStatus::Failed,
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            cache_read_tokens: record.cache_read_tokens,
            cache_write_tokens: record.cache_write_tokens,
            service_tier: record.service_tier.clone(),
            usage_reported: record.usage_reported,
        }
    }

    /// A terminal provider event, or `None` for every other event.
    pub fn from_event(kind: &EventKind) -> Option<Self> {
        match kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                provider,
                profile,
                model,
                run_id,
                purpose,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                service_tier,
                usage_reported,
                ..
            }) => Some(Self {
                provider: provider.clone(),
                profile: profile.clone(),
                model: model.clone(),
                run_id: run_id.clone(),
                role: None,
                purpose: purpose.map(|purpose| purpose.as_str().to_owned()),
                failed: false,
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                cache_read_tokens: *cache_read_tokens,
                cache_write_tokens: *cache_write_tokens,
                service_tier: service_tier.map(|tier| tier.as_str().to_owned()),
                usage_reported: *usage_reported,
            }),
            EventKind::Provider(ProviderEvent::RequestFailed {
                provider,
                profile,
                model,
                run_id,
                purpose,
                ..
            }) => Some(Self {
                provider: provider.clone(),
                profile: profile.clone(),
                model: model.clone(),
                run_id: run_id.clone(),
                role: None,
                purpose: purpose.map(|purpose| purpose.as_str().to_owned()),
                failed: true,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                service_tier: None,
                usage_reported: true,
            }),
            _ => None,
        }
    }

    /// Requests the evaluation itself made, reported apart from the agent's.
    pub fn is_overhead(&self) -> bool {
        matches!(
            self.purpose.as_deref(),
            Some(purpose) if purpose == RequestPurpose::UserSimulation.as_str()
                || purpose == RequestPurpose::Judging.as_str()
        )
    }

    fn tokens(&self) -> TokenUsage {
        TokenUsage {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_read: self.cache_read_tokens,
            cache_write: self.cache_write_tokens,
        }
    }
}

/// Token counts with input split into its uncached and cached parts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub requests: u32,
    pub failed_requests: u32,
    pub input_uncached: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl Tokens {
    pub fn of<'a>(requests: impl IntoIterator<Item = &'a RequestUsage>) -> Self {
        let mut tokens = Self::default();
        for request in requests {
            tokens.add(request);
        }
        tokens
    }

    pub fn add(&mut self, request: &RequestUsage) {
        self.requests += 1;
        self.failed_requests += u32::from(request.failed);
        self.input_uncached += request
            .input_tokens
            .saturating_sub(request.cache_read_tokens)
            .saturating_sub(request.cache_write_tokens);
        self.cache_read += request.cache_read_tokens;
        self.cache_write += request.cache_write_tokens;
        self.output += request.output_tokens;
    }

    pub fn input(&self) -> u64 {
        self.input_uncached + self.cache_read + self.cache_write
    }

    /// Cache reads as a percentage of all input.
    pub fn cache_hit_rate(&self) -> f64 {
        TokenUsage {
            input: self.input(),
            cache_read: self.cache_read,
            ..TokenUsage::default()
        }
        .cache_hit_rate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Unpriced {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub model: String,
    pub requests: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    /// USD of every priced request, API-equivalent amounts included.
    pub usd: f64,
    /// The part of `usd` that prices subscription requests at public API rates.
    pub api_equivalent_usd: f64,
    /// Models whose requests had no price; while any remain the cost is incomplete.
    pub unpriced: Vec<Unpriced>,
    /// Models whose requests reported no usage; their cost is unknown, so while any remain
    /// the cost is incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreported: Vec<Unpriced>,
    /// Replays that left no usage record at all (their database is gone): an
    /// empty record is not a free replay, so while any remain the cost is incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unrecorded: Vec<String>,
}

impl Cost {
    pub fn is_complete(&self) -> bool {
        self.unpriced.is_empty() && self.unreported.is_empty() && self.unrecorded.is_empty()
    }

    pub fn is_api_equivalent(&self) -> bool {
        self.api_equivalent_usd > 0.0
    }

    pub fn add(&mut self, other: &Self) {
        self.usd += other.usd;
        self.api_equivalent_usd += other.api_equivalent_usd;
        for unpriced in &other.unpriced {
            self.note_unpriced(unpriced);
        }
        for unreported in &other.unreported {
            self.note_unreported(unreported);
        }
        for run in &other.unrecorded {
            if !self.unrecorded.contains(run) {
                self.unrecorded.push(run.clone());
            }
        }
    }

    fn note_unpriced(&mut self, unpriced: &Unpriced) {
        Self::note(&mut self.unpriced, unpriced);
    }

    fn note_unreported(&mut self, unreported: &Unpriced) {
        Self::note(&mut self.unreported, unreported);
    }

    fn note(list: &mut Vec<Unpriced>, entry: &Unpriced) {
        match list.iter_mut().find(|known| {
            (&known.provider, &known.profile, &known.model)
                == (&entry.provider, &entry.profile, &entry.model)
        }) {
            Some(known) => known.requests += entry.requests,
            None => {
                list.push(entry.clone());
                list.sort();
            }
        }
    }
}

/// The dearest per-million price among every token kind and every input-size
/// tier of the price set `speed` selects, or `None` when any price a request of
/// any size could be charged at is unknown. The standard price never stands in
/// for a missing fast one: that fallback gives a lower bound, which is no ceiling.
fn schedule_ceiling(
    schedule: &config::types::provider::PriceSchedule,
    speed: config::types::provider::ModelSpeed,
) -> Option<f64> {
    let set = match speed {
        config::types::provider::ModelSpeed::Fast => schedule.fast.as_ref()?,
        config::types::provider::ModelSpeed::Standard => &schedule.standard,
    };
    let thresholds = std::iter::once(0_u64).chain(
        set.tiers
            .iter()
            .map(|tier| tier.above_input_tokens.saturating_add(1)),
    );
    let mut dearest = 0.0_f64;
    for input_tokens in thresholds {
        let rates = set.rates_for(input_tokens);
        for price in [
            rates.input,
            rates.output,
            rates.cache_read,
            rates.cache_write,
        ] {
            let price = price.filter(|price| price.is_finite())?;
            dearest = dearest.max(price);
        }
    }
    Some(dearest)
}

/// The prices of one report: the user's provider settings over models.dev.
pub struct PriceTable {
    pricing: UsagePricing,
    catalog: Option<Arc<ModelCatalog>>,
}

impl PriceTable {
    pub fn new(
        providers: BTreeMap<String, config::ProviderProfileConfig>,
        catalog: Option<Arc<ModelCatalog>>,
    ) -> Self {
        Self {
            pricing: UsagePricing::new(providers, catalog.clone()),
            catalog,
        }
    }

    pub fn catalog(&self) -> Option<&ModelCatalog> {
        self.catalog.as_deref()
    }

    /// What one million tokens of `model` can cost at most, in USD: the dearest
    /// price of any token kind (uncached input, output, cache read, cache write)
    /// under any input-size tier that can apply, at the speed its id names.
    /// `None` when any of those prices is unknown, so an unknown price is never
    /// read as a cheap one.
    pub fn ceiling_per_million(&self, model: &crate::roles::ModelRef) -> Option<f64> {
        let provider = model
            .provider_type
            .map_or_else(|| model.profile.clone(), |kind| kind.as_str().to_owned());
        let (base, speed) = config::types::provider::parse_model_speed(&model.model);
        let (schedule, _) =
            self.pricing
                .comparable_pricing(&provider, Some(&model.profile), base)?;
        schedule_ceiling(&schedule, speed)
    }

    /// Price every request with tokens at its own speed and input-size tier.
    pub fn cost<'a>(&self, requests: impl IntoIterator<Item = &'a RequestUsage>) -> Cost {
        let mut cost = Cost::default();
        for request in requests {
            // Placeholder zeros must not read as a free request.
            if !request.usage_reported {
                cost.note_unreported(&Unpriced {
                    provider: request.provider.clone(),
                    profile: request.profile.clone(),
                    model: request.model.clone(),
                    requests: 1,
                });
                continue;
            }
            let tokens = request.tokens();
            if tokens == TokenUsage::default() {
                continue;
            }
            let priced = self
                .pricing
                .comparable_pricing(
                    &request.provider,
                    request.profile.as_deref(),
                    &request.model,
                )
                .and_then(|(schedule, basis)| {
                    let tier = request
                        .service_tier
                        .as_deref()
                        .and_then(event_bus::ServiceTier::parse);
                    tokens
                        .request_cost(Some(&schedule), speed(tier))
                        .map(|usd| (usd, basis))
                });
            match priced {
                Some((usd, basis)) => {
                    cost.usd += usd;
                    if basis == PriceBasis::ApiEquivalent {
                        cost.api_equivalent_usd += usd;
                    }
                }
                None => cost.note_unpriced(&Unpriced {
                    provider: request.provider.clone(),
                    profile: request.profile.clone(),
                    model: request.model.clone(),
                    requests: 1,
                }),
            }
        }
        cost
    }
}

#[cfg(test)]
mod tests;
