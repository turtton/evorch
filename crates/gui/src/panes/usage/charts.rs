//! Shared chart parts for the Usage tab: palette, sparkline, legend and grid.
//!
//! Both workbench themes are dark, so the categorical slots are the dark steps
//! of the reference data-viz palette, validated against both theme surfaces.

use egui::{Color32, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};

use egui_plot::{Bar, BarChart, Plot};

use super::compact_tokens;
use crate::model::usage_stats::{LocalDay, UsageTotals};
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
                ui.label(crate::theme::text::muted(short))
                    .on_hover_text(label);
            }
            ui.end_row();
        }
        for (row, label) in row_labels.iter().enumerate() {
            ui.label(crate::theme::text::muted(label.clone()));
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

/// A named token measure stacked in daily bars.
pub type TokenLayer = (&'static str, fn(&UsageTotals) -> u64);

/// Input split by cache use, then output. Cache charts use the first three.
pub const TOKEN_LAYERS: [TokenLayer; 4] = [
    ("Uncached input", UsageTotals::uncached_input),
    ("Cache write", |totals| totals.tokens.cache_write),
    ("Cache read", |totals| totals.tokens.cache_read),
    ("Output", |totals| totals.tokens.output),
];

/// One stacked bar per day, a layer per categorical slot in order.
pub fn stacked_day_bars(
    days: &[(LocalDay, UsageTotals)],
    first: LocalDay,
    layers: &[TokenLayer],
) -> Vec<BarChart> {
    let mut charts: Vec<BarChart> = Vec::new();
    for (index, (name, value)) in layers.iter().enumerate() {
        let bars = days
            .iter()
            .enumerate()
            .map(|(day, (_, totals))| Bar::new(day as f64, value(totals) as f64).width(0.7))
            .collect();
        let name = (*name).to_owned();
        let mut chart = BarChart::new(name.clone(), bars)
            .color(SERIES[index])
            .element_formatter(Box::new(move |bar, _| {
                format!(
                    "{} · {name}: {}",
                    first.add_days(bar.argument as i64),
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
