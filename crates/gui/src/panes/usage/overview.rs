//! Overview: stat tiles, daily token and cost bars, activity grid and distribution.

use egui::Ui;
use workspace_ui::SidebarState;

use super::charts;
use super::{cells, key_label};
use crate::model::usage_stats::series::{self, Share};
use crate::model::usage_stats::{
    CostCalculator, CostMode, LocalDay, UsageDataset, UsageDimension, UsageFilter, UsageRange,
    UsageTotals,
};
use crate::panes::usage_widgets::{
    self, ShareRow, TILE_WIDTH, UsageCells, activity_grid, daily_cost_chart, daily_token_chart,
    overview_tiles,
};
use crate::theme::text::muted;
use recorder::pricing::UsagePricing;

/// Dimensions offered by the distribution view.
pub(super) const DISTRIBUTION: [UsageDimension; 6] = [
    UsageDimension::Model,
    UsageDimension::Provider,
    UsageDimension::Project,
    UsageDimension::Thread,
    UsageDimension::Role,
    UsageDimension::Purpose,
];

/// Precomputed series, rebuilt only when the data or controls change.
pub(super) struct OverviewData {
    first: LocalDay,
    days: Vec<(LocalDay, UsageCells)>,
    total: UsageCells,
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
            days: days
                .iter()
                .map(|(day, totals)| (*day, cells(totals)))
                .collect(),
            total: cells(&total),
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
        charts::tiles(ui, TILE_WIDTH, overview_tiles(&self.total, &self.days));
        charts::section(
            ui,
            "Daily tokens",
            "Input split by cache use, plus output; one bar per day",
        );
        daily_token_chart(ui, "usage-daily-tokens", self.first, &self.days);
        charts::section(ui, "Daily cost", "Estimated cost per day in USD");
        daily_cost_chart(ui, "usage-daily-cost", self.first, &self.days);
        charts::section(
            ui,
            "Activity",
            "Total tokens per day; darker cells used less",
        );
        activity_grid(ui, "usage-activity", &self.days);
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
}

/// A part-to-whole bar plus one row per slice, colored by stable entity order.
pub(super) fn share_list(
    ui: &mut Ui,
    shares: &[Share],
    order: &[String],
    dimension: UsageDimension,
    sidebar: &SidebarState,
) {
    let rows: Vec<ShareRow> = shares
        .iter()
        .map(|share| {
            let (label, color) = match &share.key {
                Some(key) => (
                    key_label(dimension, key, sidebar),
                    charts::entity_color(order, key),
                ),
                None => ("Other".to_owned(), charts::other_color()),
            };
            ShareRow {
                label,
                color,
                fraction: share.fraction,
                cells: cells(&share.totals),
            }
        })
        .collect();
    usage_widgets::share_list(ui, ("usage-shares", dimension.label()), &rows);
}
