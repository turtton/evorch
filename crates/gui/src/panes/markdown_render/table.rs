//! Markdown tables laid out to the message width.
//!
//! egui_commonmark draws tables as an unwrapped `Grid`, so a table wider than
//! the pane loses its trailing columns. Here each cell wraps inside a column
//! width fitted to the available space; only tables whose columns cannot reach
//! a readable minimum fall back to a local horizontal scroller.

use egui::{Align, Layout, Stroke, Ui, epaint::RectShape};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use crate::theme::tokens::{SP_1, SP_2, palette};

/// Narrowest column that still reads as text rather than a letter stack.
const MIN_COLUMN: f32 = 72.0;
const CELL_PAD_X: f32 = SP_2;
const CELL_PAD_Y: f32 = SP_1;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Table {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Table {
    fn columns(&self) -> usize {
        self.rows
            .iter()
            .map(Vec::len)
            .chain([self.header.len()])
            .max()
            .unwrap_or(0)
    }

    fn cells(&self) -> impl Iterator<Item = &[String]> {
        std::iter::once(self.header.as_slice()).chain(self.rows.iter().map(Vec::as_slice))
    }
}

/// Text of one table cell in `source`, without the delimiting pipes.
///
/// Cells hold inline content only, so a leading block marker (`-`, `#`,
/// `1.`, ...) is escaped instead of turning the cell into a list or heading.
pub(super) fn cell_source(source: &str) -> String {
    let cell = source.trim().trim_matches('|').trim();
    let marker_end = cell
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(cell.len());
    let rest = &cell[marker_end..];
    let block_marker = match cell.chars().next() {
        Some('#' | '>') => true,
        Some('-' | '+' | '*') => cell[1..].chars().next().is_none_or(char::is_whitespace),
        Some(c) if c.is_ascii_digit() => {
            rest.starts_with(['.', ')']) && rest[1..].chars().next().is_none_or(char::is_whitespace)
        }
        _ => false,
    };
    if !block_marker {
        return cell.to_owned();
    }
    let at = if cell.starts_with(|c: char| c.is_ascii_digit()) {
        marker_end
    } else {
        0
    };
    format!("{}\\{}", &cell[..at], &cell[at..])
}

/// Column widths (cell padding included) for `natural` widths in `available`.
///
/// Columns keep their natural width when the table fits. Otherwise every
/// column starts from its readable minimum and the remaining space goes to the
/// columns that need it, in proportion to how much they would wrap.
pub(super) fn fit_columns(natural: &[f32], available: f32) -> Vec<f32> {
    if natural.iter().sum::<f32>() <= available {
        return natural.to_vec();
    }
    let minimum: Vec<f32> = natural.iter().map(|width| width.min(MIN_COLUMN)).collect();
    let spare = available - minimum.iter().sum::<f32>();
    let demand: f32 = natural.iter().zip(&minimum).map(|(n, m)| n - m).sum();
    if spare <= 0.0 || demand <= 0.0 {
        return minimum;
    }
    natural
        .iter()
        .zip(&minimum)
        .map(|(natural, minimum)| minimum + spare * (natural - minimum) / demand)
        .collect()
}

pub(super) fn show(ui: &mut Ui, id_salt: &str, table: &Table, width: f32) {
    let columns = table.columns();
    if columns == 0 {
        return;
    }
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut natural = vec![0.0_f32; columns];
    for row in table.cells() {
        for (column, cell) in row.iter().enumerate() {
            let text = ui.fonts_mut(|fonts| {
                fonts
                    .layout_no_wrap(cell.clone(), font.clone(), palette().TEXT)
                    .size()
                    .x
            });
            natural[column] = natural[column].max(text + 2.0 * CELL_PAD_X);
        }
    }
    let widths = fit_columns(&natural, width);
    let total: f32 = widths.iter().sum();
    let draw = |ui: &mut Ui| {
        ui.set_width(total);
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
        let border = palette().BORDER;
        let top = ui.cursor().top();
        for (index, row) in table.cells().enumerate() {
            let background = ui.painter().add(egui::Shape::Noop);
            let response = ui
                .horizontal_top(|ui| {
                    for (column, column_width) in widths.iter().enumerate() {
                        let cell = row.get(column).map_or("", String::as_str);
                        show_cell(ui, cell, *column_width, index == 0);
                    }
                })
                .response;
            let rect = response.rect.with_max_x(response.rect.left() + total);
            let fill = if index == 0 {
                Some(palette().SURFACE_RAISED)
            } else if index % 2 == 0 {
                Some(palette().SURFACE)
            } else {
                None
            };
            if let Some(fill) = fill {
                ui.painter()
                    .set(background, RectShape::filled(rect, 0.0, fill));
            }
            if index > 0 {
                ui.painter()
                    .hline(rect.x_range(), rect.top(), Stroke::new(1.0, border));
            }
        }
        let outline = egui::Rect::from_min_max(
            egui::pos2(ui.min_rect().left(), top),
            egui::pos2(ui.min_rect().left() + total, ui.min_rect().bottom()),
        );
        ui.painter().rect_stroke(
            outline,
            crate::theme::tokens::R_SM,
            Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );
    };
    if total <= width + 0.5 {
        ui.allocate_ui_with_layout(egui::vec2(width, 0.0), Layout::top_down(Align::Min), draw);
    } else {
        egui::ScrollArea::horizontal()
            .id_salt(id_salt)
            .max_width(width)
            .show(ui, draw);
    }
}

fn show_cell(ui: &mut Ui, source: &str, width: f32, header: bool) {
    ui.allocate_ui_with_layout(egui::vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.set_width(width);
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(CELL_PAD_X as i8, CELL_PAD_Y as i8))
            .show(ui, |ui| {
                ui.set_width((width - 2.0 * CELL_PAD_X).max(0.0));
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                if header {
                    ui.visuals_mut().override_text_color = Some(palette().TEXT);
                }
                CommonMarkViewer::new().show(ui, &mut CommonMarkCache::default(), source);
            });
    });
}

#[cfg(test)]
mod tests {
    use super::{MIN_COLUMN, cell_source, fit_columns};

    #[test]
    fn cells_escape_block_markers_but_keep_inline_markup() {
        assert_eq!(cell_source("| - |"), "\\-");
        assert_eq!(cell_source(" # 1 "), "\\# 1");
        assert_eq!(cell_source("1. step"), "1\\. step");
        assert_eq!(cell_source("*bold*"), "*bold*");
        assert_eq!(cell_source("-1"), "-1");
        assert_eq!(cell_source("2026"), "2026");
    }

    #[test]
    fn natural_widths_are_kept_when_the_table_fits() {
        assert_eq!(fit_columns(&[50.0, 120.0], 400.0), vec![50.0, 120.0]);
    }

    #[test]
    fn wide_columns_shrink_while_narrow_ones_keep_their_width() {
        // Given: one short column and two long ones in a narrow message.
        let widths = fit_columns(&[40.0, 600.0, 300.0], 400.0);
        // Then: the table fills the width and the short column is untouched.
        assert!((widths.iter().sum::<f32>() - 400.0).abs() < 0.01);
        assert_eq!(widths[0], 40.0);
        assert!(widths[1] > widths[2]);
    }

    #[test]
    fn columns_stop_at_a_readable_minimum() {
        let widths = fit_columns(&[300.0, 300.0, 300.0], 100.0);
        assert_eq!(widths, vec![MIN_COLUMN; 3]);
    }
}
