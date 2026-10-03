//! Live: the recent pace, today's and this month's projection, and how much
//! work each percent of a Codex subscription window bought.

use egui::{Ui, vec2};
use egui_plot::{Bar, BarChart, Legend, Plot};

use super::charts::{self, SERIES, TOKEN_LAYERS};
use super::{compact_tokens, format_cost};
use crate::model::telemetry::pricing::UsagePricing;
use crate::model::telemetry::quota::QuotaState;
use crate::model::usage_stats::series::{self, Projection, QuotaEfficiency};
use crate::model::usage_stats::{CostCalculator, CostMode, UsageDataset, UsageFilter, UsageTotals};
use crate::theme::text::muted;

/// Codex observations carry this provider label when no profile is known.
const CODEX_PROVIDER: &str = "openai-codex";

/// Recent window choices, in minutes.
pub(super) const WINDOWS: [i64; 3] = [15, 60, 180];

struct QuotaRow {
    subscription: String,
    window: String,
    resets_in: String,
    efficiency: QuotaEfficiency,
}

pub(super) struct LiveData {
    window_minutes: i64,
    recent: UsageTotals,
    minutes: Vec<UsageTotals>,
    projection: Option<Projection>,
    quotas: Vec<QuotaRow>,
}

impl LiveData {
    pub(super) fn compute(
        dataset: &UsageDataset,
        filter: &UsageFilter,
        pricing: &UsagePricing,
        mode: CostMode,
        window_minutes: i64,
        quota: &QuotaState,
    ) -> Self {
        let now = storage::system_time_to_ns(std::time::SystemTime::now()).unwrap_or_default();
        let mut costs = CostCalculator::new(pricing, mode);
        let facts = &dataset.facts;
        let minute = 60_000_000_000_i64;
        let recent = series::totals_between(
            facts,
            filter,
            &mut costs,
            now - window_minutes * minute,
            now + 1,
        );
        let minutes =
            series::minute_totals(facts, filter, &mut costs, now, window_minutes as usize);
        let projection = dataset
            .today
            .zip(dataset.today_bounds_ns)
            .map(|(today, bounds)| {
                series::projection(
                    facts,
                    filter,
                    &mut costs,
                    today,
                    bounds,
                    now,
                    window_minutes,
                )
            });
        let mut quotas = Vec::new();
        let subscriptions: Vec<(&str, &QuotaState)> = if quota.subscriptions.is_empty() {
            vec![(CODEX_PROVIDER, quota)]
        } else {
            quota
                .subscriptions
                .iter()
                .map(|(profile, state)| (profile.as_str(), state))
                .collect()
        };
        for (subscription, state) in subscriptions {
            let Some(snapshot) = &state.snapshot else {
                continue;
            };
            let windows = [&snapshot.quota.primary, &snapshot.quota.secondary];
            for window in windows.into_iter().flatten() {
                let Some(reset_ns) = window.resets_at.timestamp_nanos_opt() else {
                    continue;
                };
                let length = i64::try_from(window.window_duration.as_nanos()).unwrap_or(i64::MAX);
                let profile_filter = UsageFilter {
                    provider: Some(subscription.to_owned()),
                    ..filter.clone()
                };
                let totals = series::totals_between(
                    facts,
                    &profile_filter,
                    &mut costs,
                    reset_ns.saturating_sub(length),
                    now + 1,
                );
                let remaining_minutes = (reset_ns - now).max(0) / minute;
                quotas.push(QuotaRow {
                    subscription: subscription.to_owned(),
                    window: window.duration_label(),
                    resets_in: if remaining_minutes >= 120 {
                        format!("resets in {}h", remaining_minutes / 60)
                    } else {
                        format!("resets in {remaining_minutes}m")
                    },
                    efficiency: QuotaEfficiency {
                        used_percent: window.used_percent,
                        totals,
                    },
                });
            }
        }
        Self {
            window_minutes,
            recent,
            minutes,
            projection,
            quotas,
        }
    }

    pub(super) fn render(&self, ui: &mut Ui, window_minutes: &mut i64) {
        ui.horizontal(|ui| {
            ui.label(muted("Window"));
            for minutes in WINDOWS {
                let label = if minutes >= 60 {
                    format!("{}h", minutes / 60)
                } else {
                    format!("{minutes}m")
                };
                if charts::segment(ui, *window_minutes == minutes, &label) {
                    *window_minutes = minutes;
                }
            }
        });
        self.tiles(ui);
        charts::section(
            ui,
            "Requests per minute",
            "Provider attempts in each minute of the window, oldest on the left",
        );
        self.request_bars(ui);
        charts::section(
            ui,
            "Tokens per minute",
            "Input split by cache use, plus output, per minute",
        );
        self.token_bars(ui);
        charts::section(
            ui,
            "Subscription efficiency",
            "Usage in each Codex quota window divided by the percent it consumed. \
             A full window is 100 times one percent.",
        );
        self.quota_table(ui);
    }

    fn tiles(&self, ui: &mut Ui) {
        let hours = self.window_minutes as f64 / 60.0;
        let recent = &self.recent;
        let mut tiles = vec![
            (
                "Burn rate".to_owned(),
                format!("${:.2}/h", recent.cost / hours),
                format!(
                    "{} tok/min",
                    compact_tokens(
                        (recent.total_tokens() as f64 / self.window_minutes as f64) as u64
                    )
                ),
            ),
            (
                "In window".to_owned(),
                format_cost(recent),
                format!(
                    "{} requests · {} tok",
                    recent.requests,
                    compact_tokens(recent.total_tokens())
                ),
            ),
        ];
        if let Some(projection) = &self.projection {
            tiles.push((
                "Today".to_owned(),
                format!("${:.2}", projection.today),
                format!("≈ ${:.2} by midnight at this pace", projection.end_of_day),
            ));
            tiles.push((
                "This month".to_owned(),
                format!("${:.2}", projection.month),
                format!("≈ ${:.2} by month end", projection.end_of_month),
            ));
        }
        charts::tiles(
            ui,
            170.0,
            tiles
                .into_iter()
                .map(|(title, value, detail)| charts::Tile {
                    title,
                    value,
                    detail,
                    sparkline: None,
                })
                .collect(),
        );
    }

    fn minute_plot(&self, id: &str) -> Plot<'static> {
        let window = self.window_minutes;
        Plot::new(id)
            .height(150.0)
            .allow_zoom(false)
            .allow_drag(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .allow_double_click_reset(false)
            .show_grid([false, true])
            .include_x(-0.5)
            .include_x(window as f64 - 0.5)
            .x_axis_formatter(move |mark, _| {
                let ago = window - mark.value.round() as i64;
                if mark.value.fract().abs() > 1e-6 || ago < 0 {
                    String::new()
                } else if ago == 0 {
                    "now".into()
                } else {
                    format!("-{ago}m")
                }
            })
    }

    fn request_bars(&self, ui: &mut Ui) {
        let bars = self
            .minutes
            .iter()
            .enumerate()
            .map(|(index, totals)| Bar::new(index as f64, totals.requests as f64).width(0.7))
            .collect();
        let chart = BarChart::new("Requests", bars).color(SERIES[0]);
        self.minute_plot("usage-live-requests")
            .include_y(1.0)
            .y_axis_formatter(|mark, _| format!("{:.0}", mark.value))
            .show(ui, |plot| plot.bar_chart(chart));
    }

    fn token_bars(&self, ui: &mut Ui) {
        let window = self.window_minutes;
        let charts_list = charts::stacked_bars(&self.minutes, &TOKEN_LAYERS, move |index| {
            format!("-{}m", window - index)
        });
        self.minute_plot("usage-live-tokens")
            .legend(Legend::default())
            .y_axis_formatter(|mark, _| compact_tokens(mark.value.max(0.0) as u64))
            .show(ui, |plot| {
                for chart in charts_list {
                    plot.bar_chart(chart);
                }
            });
    }

    fn quota_table(&self, ui: &mut Ui) {
        if self.quotas.is_empty() {
            ui.label(muted(
                "No Codex quota is available. Sign in to a Codex subscription to compare it.",
            ));
            return;
        }
        egui::Grid::new("usage-quota-efficiency")
            .striped(true)
            .spacing(vec2(16.0, 4.0))
            .show(ui, |ui| {
                for title in [
                    "Subscription",
                    "Window",
                    "Used",
                    "Tokens",
                    "Cost",
                    "Per 1%",
                    "Full window ≈",
                    "Left ≈",
                ] {
                    ui.label(muted(title));
                }
                ui.end_row();
                for row in &self.quotas {
                    let efficiency = &row.efficiency;
                    ui.label(&row.subscription);
                    ui.label(format!("{} · {}", row.window, row.resets_in));
                    ui.label(format!("{:.0}%", efficiency.used_percent));
                    ui.label(compact_tokens(efficiency.totals.total_tokens()));
                    ui.label(format_cost(&efficiency.totals));
                    if efficiency.is_reliable() {
                        let remaining = (100.0 - efficiency.used_percent).max(0.0);
                        ui.label(format!(
                            "{} tok · ${:.2}",
                            compact_tokens(efficiency.tokens_per_percent() as u64),
                            efficiency.cost_per_percent()
                        ));
                        ui.label(format!(
                            "{} tok · ${:.0}",
                            compact_tokens((efficiency.tokens_per_percent() * 100.0) as u64),
                            efficiency.cost_per_percent() * 100.0
                        ));
                        ui.label(format!(
                            "{} tok",
                            compact_tokens((efficiency.tokens_per_percent() * remaining) as u64)
                        ));
                    } else {
                        ui.label(muted("too early")).on_hover_text(
                            "Under 1% of the window is used, so scaling it would mislead.",
                        );
                        ui.label("—");
                        ui.label("—");
                    }
                    ui.end_row();
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use providers::provider::codex::quota::{CodexQuota, QuotaSnapshot, QuotaSource, QuotaWindow};

    use super::*;
    use crate::model::telemetry::TokenUsage;
    use crate::model::usage_stats::{LocalDay, UsageFact};

    fn codex_fact(provider: &str, at: SystemTime, tokens: u64) -> UsageFact {
        UsageFact {
            day: LocalDay::parse("2026-10-04").unwrap(),
            at_ns: Some(storage::system_time_to_ns(at).unwrap()),
            provider: provider.into(),
            profile: None,
            model: "gpt-codex".into(),
            project_id: None,
            thread_id: None,
            role: None,
            purpose: None,
            requests: 1,
            failed: 0,
            tokens: TokenUsage {
                input: tokens,
                output: 0,
                cache_read: 0,
                cache_write: 0,
            },
            reasoning: 0,
            recorded_cost: 0.5,
            unpriced: TokenUsage::default(),
            ttft_sum_ms: 0,
            ttft_count: 0,
            duration_sum_ms: 0,
            request: None,
        }
    }

    #[test]
    fn quota_window_scales_its_usage_by_the_consumed_percent() {
        // Given: a 5h Codex window that is 25% used and resets in an hour, with
        // one Codex request inside it, one before it and one from another provider.
        let now = SystemTime::now();
        let window = QuotaWindow {
            used_percent: 25.0,
            remaining_percent: 75.0,
            window_duration: Duration::from_secs(5 * 3_600),
            resets_at: (now + Duration::from_secs(3_600)).into(),
        };
        let mut quota = QuotaState::default();
        quota.snapshot = Some(QuotaSnapshot {
            quota: CodexQuota {
                plan: None,
                primary: Some(window),
                secondary: None,
                code_review: None,
            },
            source: QuotaSource::AppServer,
            stale: false,
            fetched_at: now.into(),
            last_error: None,
        });
        let dataset = UsageDataset {
            facts: vec![
                codex_fact(CODEX_PROVIDER, now - Duration::from_secs(5 * 3_600), 9_000),
                codex_fact(CODEX_PROVIDER, now - Duration::from_secs(60), 10_000),
                codex_fact("anthropic", now - Duration::from_secs(60), 7_000),
            ],
            ..UsageDataset::default()
        };

        // When: the Live data is computed.
        let data = LiveData::compute(
            &dataset,
            &UsageFilter::default(),
            &UsagePricing::default(),
            CostMode::Recorded,
            60,
            &quota,
        );

        // Then: only the in-window Codex request counts, scaled by 25%.
        assert_eq!(data.quotas.len(), 1);
        let efficiency = data.quotas[0].efficiency;
        assert!(efficiency.is_reliable());
        assert_eq!(efficiency.totals.total_tokens(), 10_000);
        assert_eq!(efficiency.tokens_per_percent(), 400.0);
        assert_eq!(efficiency.cost_per_percent(), 0.02);
        assert_eq!(data.quotas[0].window, "5h");
    }
}
