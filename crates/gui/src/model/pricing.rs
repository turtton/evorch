use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use catalog::ModelCatalog;
use config::types::provider::{ModelPricing, ModelSpeed, PriceSchedule};
use recorder::pricing::{TokenUsage, fallback_provider_type, resolve_model_pricing};

use super::{TelemetryOverlay, TelemetryRow};
use crate::model::provider_settings::ProviderSettingsModel;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ModelKey {
    pub provider: String,
    pub profile: Option<String>,
    pub model: String,
}

impl ModelKey {
    pub(super) fn provider_type(
        &self,
        settings: &ProviderSettingsModel,
    ) -> Option<config::ProviderTypeConfig> {
        settings
            .provider_type(Some(self.profile.as_deref().unwrap_or(&self.provider)))
            .or(fallback_provider_type(&self.provider))
    }
}

/// Everything that decides a model's price and context window. Resolving them
/// searches the whole catalog, so results are kept until any of these change.
#[derive(Debug)]
pub(super) struct PricingSource {
    entries: BTreeMap<String, config::ProviderProfileConfig>,
    provider_types: Vec<(String, config::ProviderTypeConfig)>,
    presets: BTreeMap<String, config::ModelPresetConfig>,
    /// Held so that a replaced catalog cannot reuse this address.
    catalog: Option<Arc<ModelCatalog>>,
}

impl PricingSource {
    pub(super) fn capture(settings: &ProviderSettingsModel) -> Self {
        Self {
            entries: settings.profile_entries().clone(),
            provider_types: settings
                .profiles
                .iter()
                .map(|profile| (profile.name.clone(), profile.provider_type))
                .collect(),
            presets: settings.model_presets.clone(),
            catalog: settings.catalog.catalog.clone(),
        }
    }

    /// Compares without allocating, so it can run every frame.
    pub(super) fn matches(&self, settings: &ProviderSettingsModel) -> bool {
        self.entries == *settings.profile_entries()
            && self
                .provider_types
                .iter()
                .map(|(name, kind)| (name.as_str(), *kind))
                .eq(settings
                    .profiles
                    .iter()
                    .map(|profile| (profile.name.as_str(), profile.provider_type)))
            && self.presets == settings.model_presets
            && match (&self.catalog, &settings.catalog.catalog) {
                (Some(kept), Some(current)) => Arc::ptr_eq(kept, current),
                (None, None) => true,
                _ => false,
            }
    }
}

fn resolve_schedule(key: &ModelKey, settings: &ProviderSettingsModel) -> Option<PriceSchedule> {
    let profile = key.profile.as_deref().unwrap_or(&key.provider);
    resolve_model_pricing(
        settings.model_entry(profile, &key.model),
        key.provider_type(settings),
        settings.catalog.catalog.as_deref(),
        profile,
        &key.model,
    )
}

/// Sum of the priced requests, `None` when none has a price, and whether
/// any request was left out. A sandbox escalation review often runs on a
/// model without a price and must not hide the run's own cost.
fn priced_total<'a>(
    requests: &[BilledRequest],
    schedule: impl Fn(&ModelKey) -> Option<&'a PriceSchedule>,
) -> (Option<f64>, bool) {
    let mut total = None;
    let mut partial = false;
    for request in requests {
        // Placeholder zeros are not a price of $0: the request's cost is unknown.
        if !request.usage_reported {
            partial = true;
            continue;
        }
        match request
            .usage
            .request_cost(schedule(&request.key), request.speed)
            .filter(|cost| cost.is_finite())
        {
            Some(cost) => total = Some(total.unwrap_or(0.0) + cost),
            None => partial = true,
        }
    }
    (total.filter(|cost: &f64| cost.is_finite()), partial)
}

/// A completed request kept whole, since its tier depends on its own input size.
#[derive(Debug, Clone)]
pub(super) struct BilledRequest {
    pub key: ModelKey,
    pub speed: ModelSpeed,
    pub usage: TokenUsage,
    /// `false` when the provider reported no usage, so `usage` is placeholder zeros.
    pub usage_reported: bool,
}

/// Hover text for a cost carrying [`partial_marker`].
pub const PARTIAL_COST_HINT: &str =
    "Some requests have no known price or reported no usage; the cost is a lower bound.";

/// `*` after a cost that leaves out requests without a known price or reported usage.
pub fn partial_marker(partial: bool) -> &'static str {
    if partial { "*" } else { "" }
}

fn tokens(value: u64) -> f64 {
    Duration::from_secs(value).as_secs_f64()
}

impl TelemetryRow {
    pub fn compact_line_at(&self, now: Instant, pricing: Option<ModelPricing>) -> String {
        self.compact_segments_at(now, self.usage.estimated_cost(pricing), false)
            .join(" · ")
    }

    /// `partial` marks a cost that leaves out requests without a known price.
    pub fn compact_segments_at(
        &self,
        now: Instant,
        cost: Option<f64>,
        partial: bool,
    ) -> Vec<String> {
        let mut segments = Vec::new();
        if let Some(cost) = cost {
            segments.push(format!("${cost:.3}{}", partial_marker(partial)));
        }
        let total = tokens(self.usage.input) + tokens(self.usage.output);
        segments.push(if total >= 1_000_000.0 {
            format!("{:.1}M tok", total / 1_000_000.0)
        } else if total >= 1_000.0 {
            format!("{:.1}K tok", total / 1_000.0)
        } else {
            format!("{total:.0} tok")
        });
        if let Some(rate) = self.tok_s_at(now) {
            let prefix = if self.request_duration.is_none() {
                "≈ "
            } else {
                ""
            };
            segments.push(format!("{prefix}{rate:.1} tok/s"));
        }
        segments
    }
}

impl TelemetryOverlay {
    /// Runs every frame: only runs with new requests are summed again, and
    /// prices are resolved again only after the settings or catalog change.
    pub fn refresh_costs(&mut self, settings: &ProviderSettingsModel) {
        if !self
            .pricing_source
            .as_ref()
            .is_some_and(|source| source.matches(settings))
        {
            self.pricing_source = Some(PricingSource::capture(settings));
            self.schedules.clear();
            self.context_windows.clear();
            self.stale_costs.extend(self.billed.keys().cloned());
            self.stale_context.extend(self.rows.keys().cloned());
        }
        self.refresh_context_windows(settings);
        for run_id in std::mem::take(&mut self.stale_costs) {
            self.refresh_run_cost(run_id, settings);
        }
    }

    fn refresh_run_cost(&mut self, run_id: String, settings: &ProviderSettingsModel) {
        let Some(requests) = self.billed.get(&run_id) else {
            return;
        };
        for request in requests {
            if !self.schedules.contains_key(&request.key) {
                let schedule = resolve_schedule(&request.key, settings);
                self.schedules.insert(request.key.clone(), schedule);
            }
        }
        let schedules = &self.schedules;
        let (cost, partial) =
            priced_total(requests, |key| schedules.get(key).and_then(Option::as_ref));
        match cost {
            Some(cost) => self.costs.insert(run_id.clone(), cost),
            None => self.costs.remove(&run_id),
        };
        if partial {
            self.partially_priced.insert(run_id);
        } else {
            self.partially_priced.remove(&run_id);
        }
    }

    /// Cost of the run's requests with a known price.
    pub fn cost(&self, run_id: &str) -> Option<f64> {
        self.costs.get(run_id).copied()
    }

    /// Whether [`Self::cost`] leaves out requests without a known price.
    pub fn cost_is_partial(&self, run_id: &str) -> bool {
        self.partially_priced.contains(run_id)
    }

    pub fn estimated_cost(&self, run_id: &str, settings: &ProviderSettingsModel) -> Option<f64> {
        self.priced_cost(run_id, settings).0
    }

    /// Resolves the prices afresh instead of using the cache of [`Self::refresh_costs`].
    fn priced_cost(&self, run_id: &str, settings: &ProviderSettingsModel) -> (Option<f64>, bool) {
        let Some(requests) = self.billed.get(run_id) else {
            return (None, false);
        };
        let mut schedules: BTreeMap<&ModelKey, Option<PriceSchedule>> = BTreeMap::new();
        for request in requests {
            schedules
                .entry(&request.key)
                .or_insert_with(|| resolve_schedule(&request.key, settings));
        }
        priced_total(requests, |key| schedules.get(key).and_then(Option::as_ref))
    }
}
