use std::time::{Duration, Instant};

use config::ModelEntryConfig;
use config::types::provider::ModelPricing;

use super::{TelemetryOverlay, TelemetryRow, TokenUsage};
use crate::model::provider_settings::ProviderSettingsModel;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ModelKey {
    pub provider: String,
    pub profile: Option<String>,
    pub model: String,
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
        let rate = self.usage.cache_hit_rate();
        if rate >= 10.0 {
            segments.push(format!("cache {rate:.1}%"));
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
        self.billed
            .get(run_id)?
            .iter()
            .try_fold(0.0, |total, (key, usage)| {
                let fallback = ModelEntryConfig::enabled(&key.model);
                let provider_type = settings.provider_type(key.profile.as_deref());
                let entry = settings
                    .model_entry(key.profile.as_deref().unwrap_or(&key.provider), &key.model)
                    .unwrap_or(&fallback);
                let catalog = settings
                    .catalog
                    .catalog
                    .as_deref()
                    .and_then(|catalog| {
                        runtime::model_resolve::resolve_catalog_entry(
                            entry,
                            catalog,
                            Some(key.profile.as_deref().unwrap_or(&key.provider)),
                            provider_type,
                        )
                    })
                    .map(|model| ModelPricing {
                        input: model.input_price,
                        output: model.output_price,
                        cache_read: model.cache_read_price,
                        cache_write: model.cache_write_price,
                    });
                let cost = total + usage.estimated_cost(entry.pricing_for(catalog))?;
                cost.is_finite().then_some(cost)
            })
    }
}
