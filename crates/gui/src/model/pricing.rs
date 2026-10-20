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

fn tokens(value: u64) -> f64 {
    Duration::from_secs(value).as_secs_f64()
}

impl TelemetryRow {
    pub fn compact_line_at(&self, now: Instant, pricing: Option<ModelPricing>) -> String {
        self.compact_segments_at(now, self.usage.estimated_cost(pricing))
            .join(" · ")
    }

    pub fn compact_segments_at(&self, now: Instant, cost: Option<f64>) -> Vec<String> {
        let mut segments = Vec::new();
        if let Some(cost) = cost {
            segments.push(format!("${cost:.3}"));
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
        self.costs = self
            .billed
            .keys()
            .filter_map(|run_id| {
                self.estimated_cost(run_id, settings)
                    .map(|cost| (run_id.clone(), cost))
            })
            .collect();
    }

    pub fn cost(&self, run_id: &str) -> Option<f64> {
        self.costs.get(run_id).copied()
    }

    pub fn estimated_cost(&self, run_id: &str, settings: &ProviderSettingsModel) -> Option<f64> {
        let mut schedules: BTreeMap<&ModelKey, Option<PriceSchedule>> = BTreeMap::new();
        self.billed
            .get(run_id)?
            .iter()
            .try_fold(0.0, |total, request| {
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
                let cost = total
                    + request
                        .usage
                        .request_cost(schedule.as_ref(), request.speed)?;
                cost.is_finite().then_some(cost)
            })
    }
}
