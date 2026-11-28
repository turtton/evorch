//! Usage charts, stat tiles, the breakdown table and the context budget bar, shared by
//! the embedded workbench's Usage and Context tabs and the core client's Usage tab
//! (ADR 0045).
//!
//! Pure over egui, the theme and plain numbers ([`UsageCells`]): nothing here reads the
//! usage ledger or prices. Each side turns what it read into cells.
//!
//! Both workbench themes are dark, so the categorical slots are the dark steps
//! of the reference data-viz palette, validated against both theme surfaces.

use egui::{Color32, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};
use egui_extras::{Column, TableBuilder};
use egui_plot::{Bar, BarChart, Legend, Plot};

use crate::format::compact_count as compact_tokens;
use crate::model::local_day::LocalDay;
use crate::theme::text::muted;
use crate::theme::tokens::palette;

/// Categorical slots in fixed order; a ninth entity folds into "Other".
pub const SERIES: [Color32; 8] = [
    Color32::from_rgb(0x39, 0x87, 0xe5),
    Color32::from_rgb(0xd9, 0x59, 0x26),
    Color32::from_rgb(0x19, 0x9e, 0x70),
    Color32::from_rgb(0xc9, 0x85, 0x00),
    Color32::from_rgb(0xd5, 0x51, 0x81),
    Color32::from_rgb(0x00, 0x83, 0x00),
    Color32::from_rgb(0x90, 0x85, 0xe9),
    Color32::from_rgb(0xe6, 0x67, 0x67),
];

/// Color for a folded tail ("Other"): neutral, never a series hue.
pub fn other_color() -> Color32 {
    palette().TEXT_MUTED
}

/// Color of `key` by its position in a stable, filter-independent order.
pub fn entity_color(order: &[String], key: &str) -> Color32 {
    order
        .iter()
        .position(|candidate| candidate == key)
        .and_then(|index| SERIES.get(index).copied())
        .unwrap_or_else(other_color)
}

/// Single-hue blue ramp for magnitude; dark (near zero) to light (high) so
/// low values recede into the dark surface.
const SEQUENTIAL: [Color32; 6] = [
    Color32::from_rgb(0x10, 0x42, 0x81),
    Color32::from_rgb(0x1c, 0x5c, 0xab),
    Color32::from_rgb(0x2a, 0x78, 0xd6),
    Color32::from_rgb(0x55, 0x98, 0xe7),
    Color32::from_rgb(0x86, 0xb6, 0xef),
    Color32::from_rgb(0xb7, 0xd3, 0xf6),
];

/// Sequential color for `value / max`; zero is the raised surface.
pub fn sequential(value: f64, max: f64) -> Color32 {
    if value <= 0.0 || max <= 0.0 {
        return palette().SURFACE_RAISED;
    }
    let ratio = (value / max).clamp(0.0, 1.0);
    let index = ((ratio * SEQUENTIAL.len() as f64).ceil() as usize).clamp(1, SEQUENTIAL.len());
    SEQUENTIAL[index - 1]
}

/// A small line drawn under a stat tile; no axes, single series.
pub fn sparkline(ui: &mut Ui, values: &[f64], color: Color32, size: Vec2) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let max = values.iter().copied().fold(0.0_f64, f64::max);
    if values.len() < 2 || max <= 0.0 {
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() - 1.0,
            Stroke::new(1.0, palette().BORDER),
        );
        return;
    }
    let step = rect.width() / (values.len() - 1) as f32;
    let points: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            pos2(
                rect.left() + step * index as f32,
                rect.bottom() - (value / max) as f32 * (rect.height() - 2.0) - 1.0,
            )
        })
        .collect();
    ui.painter()
        .add(egui::Shape::line(points, Stroke::new(2.0, color)));
}

/// A legend swatch followed by its label in text ink.
pub fn legend_item(ui: &mut Ui, color: Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
    ui.label(label);
}

/// A part-to-whole bar with a 2px surface gap between segments.
pub fn share_bar(ui: &mut Ui, segments: &[(f64, Color32, String)], height: f32) {
    let width = ui.available_width().max(120.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let total: f64 = segments.iter().map(|(value, ..)| value).sum();
    if total <= 0.0 {
        ui.painter()
            .rect_filled(rect, 4.0, palette().SURFACE_RAISED);
        return;
    }
    let mut left = rect.left();
    for (value, color, label) in segments {
        let segment_width = (value / total) as f32 * rect.width();
        if segment_width <= 0.0 {
            continue;
        }
        let segment = Rect::from_min_max(
            pos2(left, rect.top()),
            pos2((left + segment_width - 2.0).max(left + 1.0), rect.bottom()),
        );
        ui.painter().rect_filled(segment, 4.0, *color);
        ui.interact(segment, ui.id().with(("share", label)), Sense::hover())
            .on_hover_text(label);
        left += segment_width;
    }
}

/// A grid of cells colored by `values[row][column] / max`, with row labels on
/// the left. Each cell carries a hover tooltip.
/// `column_labels` may be empty to omit the header row.
pub fn heat_grid(
    ui: &mut Ui,
    id: &str,
    row_labels: &[String],
    column_labels: &[String],
    values: &[Vec<f64>],
    tooltips: &dyn Fn(usize, usize) -> String,
    cell: Vec2,
) {
    let max = values.iter().flatten().copied().fold(0.0_f64, f64::max);
    egui::Grid::new(id).spacing(vec2(2.0, 2.0)).show(ui, |ui| {
        if !column_labels.is_empty() {
            ui.label("");
            for label in column_labels {
                let short: String = label.chars().take(10).collect();
                ui.label(muted(short)).on_hover_text(label);
            }
            ui.end_row();
        }
        for (row, label) in row_labels.iter().enumerate() {
            ui.label(muted(label.clone()));
            for (column, value) in values[row].iter().enumerate() {
                let (rect, response) = ui.allocate_exact_size(cell, Sense::hover());
                ui.painter().rect_filled(rect, 2.0, sequential(*value, max));
                if response.hovered() {
                    ui.painter().rect_stroke(
                        rect,
                        2.0,
                        Stroke::new(1.0, palette().TEXT),
                        StrokeKind::Inside,
                    );
                }
                response.on_hover_text(tooltips(row, column));
            }
            ui.end_row();
        }
    });
}

/// `MM-DD` labels for a day-indexed x axis starting at `first`.
pub fn day_axis_label(first: LocalDay, value: f64) -> String {
    if value.fract().abs() > 1e-6 || value < 0.0 {
        return String::new();
    }
    let (_, month, day) = first.add_days(value as i64).ymd();
    format!("{month:02}-{day:02}")
}

/// Section heading used above each chart.
pub fn section(ui: &mut Ui, title: &str, hint: &str) {
    ui.add_space(8.0);
    ui.label(crate::theme::text::h4(title)).on_hover_text(hint);
}

/// A segmented-control button. The theme's selection stroke matches its fill,
/// so the selected state sets accent foreground text explicitly.
pub fn segment(ui: &mut Ui, selected: bool, label: &str) -> bool {
    let colors = palette();
    let text = egui::RichText::new(label).color(if selected {
        colors.ACCENT_FG
    } else {
        colors.TEXT
    });
    let fill = if selected {
        colors.ACCENT
    } else {
        Color32::TRANSPARENT
    };
    ui.add(egui::Button::new(text).fill(fill).selected(selected))
        .clicked()
}

pub type Layer<T> = (&'static str, fn(&T) -> u64);

pub const UNCACHED_INPUT: &str = "Uncached input";
pub const CACHE_WRITE: &str = "Cache write";
pub const CACHE_READ: &str = "Cache read";
pub const OUTPUT: &str = "Output";

/// One stacked bar per day, a layer per categorical slot in order.
pub fn stacked_day_bars<T: Copy>(
    days: &[(LocalDay, T)],
    first: LocalDay,
    layers: &[Layer<T>],
) -> Vec<BarChart> {
    let buckets: Vec<T> = days.iter().map(|(_, bucket)| *bucket).collect();
    stacked_bars(&buckets, layers, move |index| {
        first.add_days(index).to_string()
    })
}

/// One stacked bar per bucket; `bucket_label` names a bucket in tooltips.
pub fn stacked_bars<T>(
    buckets: &[T],
    layers: &[Layer<T>],
    bucket_label: impl Fn(i64) -> String + Clone + 'static,
) -> Vec<BarChart> {
    let mut charts: Vec<BarChart> = Vec::new();
    for (index, (name, value)) in layers.iter().enumerate() {
        let bars = buckets
            .iter()
            .enumerate()
            .map(|(bucket, totals)| Bar::new(bucket as f64, value(totals) as f64).width(0.7))
            .collect();
        let name = (*name).to_owned();
        let label = bucket_label.clone();
        let mut chart = BarChart::new(name.clone(), bars)
            .color(SERIES[index])
            .element_formatter(Box::new(move |bar, _| {
                format!(
                    "{} · {name}: {}",
                    label(bar.argument as i64),
                    compact_tokens(bar.value as u64)
                )
            }));
        let below: Vec<&BarChart> = charts.iter().collect();
        if !below.is_empty() {
            chart = chart.stack_on(&below);
        }
        charts.push(chart);
    }
    charts
}

/// A static plot whose x axis counts days from `first`.
pub fn day_plot(id: &str, first: LocalDay, height: f32) -> Plot<'static> {
    Plot::new(id)
        .height(height)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .show_grid([false, true])
        .x_axis_formatter(move |mark, _| day_axis_label(first, mark.value))
}

/// A stat tile: muted title, headline value, muted detail and an optional sparkline.
pub struct Tile {
    pub title: String,
    pub value: String,
    pub detail: String,
    pub sparkline: Option<Vec<f64>>,
    /// Why the value is what it is (a lower bound, say), added to its tooltips.
    pub hint: Option<String>,
}

impl Tile {
    fn tooltip(&self, text: &str) -> String {
        match &self.hint {
            Some(hint) => format!("{text}\n{hint}"),
            None => text.to_owned(),
        }
    }
}

/// Lays tiles out in rows that fit the available width. Frames report their
/// size only after drawing, so the column count is computed up front.
pub fn tiles(ui: &mut Ui, width: f32, tiles: Vec<Tile>) {
    let spacing = ui.spacing().item_spacing.x;
    let outer = width + 20.0 + spacing;
    let columns = ((ui.available_width() + spacing) / outer).floor().max(1.0) as usize;
    let mut tiles = tiles.into_iter().peekable();
    while tiles.peek().is_some() {
        ui.horizontal(|ui| {
            for tile in tiles.by_ref().take(columns) {
                egui::Frame::new()
                    .fill(palette().SURFACE_RAISED)
                    .corner_radius(8)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            ui.label(muted(tile.title.clone()));
                            ui.add(
                                egui::Label::new(crate::theme::text::h2(tile.value.clone()))
                                    .truncate(),
                            )
                            .on_hover_text(tile.tooltip(&tile.value));
                            ui.add(egui::Label::new(muted(tile.detail.clone())).truncate())
                                .on_hover_text(tile.tooltip(&tile.detail));
                            if let Some(values) = &tile.sparkline {
                                sparkline(ui, values, SERIES[0], vec2(width, 26.0));
                            }
                        });
                    });
            }
        });
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageCells {
    pub requests: u64,
    pub failed: u64,
    /// All input, cache reads and writes included.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub cost: f64,
    /// Some tokens had no known price, or some requests reported no usage.
    pub lower_bound: bool,
    pub ttft_ms: Option<u64>,
}

impl UsageCells {
    /// Input tokens that were neither read from nor written to the cache.
    pub fn uncached_input(&self) -> u64 {
        self.input
            .saturating_sub(self.cache_read)
            .saturating_sub(self.cache_write)
    }

    pub fn total_tokens(&self) -> u64 {
        self.input + self.output
    }

    /// Share of input served from cache, in percent.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        (self.input > 0).then(|| (self.cache_read as f64 / self.input as f64 * 100.0).min(100.0))
    }

    pub fn success_rate(&self) -> Option<f64> {
        (self.requests > 0)
            .then(|| (self.requests - self.failed) as f64 / self.requests as f64 * 100.0)
    }

    /// Blended dollars per million total tokens.
    pub fn cost_per_million(&self) -> Option<f64> {
        let total = self.total_tokens();
        (total > 0 && self.cost > 0.0).then(|| self.cost / total as f64 * 1_000_000.0)
    }

    /// USD with a `*` when the cost is a lower bound.
    pub fn cost_label(&self) -> String {
        crate::format::cost_label(self.cost, self.lower_bound)
    }
}

/// Input split by cache use, then output. Cache charts use the first three.
pub const CELL_LAYERS: [Layer<UsageCells>; 4] = [
    (UNCACHED_INPUT, UsageCells::uncached_input),
    (CACHE_WRITE, |cells| cells.cache_write),
    (CACHE_READ, |cells| cells.cache_read),
    (OUTPUT, |cells| cells.output),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
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
    pub fn format(self, cells: &UsageCells) -> String {
        match self {
            Self::Requests => cells.requests.to_string(),
            Self::Input => compact_tokens(cells.uncached_input()),
            Self::CacheWrite => compact_tokens(cells.cache_write),
            Self::CacheRead => compact_tokens(cells.cache_read),
            Self::Output => compact_tokens(cells.output),
            Self::Total => compact_tokens(cells.total_tokens()),
            Self::CacheHit => cells
                .cache_hit_rate()
                .map_or_else(|| "—".into(), |rate| format!("{rate:.1}%")),
            Self::Cost => cells.cost_label(),
        }
    }

    pub fn value(self, cells: &UsageCells) -> f64 {
        match self {
            Self::Requests => cells.requests as f64,
            Self::Input => cells.uncached_input() as f64,
            Self::CacheWrite => cells.cache_write as f64,
            Self::CacheRead => cells.cache_read as f64,
            Self::Output => cells.output as f64,
            Self::Total => cells.total_tokens() as f64,
            Self::CacheHit => cells.cache_hit_rate().unwrap_or_default(),
            Self::Cost => cells.cost,
        }
    }
}

// Cost and totals first: they stay visible when the tab is narrow.
pub const COLUMNS: [(&str, SortColumn, &str); 8] = [
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

pub const LOWER_BOUND_HINT: &str =
    "Some requests have no known price or reported no usage; the cost is a lower bound.";

pub struct TableRow {
    pub label: String,
    pub cells: UsageCells,
    pub children: Vec<(String, UsageCells)>,
}

/// `vscroll` is off when the table already sits in a vertical scroll area, as nested
/// vertical scroll areas would leave the table a fixed, cramped height.
#[allow(clippy::too_many_arguments)]
pub fn breakdown_table(
    ui: &mut Ui,
    id: &str,
    dimension_label: &str,
    rows: &[TableRow],
    total: &UsageCells,
    sort: &mut Option<(SortColumn, bool)>,
    show_children: bool,
    vscroll: bool,
) {
    let current = *sort;
    let mut order: Vec<&TableRow> = rows.iter().collect();
    if let Some((column, descending)) = current {
        order.sort_by(|left, right| {
            let ordering = column
                .value(&left.cells)
                .total_cmp(&column.value(&right.cells));
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
    }
    let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    TableBuilder::new(ui)
        .id_salt(id)
        .striped(true)
        .resizable(true)
        .vscroll(vscroll)
        .cell_layout(egui::Layout::right_to_left(egui::Align::Center))
        .column(Column::initial(150.0).at_least(110.0).clip(true))
        .columns(Column::auto().at_least(56.0), COLUMNS.len())
        .header(row_height, |mut header| {
            header.col(|ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.strong(dimension_label);
                });
            });
            for (title, column, hint) in COLUMNS {
                header.col(|ui| {
                    let marker = match current {
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
                        *sort = match current {
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
                    cells_row(&mut table_row, &row.label, &row.cells, false);
                });
                if show_children {
                    for (model, cells) in &row.children {
                        body.row(row_height, |mut table_row| {
                            cells_row(&mut table_row, &format!("    └ {model}"), cells, false);
                        });
                    }
                }
            }
            body.row(row_height, |mut table_row| {
                cells_row(&mut table_row, "Total", total, true);
            });
        });
}

fn cells_row(
    row: &mut egui_extras::TableRow<'_, '_>,
    label: &str,
    cells: &UsageCells,
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
            let response = ui.label(text(column.format(cells)));
            if column == SortColumn::Cost && cells.lower_bound {
                response.on_hover_text(LOWER_BOUND_HINT);
            }
        });
    }
}

pub const TILE_WIDTH: f32 = 132.0;

/// `day_count` is the number of days `total` covers, which can exceed `days` when
/// older days are left out of the charts.
pub fn overview_tiles_over(
    total: &UsageCells,
    days: &[(LocalDay, UsageCells)],
    day_count: usize,
) -> Vec<Tile> {
    let daily = |metric: fn(&UsageCells) -> f64| -> Vec<f64> {
        days.iter().map(|(_, cells)| metric(cells)).collect()
    };
    let day_count = day_count.max(1) as f64;
    let tiles: [(&str, String, String, Vec<f64>); 6] = [
        (
            "Cost",
            total.cost_label(),
            format!("${:.2} / day", total.cost / day_count),
            daily(|cells| cells.cost),
        ),
        (
            "Tokens",
            compact_tokens(total.total_tokens()),
            format!(
                "in {} · out {}",
                compact_tokens(total.input),
                compact_tokens(total.output)
            ),
            daily(|cells| cells.total_tokens() as f64),
        ),
        (
            "Requests",
            total.requests.to_string(),
            total
                .success_rate()
                .map_or_else(|| "—".into(), |rate| format!("{rate:.1}% succeeded")),
            daily(|cells| cells.requests as f64),
        ),
        (
            "Cache hit",
            total
                .cache_hit_rate()
                .map_or_else(|| "—".into(), |rate| format!("{rate:.1}%")),
            format!("{} read", compact_tokens(total.cache_read)),
            daily(|cells| cells.cache_hit_rate().unwrap_or_default()),
        ),
        (
            "Per 1M tokens",
            total
                .cost_per_million()
                .map_or_else(|| "—".into(), |cost| format!("${cost:.2}")),
            "blended cost".into(),
            daily(|cells| cells.cost_per_million().unwrap_or_default()),
        ),
        (
            "Avg TTFT",
            total
                .ttft_ms
                .map_or_else(|| "—".into(), |ttft| crate::format::latency(ttft as f64)),
            format!("{} reasoning", compact_tokens(total.reasoning)),
            daily(|cells| cells.ttft_ms.unwrap_or_default() as f64),
        ),
    ];
    tiles
        .into_iter()
        .map(|(title, value, detail, values)| Tile {
            title: title.to_owned(),
            value,
            detail,
            sparkline: Some(values),
            hint: None,
        })
        .collect()
}

pub fn daily_token_chart(ui: &mut Ui, id: &str, first: LocalDay, days: &[(LocalDay, UsageCells)]) {
    let charts = stacked_day_bars(days, first, &CELL_LAYERS);
    day_plot(id, first, 170.0)
        .legend(Legend::default())
        .y_axis_formatter(|mark, _| compact_tokens(mark.value.max(0.0) as u64))
        .show(ui, |plot| {
            for chart in charts {
                plot.bar_chart(chart);
            }
        });
}

pub fn daily_cost_chart(ui: &mut Ui, id: &str, first: LocalDay, days: &[(LocalDay, UsageCells)]) {
    let bars = days
        .iter()
        .enumerate()
        .map(|(day, (_, cells))| Bar::new(day as f64, cells.cost).width(0.7))
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
    day_plot(id, first, 170.0)
        .y_axis_formatter(|mark, _| format!("${:.2}", mark.value))
        .show(ui, |plot| plot.bar_chart(chart));
}

/// `days` must be contiguous: each one is placed by its offset from the first.
pub fn activity_grid(ui: &mut Ui, id: &str, days: &[(LocalDay, UsageCells)]) {
    let (Some((first, _)), Some((last, _))) = (days.first(), days.last()) else {
        return;
    };
    let start = first.week_start();
    let weeks = (last.days_since(start) / 7 + 1) as usize;
    let mut values = vec![vec![0.0; weeks]; 7];
    let mut labels = vec![vec![String::new(); weeks]; 7];
    for (day, cells) in days {
        let offset = day.days_since(start);
        let (week, weekday) = ((offset / 7) as usize, (offset % 7) as usize);
        values[weekday][week] = cells.total_tokens() as f64;
        labels[weekday][week] = format!(
            "{day}: {} tokens · {}",
            compact_tokens(cells.total_tokens()),
            cells.cost_label()
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
    let grid = format!("{id}-grid");
    egui::ScrollArea::horizontal().id_salt(id).show(ui, |ui| {
        heat_grid(ui, &grid, &rows, &[], &values, &tooltip, vec2(14.0, 14.0));
    });
}

pub struct ShareRow {
    pub label: String,
    pub color: Color32,
    pub fraction: f64,
    pub cells: UsageCells,
}

pub const EMPTY_SHARES: &str = "No usage for these filters.";

/// A part-to-whole bar plus one row per slice.
pub fn share_list(ui: &mut Ui, id_salt: impl std::hash::Hash + std::fmt::Debug, rows: &[ShareRow]) {
    if rows.is_empty() {
        ui.label(muted(EMPTY_SHARES));
        return;
    }
    share_bar(
        ui,
        &rows
            .iter()
            .map(|row| {
                (
                    row.fraction,
                    row.color,
                    format!("{}: {:.1}%", row.label, row.fraction * 100.0),
                )
            })
            .collect::<Vec<_>>(),
        14.0,
    );
    egui::Grid::new(id_salt).striped(true).show(ui, |ui| {
        for row in rows {
            ui.horizontal(|ui| legend_item(ui, row.color, &row.label));
            ui.label(format!("{:.1}%", row.fraction * 100.0));
            ui.label(row.cells.cost_label());
            ui.label(format!("{} tok", compact_tokens(row.cells.total_tokens())));
            ui.label(
                row.cells
                    .cache_hit_rate()
                    .map_or_else(|| "cache —".into(), |rate| format!("cache {rate:.0}%")),
            );
            ui.end_row();
        }
    });
}

pub fn segment_color(index: usize) -> Color32 {
    SERIES[index % SERIES.len()]
}

/// Heading of a run's message list, plus a note when `hidden` replay-only messages are
/// not listed. `listed` falls short of `total` without a note while pages remain.
pub fn message_heading(listed: usize, hidden: usize, total: usize) -> (String, Option<String>) {
    let heading = if listed == total {
        format!("Messages ({listed})")
    } else {
        format!("Messages ({listed} of {total})")
    };
    let noun = if hidden == 1 { "message" } else { "messages" };
    let note =
        (hidden > 0).then(|| format!("{hidden} replay-only {noun} hidden · counted in the budget"));
    (heading, note)
}

/// [`budget_bar`] without a free share when the context window is not known.
pub fn segments_bar(ui: &mut Ui, segments: &[(&str, u64)], window: Option<u64>) {
    let used: u64 = segments.iter().map(|(_, tokens)| tokens).sum();
    let scale = window.unwrap_or(used).max(used).max(1) as f32;
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, 14.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, palette().BORDER);
    let mut x = rect.left();
    for (index, (_, tokens)) in segments.iter().enumerate() {
        let w = rect.width() * *tokens as f32 / scale;
        let part = Rect::from_min_max(
            pos2(x, rect.top()),
            pos2((x + w).min(rect.right()), rect.bottom()),
        );
        painter.rect_filled(part, 0.0, segment_color(index));
        x += w;
    }
    response.on_hover_text(
        segments
            .iter()
            .map(|(label, tokens)| format!("{label}: {tokens}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    ui.horizontal_wrapped(|ui| {
        for (index, (label, tokens)) in segments.iter().enumerate() {
            ui.label(egui::RichText::new("■").color(segment_color(index)));
            ui.label(muted(format!("{label} {}", compact_tokens(*tokens))));
        }
        if let Some(window) = window {
            ui.label(egui::RichText::new("■").color(palette().BORDER));
            ui.label(muted(format!(
                "Free {}",
                compact_tokens(window.saturating_sub(used))
            )));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(input: u64, cache_read: u64, cost: f64, lower_bound: bool) -> UsageCells {
        UsageCells {
            requests: 4,
            failed: 1,
            input,
            output: 30,
            cache_read,
            cache_write: 10,
            reasoning: 7,
            cost,
            lower_bound,
            ttft_ms: None,
        }
    }

    #[test]
    fn columns_read_cells_as_the_usage_tab_shows_them() {
        let none = cells(0, 0, 0.0042, true);
        assert_eq!(SortColumn::CacheHit.format(&none), "—");
        assert_eq!(SortColumn::Cost.format(&none), "$0.0042*");
        assert_eq!(SortColumn::Input.format(&none), "0");

        let some = cells(400, 100, 1.5, false);
        assert_eq!(SortColumn::CacheHit.format(&some), "25.0%");
        assert_eq!(SortColumn::Cost.format(&some), "$1.50");
        assert_eq!(SortColumn::Input.format(&some), "290");
        assert_eq!(SortColumn::Total.format(&some), "430");
        assert_eq!(SortColumn::CacheHit.value(&some), 25.0);
    }
}
