//! Analysis: model efficiency, cache trend, project × model, latency and overhead.

use egui::{Ui, vec2};
use egui_plot::{HLine, Legend, Line, Plot, PlotPoint, PlotPoints, Points, Text, VLine};
use workspace_ui::SidebarState;

use super::charts::{self, SERIES};
use super::overview::share_list;
use super::{compact_tokens, format_cost, key_label};
use crate::model::telemetry::pricing::UsagePricing;
use crate::model::usage_stats::series::{self, LatencySample, Matrix, Share};
use crate::model::usage_stats::{
    BreakdownRow, CostCalculator, CostMode, LocalDay, UsageDataset, UsageDimension, UsageFilter,
    UsageRange, UsageTotals, breakdown,
};
use crate::theme::text::muted;
use crate::theme::tokens::palette;

/// Requests drawn in the latency scatter; the most recent are kept.
const LATENCY_POINTS: usize = 2_000;

pub(super) struct AnalysisData {
    first: LocalDay,
    models: Vec<BreakdownRow>,
    days: Vec<(LocalDay, UsageTotals)>,
    matrix: Matrix,
    latency: Vec<LatencySample>,
    purposes: Vec<Share>,
    purpose_order: Vec<String>,
    roles: Vec<Share>,
    role_order: Vec<String>,
}

impl AnalysisData {
    pub(super) fn compute(
        dataset: &UsageDataset,
        range: UsageRange,
        filter: &UsageFilter,
        pricing: &UsagePricing,
        mode: CostMode,
    ) -> Option<Self> {
        let today = dataset.today?;
        let first = range
            .first_day(today)
            .or_else(|| dataset.facts.first().map(|fact| fact.day))
            .unwrap_or(today);
        let facts = &dataset.facts;
        let mut costs = CostCalculator::new(pricing, mode);
        let mut latency = series::latency_samples(facts, filter);
        if latency.len() > LATENCY_POINTS {
            latency.drain(..latency.len() - LATENCY_POINTS);
        }
        Some(Self {
            first,
            models: breakdown(facts, filter, UsageDimension::Model, &mut costs).0,
            days: series::daily_totals(facts, filter, &mut costs, first, today),
            matrix: series::matrix(
                facts,
                filter,
                UsageDimension::Project,
                UsageDimension::Model,
                &mut costs,
                8,
            ),
            latency,
            purposes: series::top_shares(facts, filter, UsageDimension::Purpose, &mut costs, 6),
            purpose_order: series::stable_order(facts, UsageDimension::Purpose, &mut costs),
            roles: series::top_shares(facts, filter, UsageDimension::Role, &mut costs, 6),
            role_order: series::stable_order(facts, UsageDimension::Role, &mut costs),
        })
    }

    pub(super) fn render(&self, ui: &mut Ui, sidebar: &SidebarState) {
        charts::section(
            ui,
            "Model efficiency",
            "Each point is a model: volume against blended cost per million tokens",
        );
        self.efficiency(ui);
        charts::section(
            ui,
            "Cache use",
            "Daily input split by cache use; the hit rate is drawn separately below",
        );
        self.cache(ui);
        charts::section(
            ui,
            "Project × model",
            "Cost of each model per project; brighter cells cost more",
        );
        self.project_models(ui, sidebar);
        charts::section(
            ui,
            "Latency",
            "Time to first token against total duration per streamed request",
        );
        self.latency_view(ui);
        charts::section(
            ui,
            "Overhead by purpose",
            "Agent turns against compaction, titles, routing and escalation reviews",
        );
        share_list(
            ui,
            &self.purposes,
            &self.purpose_order,
            UsageDimension::Purpose,
            sidebar,
        );
        charts::section(
            ui,
            "Cost by role",
            "Agent roles that spent the period's tokens",
        );
        share_list(
            ui,
            &self.roles,
            &self.role_order,
            UsageDimension::Role,
            sidebar,
        );
    }

    fn efficiency(&self, ui: &mut Ui) {
        let points: Vec<_> = self
            .models
            .iter()
            .filter_map(|row| {
                let tokens = row.totals.total_tokens();
                Some((
                    row.key.clone(),
                    (tokens as f64).log10(),
                    row.totals.cost_per_million()?,
                ))
            })
            .collect();
        if points.is_empty() {
            ui.label(muted("No priced models in this period."));
            return;
        }
        Plot::new("usage-efficiency")
            .height(220.0)
            .allow_zoom(false)
            .allow_drag(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .x_axis_label("Total tokens")
            .y_axis_label("$ / 1M tokens")
            .x_axis_formatter(|mark, _| compact_tokens(10_f64.powf(mark.value) as u64))
            .y_axis_formatter(|mark, _| format!("${:.2}", mark.value))
            .label_formatter({
                let names: Vec<String> = points.iter().map(|(name, ..)| name.clone()).collect();
                move |hover| match hover {
                    egui_plot::HoverPosition::NearDataPoint {
                        position, index, ..
                    } => Some(format!(
                        "{}\n{} tokens · ${:.2} / 1M",
                        names.get(*index).map_or("", String::as_str),
                        compact_tokens(10_f64.powf(position.x) as u64),
                        position.y
                    )),
                    egui_plot::HoverPosition::Elsewhere { .. } => None,
                }
            })
            .include_y(0.0)
            .show(ui, |plot| {
                plot.points(
                    Points::new(
                        "Models",
                        PlotPoints::from_iter(points.iter().map(|(_, x, y)| [*x, *y])),
                    )
                    .radius(5.0)
                    .color(SERIES[0]),
                );
                // Label the eight most expensive models; the rest keep tooltips.
                for (name, x, y) in points.iter().take(8) {
                    plot.text(
                        Text::new(name.clone(), PlotPoint::new(*x, *y), format!("  {name}"))
                            .anchor(egui::Align2::LEFT_CENTER)
                            .color(palette().TEXT),
                    );
                }
            });
    }

    fn cache(&self, ui: &mut Ui) {
        let first = self.first;
        let charts = charts::stacked_day_bars(&self.days, first, &charts::TOKEN_LAYERS[..3]);
        charts::day_plot("usage-cache-tokens", first, 160.0)
            .legend(Legend::default())
            .y_axis_formatter(|mark, _| compact_tokens(mark.value.max(0.0) as u64))
            .show(ui, |plot| {
                for chart in charts {
                    plot.bar_chart(chart);
                }
            });
        let rates: Vec<[f64; 2]> = self
            .days
            .iter()
            .enumerate()
            .filter_map(|(day, (_, totals))| Some([day as f64, totals.cache_hit_rate()?]))
            .collect();
        ui.label(muted("Cache hit rate"));
        charts::day_plot("usage-cache-rate", first, 110.0)
            .include_y(0.0)
            .include_y(100.0)
            .y_axis_formatter(|mark, _| format!("{:.0}%", mark.value))
            .show(ui, |plot| {
                plot.line(
                    Line::new("Cache hit", PlotPoints::from(rates))
                        .color(SERIES[0])
                        .width(2.0),
                );
            });
    }

    fn project_models(&self, ui: &mut Ui, sidebar: &SidebarState) {
        let matrix = &self.matrix;
        if matrix.rows.is_empty() || matrix.columns.is_empty() {
            ui.label(muted("No usage for these filters."));
            return;
        }
        let rows: Vec<String> = matrix
            .rows
            .iter()
            .map(|key| key_label(UsageDimension::Project, key, sidebar))
            .collect();
        let values: Vec<Vec<f64>> = matrix
            .cells
            .iter()
            .map(|row| row.iter().map(|totals| totals.cost).collect())
            .collect();
        let tooltip = |row: usize, column: usize| {
            let totals = &matrix.cells[row][column];
            format!(
                "{} · {}\n{} · {} tokens · {} requests",
                rows[row],
                matrix.columns[column],
                format_cost(totals),
                compact_tokens(totals.total_tokens()),
                totals.requests
            )
        };
        egui::ScrollArea::horizontal()
            .id_salt("usage-project-models")
            .show(ui, |ui| {
                charts::heat_grid(
                    ui,
                    "usage-project-model-grid",
                    &rows,
                    &matrix.columns,
                    &values,
                    &tooltip,
                    vec2(64.0, 22.0),
                );
            });
    }

    fn latency_view(&self, ui: &mut Ui) {
        if self.latency.is_empty() {
            ui.label(muted(
                "No streamed requests with a first token in this period.",
            ));
            return;
        }
        let ttft_p50 = series::percentile(self.latency.iter().map(|s| s.ttft_ms), 0.5);
        let ttft_p95 = series::percentile(self.latency.iter().map(|s| s.ttft_ms), 0.95);
        let duration_p95 = series::percentile(self.latency.iter().map(|s| s.duration_ms), 0.95);
        let speed =
            series::percentile(self.latency.iter().filter_map(|s| s.tokens_per_second), 0.5);
        let format_ms =
            |value: Option<f64>| value.map_or_else(|| "—".into(), |value| format!("{value:.0} ms"));
        ui.label(muted(format!(
            "TTFT p50 {} · p95 {} · duration p95 {} · median {} tok/s · {} requests",
            format_ms(ttft_p50),
            format_ms(ttft_p95),
            format_ms(duration_p95),
            speed.map_or_else(|| "—".into(), |speed| format!("{speed:.1}")),
            self.latency.len()
        )));
        Plot::new("usage-latency")
            .height(220.0)
            .allow_zoom(false)
            .allow_drag(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .x_axis_label("Time to first token")
            .y_axis_label("Total duration")
            .x_axis_formatter(|mark, _| format!("{:.0} ms", mark.value))
            .y_axis_formatter(|mark, _| format!("{:.1} s", mark.value / 1_000.0))
            .include_x(0.0)
            .include_y(0.0)
            .show(ui, |plot| {
                plot.points(
                    Points::new(
                        "Requests",
                        PlotPoints::from_iter(
                            self.latency
                                .iter()
                                .map(|sample| [sample.ttft_ms, sample.duration_ms]),
                        ),
                    )
                    .radius(2.5)
                    .color(SERIES[0]),
                );
                let guide = palette().TEXT_MUTED;
                if let Some(p95) = ttft_p95 {
                    plot.vline(VLine::new("TTFT p95", p95).color(guide).width(1.0));
                }
                if let Some(p95) = duration_p95 {
                    plot.hline(HLine::new("Duration p95", p95).color(guide).width(1.0));
                }
            });
    }
}
