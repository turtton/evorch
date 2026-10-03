//! Token usage statistics tab, opened from the footer.
//!
//! A shared toolbar selects the period, cost mode and filters; the view below
//! groups the usage ledger. Loads run on a worker thread so the render thread
//! never waits on SQLite.

use std::time::{Duration, Instant};

use egui_extras::{Column, TableBuilder};
use workspace_ui::SidebarState;

use crate::model::telemetry::pricing::UsagePricing;
use crate::model::usage_stats::{
    BreakdownRow, CostCalculator, CostMode, UsageDataset, UsageDimension, UsageFilter, UsageLoader,
    UsageRange, UsageTotals, breakdown, filter_options,
};
use crate::theme::text::{h3, muted};

#[path = "usage/analysis.rs"]
mod analysis;
#[path = "usage/charts.rs"]
mod charts;
#[path = "usage/live.rs"]
mod live;
#[path = "usage/overview.rs"]
mod overview;
#[path = "usage/requests.rs"]
mod requests;

/// The views offered below the shared toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum UsageView {
    #[default]
    Overview,
    Breakdown,
    Analysis,
    Live,
    Requests,
}

impl UsageView {
    const ALL: [Self; 5] = [
        Self::Overview,
        Self::Breakdown,
        Self::Analysis,
        Self::Live,
        Self::Requests,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Breakdown => "Breakdown",
            Self::Analysis => "Analysis",
            Self::Live => "Live",
            Self::Requests => "Request log",
        }
    }
}

/// Reload the open tab this often so new requests appear without a click.
const AUTO_REFRESH: Duration = Duration::from_secs(30);
/// The Live view reloads its own month-to-date data this often.
const LIVE_REFRESH: Duration = Duration::from_secs(15);
/// Refresh today's footer total this often.
const FOOTER_REFRESH: Duration = Duration::from_secs(60);
const PRICING_REFRESH: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Requests,
    Input,
    CacheWrite,
    CacheRead,
    Output,
    Total,
    CacheHit,
    Cost,
}

impl SortColumn {
    fn format(self, totals: &UsageTotals) -> String {
        match self {
            Self::Requests => totals.requests.to_string(),
            Self::Input => compact_tokens(totals.uncached_input()),
            Self::CacheWrite => compact_tokens(totals.tokens.cache_write),
            Self::CacheRead => compact_tokens(totals.tokens.cache_read),
            Self::Output => compact_tokens(totals.tokens.output),
            Self::Total => compact_tokens(totals.total_tokens()),
            Self::CacheHit => totals
                .cache_hit_rate()
                .map_or_else(|| "—".into(), |rate| format!("{rate:.1}%")),
            Self::Cost => format_cost(totals),
        }
    }

    fn value(self, totals: &UsageTotals) -> f64 {
        match self {
            Self::Requests => totals.requests as f64,
            Self::Input => totals.uncached_input() as f64,
            Self::CacheWrite => totals.tokens.cache_write as f64,
            Self::CacheRead => totals.tokens.cache_read as f64,
            Self::Output => totals.tokens.output as f64,
            Self::Total => totals.total_tokens() as f64,
            Self::CacheHit => totals.cache_hit_rate().unwrap_or_default(),
            Self::Cost => totals.cost,
        }
    }
}

// Cost and totals first: they stay visible when the tab is narrow.
const COLUMNS: [(&str, SortColumn, &str); 8] = [
    ("Cost", SortColumn::Cost, "Estimated cost in USD"),
    ("Total", SortColumn::Total, "All input and output tokens"),
    (
        "Cache hit",
        SortColumn::CacheHit,
        "Cache read share of all input tokens",
    ),
    (
        "Requests",
        SortColumn::Requests,
        "Provider attempts, including failures",
    ),
    (
        "Input",
        SortColumn::Input,
        "Input tokens not served from or written to cache",
    ),
    (
        "Cache write",
        SortColumn::CacheWrite,
        "Input tokens written to the prompt cache",
    ),
    (
        "Cache read",
        SortColumn::CacheRead,
        "Input tokens served from the prompt cache",
    ),
    (
        "Output",
        SortColumn::Output,
        "Output tokens, including reasoning",
    ),
];

#[derive(Debug, Clone, PartialEq)]
struct ViewKey {
    generation: u64,
    filter: UsageFilter,
    dimension: UsageDimension,
    cost_mode: CostMode,
}

#[derive(Default)]
struct TodaySummary {
    loader: UsageLoader,
    totals: Option<UsageTotals>,
    dataset: Option<UsageDataset>,
    next_refresh: Option<Instant>,
}

/// State of the Usage tab and of the footer's daily total.
#[derive(Default)]
pub struct UsagePane {
    range: UsageRange,
    cost_mode: CostMode,
    dimension: UsageDimension,
    filter: UsageFilter,
    show_models: bool,
    sort: Option<(SortColumn, bool)>,
    dataset: Option<(UsageRange, UsageDataset)>,
    loaded_at: Option<Instant>,
    loader: UsageLoader,
    error: Option<String>,
    generation: u64,
    pricing: UsagePricing,
    next_pricing_refresh: Option<Instant>,
    view: Option<(ViewKey, Vec<BreakdownRow>, UsageTotals)>,
    today: TodaySummary,
    active: UsageView,
    distribution: UsageDimension,
    overview: Option<(ViewKey, Option<overview::OverviewData>)>,
    analysis: Option<(ViewKey, Option<analysis::AnalysisData>)>,
    live: LiveState,
    request_controls: requests::RequestControls,
    requests: Option<(ViewKey, requests::RequestControls, requests::RequestsData)>,
    open_conversation: bool,
}

/// Month-to-date data for the Live view, loaded separately from the period.
struct LiveState {
    loader: UsageLoader,
    dataset: Option<UsageDataset>,
    generation: u64,
    loaded_at: Option<Instant>,
    window_minutes: i64,
    view: Option<((u64, ViewKey, i64), live::LiveData)>,
}

impl Default for LiveState {
    fn default() -> Self {
        Self {
            loader: UsageLoader::default(),
            dataset: None,
            generation: 0,
            loaded_at: None,
            window_minutes: 60,
            view: None,
        }
    }
}

impl UsagePane {
    /// Per-frame bookkeeping: collects finished loads, refreshes prices and
    /// keeps the footer's daily total current.
    pub fn update(
        &mut self,
        ctx: &egui::Context,
        now: Instant,
        config: Option<&storage::StorageConfig>,
        pricing: impl FnOnce() -> UsagePricing,
    ) {
        if self.next_pricing_refresh.is_none_or(|next| now >= next) {
            self.next_pricing_refresh = Some(now + PRICING_REFRESH);
            let fresh = pricing();
            if !self.pricing.same_source(&fresh) {
                self.pricing = fresh;
                self.generation += 1;
                self.today.totals = self
                    .today
                    .dataset
                    .as_ref()
                    .map(|data| self.today_totals(data));
            }
        }
        if let Some((range, result)) = self.loader.poll() {
            match result {
                Ok(dataset) => {
                    self.dataset = Some((range, dataset));
                    self.error = None;
                    self.generation += 1;
                }
                Err(error) => self.error = Some(error),
            }
        }
        if let Some((_, result)) = self.live.loader.poll() {
            match result {
                Ok(dataset) => {
                    self.live.dataset = Some(dataset);
                    self.live.generation += 1;
                }
                Err(error) => self.error = Some(error),
            }
        }
        if let Some((_, result)) = self.today.loader.poll() {
            match result {
                Ok(dataset) => {
                    self.today.totals = Some(self.today_totals(&dataset));
                    self.today.dataset = Some(dataset);
                }
                Err(error) => tracing::debug!(%error, "usage footer refresh failed"),
            }
        }
        if let Some(config) = config
            && !self.today.loader.is_loading()
            && self.today.next_refresh.is_none_or(|next| now >= next)
        {
            self.today.next_refresh = Some(now + FOOTER_REFRESH);
            self.today
                .loader
                .start(config.clone(), UsageRange::Today, Some(ctx.clone()));
        }
    }

    fn today_totals(&self, dataset: &UsageDataset) -> UsageTotals {
        breakdown(
            &dataset.facts,
            &UsageFilter::default(),
            UsageDimension::Day,
            &mut CostCalculator::new(&self.pricing, CostMode::Recorded),
        )
        .1
    }

    /// Today's recorded cost, once the first footer refresh finishes.
    pub fn today_totals_snapshot(&self) -> Option<UsageTotals> {
        self.today.totals
    }

    /// Force the next frame to reload both the tab and the footer total.
    pub fn invalidate(&mut self) {
        self.loaded_at = None;
        self.live.loaded_at = None;
        self.today.next_refresh = None;
    }

    /// Whether a request row asked to show its conversation; clears the request.
    pub fn take_conversation_request(&mut self) -> bool {
        std::mem::take(&mut self.open_conversation)
    }

    /// Renders the tab. Returns a thread a request row asked to open.
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        config: Option<&storage::StorageConfig>,
        sidebar: &SidebarState,
        quota: &crate::model::telemetry::quota::QuotaState,
    ) -> Option<String> {
        let Some(config) = config else {
            ui.label("Usage statistics need the events database.");
            return None;
        };
        let refresh = self.toolbar(ui);
        let stale = self
            .dataset
            .as_ref()
            .is_none_or(|(range, _)| *range != self.range)
            || self
                .loaded_at
                .is_none_or(|loaded| loaded.elapsed() >= AUTO_REFRESH);
        if (refresh || stale) && !self.loader.is_loading() {
            self.loaded_at = Some(Instant::now());
            self.loader
                .start(config.clone(), self.range, Some(ui.ctx().clone()));
        }
        if let Some(error) = &self.error {
            ui.colored_label(crate::theme::tokens::palette().ERROR_FG, error);
        }
        ui.horizontal(|ui| {
            for view in UsageView::ALL {
                if charts::segment(ui, self.active == view, view.label()) {
                    self.active = view;
                }
            }
        });
        if self.active == UsageView::Live {
            self.filters(ui, sidebar);
            ui.separator();
            self.live_view(ui, config, quota);
            return None;
        }
        let Some((_, dataset)) = &self.dataset else {
            ui.label(muted("Loading usage…"));
            return None;
        };
        if dataset.facts.is_empty() {
            ui.label(muted("No usage recorded for this period."));
            return None;
        }
        self.filters(ui, sidebar);
        ui.separator();
        let mut open = None;
        match self.active {
            UsageView::Overview => {
                self.refresh_overview();
                let distribution = &mut self.distribution;
                if let Some((_, Some(data))) = &self.overview {
                    egui::ScrollArea::vertical()
                        .id_salt("usage-overview-scroll")
                        .show(ui, |ui| data.render(ui, sidebar, distribution));
                }
            }
            UsageView::Breakdown => {
                egui::ScrollArea::horizontal()
                    .id_salt("usage-breakdown-scroll")
                    .show(ui, |ui| self.breakdown_view(ui, sidebar));
            }
            UsageView::Analysis => {
                self.refresh_analysis();
                if let Some((_, Some(data))) = &self.analysis {
                    egui::ScrollArea::vertical()
                        .id_salt("usage-analysis-scroll")
                        .show(ui, |ui| data.render(ui, sidebar));
                }
            }
            UsageView::Requests => {
                self.refresh_requests();
                if let Some((_, _, data)) = &self.requests {
                    open = data.render(ui, sidebar, &mut self.request_controls);
                }
            }
            UsageView::Live => {}
        }
        self.open_conversation |= open.is_some();
        open
    }

    fn refresh_requests(&mut self) {
        let key = self.view_key(UsageDimension::Day);
        if self.requests.as_ref().is_some_and(|(cached, controls, _)| {
            *cached == key && *controls == self.request_controls
        }) {
            return;
        }
        let Some((_, dataset)) = &self.dataset else {
            return;
        };
        let data = requests::RequestsData::compute(
            dataset,
            &self.filter,
            &self.pricing,
            self.cost_mode,
            &self.request_controls,
        );
        self.requests = Some((key, self.request_controls.clone(), data));
    }

    fn live_view(
        &mut self,
        ui: &mut egui::Ui,
        config: &storage::StorageConfig,
        quota: &crate::model::telemetry::quota::QuotaState,
    ) {
        let live = &mut self.live;
        if live
            .loaded_at
            .is_none_or(|loaded| loaded.elapsed() >= LIVE_REFRESH)
            && !live.loader.is_loading()
        {
            live.loaded_at = Some(Instant::now());
            live.loader
                .start(config.clone(), UsageRange::Live, Some(ui.ctx().clone()));
        }
        // Repaint after the next refresh even without input, so the pace stays current.
        ui.ctx().request_repaint_after(LIVE_REFRESH);
        let Some(dataset) = &self.live.dataset else {
            ui.label(muted("Loading usage…"));
            return;
        };
        let key = (
            self.live.generation,
            self.view_key(UsageDimension::Day),
            self.live.window_minutes,
        );
        if self
            .live
            .view
            .as_ref()
            .is_none_or(|(cached, _)| *cached != key)
        {
            let data = live::LiveData::compute(
                dataset,
                &self.filter,
                &self.pricing,
                self.cost_mode,
                self.live.window_minutes,
                quota,
            );
            self.live.view = Some((key, data));
        }
        let mut window = self.live.window_minutes;
        if let Some((_, data)) = &self.live.view {
            egui::ScrollArea::vertical()
                .id_salt("usage-live-scroll")
                .show(ui, |ui| data.render(ui, &mut window));
        }
        self.live.window_minutes = window;
    }

    fn view_key(&self, dimension: UsageDimension) -> ViewKey {
        ViewKey {
            generation: self.generation,
            filter: self.filter.clone(),
            dimension,
            cost_mode: self.cost_mode,
        }
    }

    fn refresh_overview(&mut self) {
        let key = self.view_key(self.distribution);
        if self
            .overview
            .as_ref()
            .is_some_and(|(cached, _)| *cached == key)
        {
            return;
        }
        let data = self.dataset.as_ref().and_then(|(range, dataset)| {
            overview::OverviewData::compute(
                dataset,
                *range,
                &self.filter,
                &self.pricing,
                self.cost_mode,
                self.distribution,
            )
        });
        self.overview = Some((key, data));
    }

    fn refresh_analysis(&mut self) {
        let key = self.view_key(UsageDimension::Day);
        if self
            .analysis
            .as_ref()
            .is_some_and(|(cached, _)| *cached == key)
        {
            return;
        }
        let data = self.dataset.as_ref().and_then(|(range, dataset)| {
            analysis::AnalysisData::compute(
                dataset,
                *range,
                &self.filter,
                &self.pricing,
                self.cost_mode,
            )
        });
        self.analysis = Some((key, data));
    }

    /// Returns whether a manual refresh was requested.
    fn toolbar(&mut self, ui: &mut egui::Ui) -> bool {
        ui.horizontal_wrapped(|ui| {
            ui.label(h3("Usage"));
            egui::ComboBox::from_id_salt("usage-range")
                .selected_text(self.range.label())
                .show_ui(ui, |ui| {
                    for range in UsageRange::ALL {
                        ui.selectable_value(&mut self.range, range, range.label());
                    }
                });
            egui::ComboBox::from_id_salt("usage-cost-mode")
                .selected_text(self.cost_mode.label())
                .show_ui(ui, |ui| {
                    for mode in [CostMode::Recorded, CostMode::Current] {
                        ui.selectable_value(&mut self.cost_mode, mode, mode.label());
                    }
                })
                .response
                .on_hover_text(
                    "Recorded: the price in force when each request ran. \
                     Current: today's prices for every token.",
                );
            let refresh = ui
                .add_enabled(!self.loader.is_loading(), egui::Button::new("Refresh"))
                .clicked();
            if self.loader.is_loading() {
                ui.spinner();
            }
            refresh
        })
        .inner
    }

    fn filters(&mut self, ui: &mut egui::Ui, sidebar: &SidebarState) {
        let Some((_, dataset)) = &self.dataset else {
            return;
        };
        let facts = &dataset.facts;
        let breakdown = self.active == UsageView::Breakdown;
        ui.horizontal_wrapped(|ui| {
            if breakdown {
                ui.label(muted("Group by"));
                egui::ComboBox::from_id_salt("usage-dimension")
                    .selected_text(self.dimension.label())
                    .show_ui(ui, |ui| {
                        for dimension in UsageDimension::ALL {
                            ui.selectable_value(&mut self.dimension, dimension, dimension.label());
                        }
                    });
            }
            for (label, dimension, selected) in [
                (
                    "Provider",
                    UsageDimension::Provider,
                    &mut self.filter.provider,
                ),
                ("Model", UsageDimension::Model, &mut self.filter.model),
                ("Project", UsageDimension::Project, &mut self.filter.project),
                ("Role", UsageDimension::Role, &mut self.filter.role),
            ] {
                let options = filter_options(facts, dimension);
                if options.len() <= 1 && selected.is_none() {
                    continue;
                }
                let text = selected.as_deref().map_or_else(
                    || format!("{label}: all"),
                    |key| format!("{label}: {}", key_label(dimension, key, sidebar)),
                );
                egui::ComboBox::from_id_salt(("usage-filter", label))
                    .selected_text(text)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(selected, None, "All");
                        for key in options {
                            let name = key_label(dimension, &key, sidebar);
                            ui.selectable_value(selected, Some(key), name);
                        }
                    });
            }
            if !self.filter.is_empty() && ui.button("Clear filters").clicked() {
                self.filter = UsageFilter::default();
            }
            if breakdown {
                ui.checkbox(&mut self.show_models, "Model breakdown");
            }
        });
    }

    fn current_view(&mut self) -> Option<(&[BreakdownRow], UsageTotals)> {
        let (_, dataset) = self.dataset.as_ref()?;
        let key = ViewKey {
            generation: self.generation,
            filter: self.filter.clone(),
            dimension: self.dimension,
            cost_mode: self.cost_mode,
        };
        if self.view.as_ref().is_none_or(|(cached, ..)| *cached != key) {
            let mut costs = CostCalculator::new(&self.pricing, self.cost_mode);
            let (rows, total) = breakdown(&dataset.facts, &self.filter, self.dimension, &mut costs);
            self.view = Some((key, rows, total));
        }
        self.view
            .as_ref()
            .map(|(_, rows, total)| (rows.as_slice(), *total))
    }

    fn breakdown_view(&mut self, ui: &mut egui::Ui, sidebar: &SidebarState) {
        let dimension = self.dimension;
        let show_models = self.show_models;
        let mut sort = self.sort;
        let Some((rows, total)) = self.current_view() else {
            return;
        };
        let mut order: Vec<&BreakdownRow> = rows.iter().collect();
        if let Some((column, descending)) = sort {
            order.sort_by(|left, right| {
                let ordering = column
                    .value(&left.totals)
                    .total_cmp(&column.value(&right.totals));
                if descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            });
        }
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        TableBuilder::new(ui)
            .id_salt("usage-breakdown")
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::right_to_left(egui::Align::Center))
            .column(Column::initial(150.0).at_least(110.0).clip(true))
            .columns(Column::auto().at_least(56.0), COLUMNS.len())
            .header(row_height, |mut header| {
                header.col(|ui| {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.strong(dimension.label());
                    });
                });
                for (title, column, hint) in COLUMNS {
                    header.col(|ui| {
                        let marker = match sort {
                            Some((sorted, true)) if sorted == column => " ▼",
                            Some((sorted, false)) if sorted == column => " ▲",
                            _ => "",
                        };
                        let clicked = ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(format!("{title}{marker}")).strong(),
                                )
                                .frame(false),
                            )
                            .on_hover_text(hint)
                            .clicked();
                        if clicked {
                            sort = match sort {
                                Some((sorted, true)) if sorted == column => Some((column, false)),
                                Some((sorted, false)) if sorted == column => None,
                                _ => Some((column, true)),
                            };
                        }
                    });
                }
            })
            .body(|mut body| {
                for row in order {
                    body.row(row_height, |mut table_row| {
                        let label = key_label(dimension, &row.key, sidebar);
                        totals_row(&mut table_row, &label, &row.totals, false);
                    });
                    if show_models {
                        for (model, totals) in &row.models {
                            body.row(row_height, |mut table_row| {
                                totals_row(
                                    &mut table_row,
                                    &format!("    └ {model}"),
                                    totals,
                                    false,
                                );
                            });
                        }
                    }
                }
                body.row(row_height, |mut table_row| {
                    totals_row(&mut table_row, "Total", &total, true);
                });
            });
        self.sort = sort;
    }
}

fn totals_row(
    row: &mut egui_extras::TableRow<'_, '_>,
    label: &str,
    totals: &UsageTotals,
    strong: bool,
) {
    let text = |value: String| {
        let text = egui::RichText::new(value);
        if strong { text.strong() } else { text }
    };
    row.col(|ui| {
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.label(text(label.to_owned())).on_hover_text(label);
        });
    });
    for (_, column, _) in COLUMNS {
        row.col(|ui| {
            let response = ui.label(text(column.format(totals)));
            if column == SortColumn::Cost && totals.unpriced {
                response
                    .on_hover_text("Some models have no known price; the cost is a lower bound.");
            }
        });
    }
}

/// Human label for a grouping key, resolving project and thread IDs.
pub fn key_label(dimension: UsageDimension, key: &str, sidebar: &SidebarState) -> String {
    if key.is_empty() {
        return match dimension {
            UsageDimension::Project => "(no project)".into(),
            UsageDimension::Thread => "(no thread)".into(),
            _ => "(unknown)".into(),
        };
    }
    match dimension {
        UsageDimension::Week => format!("Week of {key}"),
        UsageDimension::Project => sidebar
            .projects
            .iter()
            .find(|project| project.id.to_string() == key)
            .map_or_else(|| key.to_owned(), |project| project.name.clone()),
        UsageDimension::Thread => sidebar
            .threads
            .iter()
            .find(|thread| thread.id.to_string() == key)
            .map_or_else(|| key.to_owned(), |thread| thread.title.clone()),
        _ => key.to_owned(),
    }
}

/// Token count with K/M/B suffixes.
pub fn compact_tokens(value: u64) -> String {
    let value = value as f64;
    if value >= 1e9 {
        format!("{:.2}B", value / 1e9)
    } else if value >= 1e6 {
        format!("{:.2}M", value / 1e6)
    } else if value >= 1e3 {
        format!("{:.1}K", value / 1e3)
    } else {
        format!("{value:.0}")
    }
}

/// USD with a `*` when part of the usage has no known price.
pub fn format_cost(totals: &UsageTotals) -> String {
    let marker = if totals.unpriced { "*" } else { "" };
    let cost = totals.cost;
    if cost > 0.0 && cost < 0.01 {
        format!("${cost:.4}{marker}")
    } else {
        format!("${cost:.2}{marker}")
    }
}
