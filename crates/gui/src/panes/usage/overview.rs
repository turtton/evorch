//! Overview: stat tiles, daily token and cost bars, activity grid and distribution.

use egui::{Ui, vec2};
use egui_plot::{Bar, BarChart, Legend};
use workspace_ui::SidebarState;

use super::charts::{self, SERIES};
use super::{compact_tokens, format_cost, key_label};
use crate::model::telemetry::pricing::UsagePricing;
use crate::model::usage_stats::series::{self, Share};
use crate::model::usage_stats::{
    CostCalculator, CostMode, LocalDay, UsageDataset, UsageDimension, UsageFilter, UsageRange,
    UsageTotals,
};
use crate::theme::text::{h2, muted};

/// Dimensions offered by the distribution view.
pub(super) const DISTRIBUTION: [UsageDimension; 6] = [
    UsageDimension::Model,
    UsageDimension::Provider,
    UsageDimension::Project,
    UsageDimension::Thread,
    UsageDimension::Role,
    UsageDimension::Purpose,
];

const TILE_WIDTH: f32 = 132.0;

/// Precomputed series, rebuilt only when the data or controls change.
pub(super) struct OverviewData {
    first: LocalDay,
    days: Vec<(LocalDay, UsageTotals)>,
    total: UsageTotals,
    dimension: UsageDimension,
    shares: Vec<Share>,
    order: Vec<String>,
}

impl OverviewData {
    pub(super) fn compute(
        dataset: &UsageDataset,
        range: UsageRange,
        filter: &UsageFilter,
        pricing: &UsagePricing,
        mode: CostMode,
        dimension: UsageDimension,
    ) -> Option<Self> {
        let today = dataset.today?;
        let first = range
            .first_day(today)
            .or_else(|| dataset.facts.first().map(|fact| fact.day))
            .unwrap_or(today);
        let mut costs = CostCalculator::new(pricing, mode);
        let days = series::daily_totals(&dataset.facts, filter, &mut costs, first, today);
        let mut total = UsageTotals::default();
        for (_, totals) in &days {
            total.merge(totals);
        }
        Some(Self {
            first,
            days,
            total,
            dimension,
            shares: series::top_shares(&dataset.facts, filter, dimension, &mut costs, 5),
            order: series::stable_order(&dataset.facts, dimension, &mut costs),
        })
    }

    pub(super) fn render(
        &self,
        ui: &mut Ui,
        sidebar: &SidebarState,
        dimension: &mut UsageDimension,
    ) {
        self.tiles(ui);
        charts::section(
            ui,
            "Daily tokens",
            "Input split by cache use, plus output; one bar per day",
        );
        self.token_bars(ui);
        charts::section(ui, "Daily cost", "Estimated cost per day in USD");
        self.cost_bars(ui);
        charts::section(
            ui,
            "Activity",
            "Total tokens per day; darker cells used less",
        );
        self.activity(ui);
        charts::section(
            ui,
            "Distribution",
            "Share of cost (or tokens when nothing is priced); the top five plus Other",
        );
        ui.horizontal(|ui| {
            ui.label(muted("By"));
            egui::ComboBox::from_id_salt("usage-distribution")
                .selected_text(dimension.label())
                .show_ui(ui, |ui| {
                    for candidate in DISTRIBUTION {
                        ui.selectable_value(dimension, candidate, candidate.label());
                    }
                });
        });
        share_list(ui, &self.shares, &self.order, self.dimension, sidebar);
    }

    fn tiles(&self, ui: &mut Ui) {
        let total = &self.total;
        let daily = |metric: fn(&UsageTotals) -> f64| -> Vec<f64> {
            self.days.iter().map(|(_, totals)| metric(totals)).collect()
        };
        let day_count = self.days.len().max(1) as f64;
        let tiles: [(&str, String, String, Vec<f64>); 6] = [
            (
                "Cost",
                format_cost(total),
                format!("${:.2} / day", total.cost / day_count),
                daily(|totals| totals.cost),
            ),
            (
                "Tokens",
                compact_tokens(total.total_tokens()),
                format!(
                    "in {} · out {}",
                    compact_tokens(total.tokens.input),
                    compact_tokens(total.tokens.output)
                ),
                daily(|totals| totals.total_tokens() as f64),
            ),
            (
                "Requests",
                total.requests.to_string(),
                total
                    .success_rate()
                    .map_or_else(|| "—".into(), |rate| format!("{rate:.1}% succeeded")),
                daily(|totals| totals.requests as f64),
            ),
            (
                "Cache hit",
                total
                    .cache_hit_rate()
                    .map_or_else(|| "—".into(), |rate| format!("{rate:.1}%")),
                format!("{} read", compact_tokens(total.tokens.cache_read)),
                daily(|totals| totals.cache_hit_rate().unwrap_or_default()),
            ),
            (
                "Per 1M tokens",
                total
                    .cost_per_million()
                    .map_or_else(|| "—".into(), |cost| format!("${cost:.2}")),
                "blended cost".into(),
                daily(|totals| totals.cost_per_million().unwrap_or_default()),
            ),
            (
                "Avg TTFT",
                total
                    .average_ttft_ms()
                    .map_or_else(|| "—".into(), |ttft| format!("{ttft} ms")),
                format!("{} reasoning", compact_tokens(total.reasoning)),
                daily(|totals| totals.average_ttft_ms().unwrap_or_default() as f64),
            ),
        ];
        // Frames report their size only after drawing, so wrap by a computed column count.
        let spacing = ui.spacing().item_spacing.x;
        let outer = TILE_WIDTH + 20.0 + spacing;
        let columns = ((ui.available_width() + spacing) / outer).floor().max(1.0) as usize;
        let mut tiles = tiles.into_iter().peekable();
        while tiles.peek().is_some() {
            ui.horizontal(|ui| {
                for (title, value, detail, values) in tiles.by_ref().take(columns) {
                    egui::Frame::new()
                        .fill(crate::theme::tokens::palette().SURFACE_RAISED)
                        .corner_radius(8)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.vertical(|ui| {
                                ui.set_width(TILE_WIDTH);
                                ui.label(muted(title));
                                ui.add(egui::Label::new(h2(value.clone())).truncate())
                                    .on_hover_text(&value);
                                ui.add(egui::Label::new(muted(detail)).truncate());
                                charts::sparkline(ui, &values, SERIES[0], vec2(TILE_WIDTH, 26.0));
                            });
                        });
                }
            });
        }
    }

    fn token_bars(&self, ui: &mut Ui) {
        let charts = charts::stacked_day_bars(&self.days, self.first, &charts::TOKEN_LAYERS);
        charts::day_plot("usage-daily-tokens", self.first, 170.0)
            .legend(Legend::default())
            .y_axis_formatter(|mark, _| compact_tokens(mark.value.max(0.0) as u64))
            .show(ui, |plot| {
                for chart in charts {
                    plot.bar_chart(chart);
                }
            });
    }

    fn cost_bars(&self, ui: &mut Ui) {
        let first = self.first;
        let bars = self
            .days
            .iter()
            .enumerate()
            .map(|(day, (_, totals))| Bar::new(day as f64, totals.cost).width(0.7))
            .collect();
        let chart = BarChart::new("Cost", bars)
            .color(SERIES[0])
            .element_formatter(Box::new(move |bar, _| {
                format!(
                    "{} · ${:.2}",
                    first.add_days(bar.argument as i64),
                    bar.value
                )
            }));
        charts::day_plot("usage-daily-cost", self.first, 170.0)
            .y_axis_formatter(|mark, _| format!("${:.2}", mark.value))
            .show(ui, |plot| plot.bar_chart(chart));
    }

    fn activity(&self, ui: &mut Ui) {
        let (Some((first, _)), Some((last, _))) = (self.days.first(), self.days.last()) else {
            return;
        };
        let start = first.week_start();
        let weeks = (last.days_since(start) / 7 + 1) as usize;
        let mut values = vec![vec![0.0; weeks]; 7];
        let mut labels = vec![vec![String::new(); weeks]; 7];
        for (day, totals) in &self.days {
            let offset = day.days_since(start);
            let (week, weekday) = ((offset / 7) as usize, (offset % 7) as usize);
            values[weekday][week] = totals.total_tokens() as f64;
            labels[weekday][week] = format!(
                "{day}: {} tokens · {}",
                compact_tokens(totals.total_tokens()),
                format_cost(totals)
            );
        }
        let rows: Vec<String> = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
            .into_iter()
            .map(String::from)
            .collect();
        let tooltip = |row: usize, column: usize| {
            let text = &labels[row][column];
            if text.is_empty() {
                "Outside the period".into()
            } else {
                text.clone()
            }
        };
        egui::ScrollArea::horizontal()
            .id_salt("usage-activity")
            .show(ui, |ui| {
                charts::heat_grid(
                    ui,
                    "usage-activity-grid",
                    &rows,
                    &[],
                    &values,
                    &tooltip,
                    vec2(14.0, 14.0),
                );
            });
    }
}

/// A part-to-whole bar plus one row per slice, colored by stable entity order.
pub(super) fn share_list(
    ui: &mut Ui,
    shares: &[Share],
    order: &[String],
    dimension: UsageDimension,
    sidebar: &SidebarState,
) {
    if shares.is_empty() {
        ui.label(muted("No usage for these filters."));
        return;
    }
    let labelled: Vec<_> = shares
        .iter()
        .map(|share| {
            let (label, color) = match &share.key {
                Some(key) => (
                    key_label(dimension, key, sidebar),
                    charts::entity_color(order, key),
                ),
                None => ("Other".to_owned(), charts::other_color()),
            };
            (share, label, color)
        })
        .collect();
    charts::share_bar(
        ui,
        &labelled
            .iter()
            .map(|(share, label, color)| {
                (
                    share.fraction,
                    *color,
                    format!("{label}: {:.1}%", share.fraction * 100.0),
                )
            })
            .collect::<Vec<_>>(),
        14.0,
    );
    egui::Grid::new(("usage-shares", dimension.label()))
        .striped(true)
        .show(ui, |ui| {
            for (share, label, color) in labelled {
                ui.horizontal(|ui| charts::legend_item(ui, color, &label));
                ui.label(format!("{:.1}%", share.fraction * 100.0));
                ui.label(format_cost(&share.totals));
                ui.label(format!(
                    "{} tok",
                    compact_tokens(share.totals.total_tokens())
                ));
                ui.label(
                    share
                        .totals
                        .cache_hit_rate()
                        .map_or_else(|| "cache —".into(), |rate| format!("cache {rate:.0}%")),
                );
                ui.end_row();
            }
        });
}
