use std::collections::BTreeMap;
use std::time::{Duration, Instant};

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

/// A completed request kept whole, since its tier depends on its own input size.
#[derive(Debug, Clone)]
pub(super) struct BilledRequest {
    pub key: ModelKey,
    pub speed: ModelSpeed,
    pub usage: TokenUsage,
}

/// Hover text for a cost carrying [`partial_marker`].
pub const PARTIAL_COST_HINT: &str = "Some requests have no known price; the cost is a lower bound.";

/// `*` after a cost that leaves out requests without a known price.
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
    pub fn refresh_costs(&mut self, settings: &ProviderSettingsModel) {
        self.refresh_context_windows(settings);
        self.costs.clear();
        self.partially_priced.clear();
        for run_id in self.billed.keys() {
            let (cost, partial) = self.priced_cost(run_id, settings);
            if let Some(cost) = cost {
                self.costs.insert(run_id.clone(), cost);
            }
            if partial {
                self.partially_priced.insert(run_id.clone());
            }
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

    /// Sum of the priced requests, `None` when none has a price, and whether
    /// any request was left out. A sandbox escalation review often runs on a
    /// model without a price and must not hide the run's own cost.
    fn priced_cost(&self, run_id: &str, settings: &ProviderSettingsModel) -> (Option<f64>, bool) {
        let Some(requests) = self.billed.get(run_id) else {
            return (None, false);
        };
        let mut schedules: BTreeMap<&ModelKey, Option<PriceSchedule>> = BTreeMap::new();
        let mut total = None;
        let mut partial = false;
        for request in requests {
            let key = &request.key;
            let schedule = schedules.entry(key).or_insert_with(|| {
                let profile = key.profile.as_deref().unwrap_or(&key.provider);
                resolve_model_pricing(
                    settings.model_entry(profile, &key.model),
                    key.provider_type(settings),
                    settings.catalog.catalog.as_deref(),
                    profile,
                    &key.model,
                )
            });
            match request
                .usage
                .request_cost(schedule.as_ref(), request.speed)
                .filter(|cost| cost.is_finite())
            {
                Some(cost) => total = Some(total.unwrap_or(0.0) + cost),
                None => partial = true,
            }
        }
        (total.filter(|cost: &f64| cost.is_finite()), partial)
    }
}
