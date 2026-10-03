//! Read-only code review surface, inspired by crit.md's unified/split diffs.

use std::collections::BTreeSet;
use std::sync::Arc;

use egui::{Color32, FontId, Rect, RichText, Sense, Stroke, UiBuilder};
use egui_extras::syntax_highlighting::{CodeTheme, highlight};

use crate::diff::presentation::{DiffDocument, DiffFile, DiffLine, LineKind};
use crate::diff::{DiffMode, DiffModel, DiffState};
use crate::theme::tokens::{FONT_MONO, SP_2, palette};
use crate::theme::widgets::empty_state;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum View {
    #[default]
    Unified,
    Split,
}

struct CachedDocument {
    text: String,
    document: DiffDocument,
    unified: Vec<Row>,
    split: Vec<Row>,
    code_columns: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    File(usize),
    Line(usize, Option<usize>, Option<usize>),
}

impl Row {
    fn file(self) -> usize {
        match self {
            Self::File(file) | Self::Line(file, ..) => file,
        }
    }
}

pub(crate) fn selected_mode(ui: &egui::Ui) -> DiffMode {
    ui.ctx()
        .data_mut(|data| data.get_temp::<DiffMode>(ui.id().with("selected_mode")))
        .unwrap_or(DiffMode::WorkingTree)
}

/// Render the diff tab and return an optional manual fetch request.
pub fn diff_pane(ui: &mut egui::Ui, diff: &DiffModel) -> Option<DiffMode> {
    let mode_id = ui.id().with("selected_mode");
    let view_id = ui.id().with("diff_view");
    let mut mode = ui
        .ctx()
        .data_mut(|data| data.get_temp::<DiffMode>(mode_id))
        .unwrap_or(DiffMode::WorkingTree);
    let mut view = ui
        .ctx()
        .data_mut(|data| data.get_temp::<View>(view_id))
        .unwrap_or_default();
    let mut requested = None;
    ui.horizontal_wrapped(|ui| {
        for (candidate, label) in [
            (DiffMode::WorkingTree, "Working tree"),
            (DiffMode::Branch, "Branch vs main"),
        ] {
            if ui.selectable_label(mode == candidate, label).clicked() {
                mode = candidate;
                requested = Some(mode.clone());
            }
        }
        if ui.button("Refresh").clicked() {
            requested = Some(mode.clone());
        }
        ui.label(
            RichText::new(if diff.is_snapshot() {
                "Snapshot"
            } else {
                "Auto-updating"
            })
            .small()
            .color(palette().TEXT_MUTED),
        )
        .on_hover_text(if diff.is_snapshot() {
            "Restored snapshot. Choose a scope or Refresh to return to live changes."
        } else {
            "Changes update automatically every 2 seconds while this tab is visible."
        });
        ui.separator();
        ui.selectable_value(&mut view, View::Unified, "Unified");
        ui.selectable_value(&mut view, View::Split, "Split");
    });
    ui.ctx().data_mut(|data| {
        data.insert_temp(mode_id, mode.clone());
        data.insert_temp(view_id, view);
    });
    ui.separator();
    if let Some(error) = diff.refresh_error(&mode) {
        ui.label(
            RichText::new(format!("Refresh failed: {error}. Retrying automatically…"))
                .color(palette().WARNING_FG),
        );
    }
    match diff.state(&mode) {
        DiffState::Idle => {
            empty_state(
                ui,
                "No diff loaded",
                "Select a project to see its changes automatically.",
                None,
            );
        }
        DiffState::Empty => {
            empty_state(
                ui,
                "no changes",
                "Changes appear automatically while this tab is open.",
                None,
            );
        }
        DiffState::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(match mode {
                    DiffMode::WorkingTree => "Loading working tree diff…",
                    DiffMode::Branch => "Loading branch diff against main…",
                });
            });
        }
        DiffState::Ready { text } => diff_body(ui, text, view),
        DiffState::Truncated {
            text,
            total_bytes,
            cap,
        } => {
            ui.label(
                RichText::new(format!("truncated: showing {cap} of {total_bytes} bytes"))
                    .color(palette().WARNING_FG),
            );
            diff_body(ui, text, view);
        }
        DiffState::Error { message } => {
            ui.label(RichText::new(format!("error: {message}")).color(palette().ERROR_FG));
        }
    }
    requested
}

fn diff_body(ui: &mut egui::Ui, text: &str, view: View) {
    let cache_id = ui.id().with("diff_document");
    let cached = ui
        .ctx()
        .data_mut(|data| data.get_temp::<Arc<CachedDocument>>(cache_id));
    let cached = cached
        .filter(|cached| cached.text == text)
        .unwrap_or_else(|| {
            let document = DiffDocument::parse(text);
            let unified = rows(&document, View::Unified);
            let split = rows(&document, View::Split);
            let code_columns = document
                .files
                .iter()
                .flat_map(|file| &file.lines)
                .map(|line| line.text.chars().count())
                .max()
                .unwrap_or(0);
            let cached = Arc::new(CachedDocument {
                text: text.into(),
                document,
                unified,
                split,
                code_columns,
            });
            ui.ctx()
                .data_mut(|data| data.insert_temp(cache_id, cached.clone()));
            cached
        });
    if cached.document.files.is_empty() {
        // Tool snapshots can contain non-Git text; retain it verbatim.
        egui::ScrollArea::both().show(ui, |ui| {
            ui.add(
                egui::Label::new(RichText::new(text).monospace())
                    .selectable(true)
                    .extend(),
            );
        });
        return;
    }
    ui.horizontal(|ui| {
        ui.label(format!("{} changed files", cached.document.files.len()));
        counts(ui, cached.document.additions(), cached.document.deletions());
    });
    let collapsed_id = ui.id().with("collapsed_diff_files");
    let mut collapsed = ui
        .ctx()
        .data_mut(|data| data.get_temp::<BTreeSet<String>>(collapsed_id))
        .unwrap_or_default();
    let all_rows = match view {
        View::Unified => &cached.unified,
        View::Split => &cached.split,
    };
    let visible: Vec<_> = all_rows
        .iter()
        .copied()
        .filter(|row| {
            matches!(row, Row::File(_))
                || !collapsed.contains(&cached.document.files[row.file()].path)
        })
        .collect();
    let theme = CodeTheme::from_style(ui.style());
    let char_width = ui.fonts_mut(|fonts| fonts.glyph_width(&FontId::monospace(FONT_MONO), 'M'));
    let cell_width = cached.code_columns as f32 * char_width + 120.0;
    let width = ui.available_width().max(match view {
        View::Unified => cell_width.max(400.0),
        View::Split => (cell_width * 2.0).max(900.0),
    });
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::both()
            .id_salt("diff_code")
            .auto_shrink([false, false])
            .show_rows(ui, 24.0, visible.len(), |ui, range| {
                ui.set_min_width(width);
                for index in range {
                    let row = visible[index];
                    let file = &cached.document.files[row.file()];
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 24.0), Sense::hover());
                    match row {
                        Row::File(_) => file_header(ui, rect, file, &mut collapsed),
                        Row::Line(_, left, right) => {
                            let left = left.map(|index| &file.lines[index]);
                            let right = right.map(|index| &file.lines[index]);
                            if view == View::Unified
                                || left.is_some_and(|line| {
                                    matches!(line.kind, LineKind::Hunk | LineKind::Metadata)
                                })
                            {
                                code_cell(
                                    ui,
                                    rect,
                                    left,
                                    file,
                                    view == View::Unified,
                                    false,
                                    &theme,
                                );
                            } else {
                                let mid = rect.center().x;
                                code_cell(
                                    ui,
                                    Rect::from_min_max(rect.min, egui::pos2(mid, rect.max.y)),
                                    left,
                                    file,
                                    false,
                                    false,
                                    &theme,
                                );
                                code_cell(
                                    ui,
                                    Rect::from_min_max(egui::pos2(mid, rect.min.y), rect.max),
                                    right,
                                    file,
                                    false,
                                    true,
                                    &theme,
                                );
                                ui.painter().vline(
                                    mid,
                                    rect.y_range(),
                                    Stroke::new(1.0, palette().BORDER),
                                );
                            }
                        }
                    }
                }
            });
    });
    ui.ctx()
        .data_mut(|data| data.insert_temp(collapsed_id, collapsed));
}

fn counts(ui: &mut egui::Ui, additions: usize, deletions: usize) {
    ui.label(
        RichText::new(format!("+{additions}"))
            .monospace()
            .color(palette().SUCCESS),
    );
    ui.label(
        RichText::new(format!("−{deletions}"))
            .monospace()
            .color(palette().ERROR_FG),
    );
}

fn file_header(ui: &mut egui::Ui, rect: Rect, file: &DiffFile, collapsed: &mut BTreeSet<String>) {
    ui.painter().rect_filled(rect, 0, palette().SURFACE_RAISED);
    let mut child = ui.new_child(UiBuilder::new().max_rect(rect.shrink2(egui::vec2(SP_2, 0.0))));
    child.horizontal_centered(|ui| {
        let closed = collapsed.contains(&file.path);
        ui.label(if closed { "›" } else { "⌄" });
        if ui
            .add(
                egui::Label::new(RichText::new(&file.path).monospace().strong())
                    .sense(Sense::click()),
            )
            .on_hover_text("Collapse / expand file diff")
            .clicked()
        {
            if closed {
                collapsed.remove(&file.path);
            } else {
                collapsed.insert(file.path.clone());
            }
        }
        ui.label(
            RichText::new(file.status)
                .small()
                .color(palette().TEXT_MUTED),
        );
        counts(ui, file.additions, file.deletions);
    });
}

fn code_cell(
    ui: &mut egui::Ui,
    rect: Rect,
    line: Option<&DiffLine>,
    file: &DiffFile,
    unified: bool,
    new_side: bool,
    theme: &CodeTheme,
) {
    let p = palette();
    let kind = line.map_or(LineKind::Context, |line| line.kind);
    let background = match kind {
        LineKind::Addition => blend(p.SURFACE, p.SUCCESS, 0.10),
        LineKind::Deletion => blend(p.SURFACE, p.ERROR_FG, 0.10),
        LineKind::Hunk => p.SURFACE_RAISED,
        _ => p.SURFACE,
    };
    ui.painter().rect_filled(rect, 0, background);
    let Some(line) = line else {
        return;
    };
    let gutter_width = if unified { 112.0 } else { 64.0 };
    let gutter = Rect::from_min_max(rect.min, egui::pos2(rect.min.x + gutter_width, rect.max.y));
    let (sign, color) = match kind {
        LineKind::Addition => ("+", p.SUCCESS),
        LineKind::Deletion => ("−", p.ERROR_FG),
        _ => (" ", p.TEXT_MUTED),
    };
    if matches!(
        kind,
        LineKind::Context | LineKind::Addition | LineKind::Deletion
    ) {
        ui.painter()
            .rect_filled(gutter, 0, blend(background, color, 0.08));
        let number = |number: Option<usize>| number.map(|n| n.to_string()).unwrap_or_default();
        let numbers = if unified {
            format!("{:>5} {:>5} {sign}", number(line.old), number(line.new))
        } else {
            format!(
                "{:>5} {sign}",
                number(if new_side { line.new } else { line.old })
            )
        };
        ui.painter().text(
            gutter.right_center() - egui::vec2(4.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            numbers,
            FontId::monospace(FONT_MONO),
            color,
        );
    }
    let content = Rect::from_min_max(egui::pos2(gutter.max.x + SP_2, rect.min.y + 5.0), rect.max);
    let mut child = ui.new_child(UiBuilder::new().max_rect(content));
    let ui = &mut child;
    {
        ui.set_clip_rect(ui.clip_rect().intersect(content));
        let extension = file.path.rsplit('.').next().unwrap_or("txt");
        if matches!(kind, LineKind::Hunk | LineKind::Metadata) {
            ui.add(
                egui::Label::new(RichText::new(&line.text).monospace().color(p.TEXT_MUTED))
                    .selectable(true)
                    .extend(),
            );
        } else {
            let mut job = highlight(ui.ctx(), ui.style(), theme, &line.text, extension);
            for section in &mut job.sections {
                section.format.font_id = FontId::monospace(FONT_MONO);
            }
            ui.add(egui::Label::new(job).selectable(true).extend());
        }
    }
}

fn blend(base: Color32, tint: Color32, amount: f32) -> Color32 {
    let mix = |a: u8, b: u8| (f32::from(a) * (1.0 - amount) + f32::from(b) * amount).round() as u8;
    Color32::from_rgb(
        mix(base.r(), tint.r()),
        mix(base.g(), tint.g()),
        mix(base.b(), tint.b()),
    )
}

fn rows(document: &DiffDocument, view: View) -> Vec<Row> {
    let mut rows = Vec::new();
    for (file_index, file) in document.files.iter().enumerate() {
        rows.push(Row::File(file_index));
        let mut index = 0;
        while index < file.lines.len() {
            let line = &file.lines[index];
            if view == View::Unified || matches!(line.kind, LineKind::Hunk | LineKind::Metadata) {
                rows.push(Row::Line(file_index, Some(index), None));
                index += 1;
            } else if line.kind == LineKind::Context {
                rows.push(Row::Line(file_index, Some(index), Some(index)));
                index += 1;
            } else {
                let start = index;
                while index < file.lines.len() && file.lines[index].kind == LineKind::Deletion {
                    index += 1;
                }
                let added_start = index;
                while index < file.lines.len() && file.lines[index].kind == LineKind::Addition {
                    index += 1;
                }
                let deleted = added_start - start;
                let added = index - added_start;
                for offset in 0..deleted.max(added) {
                    rows.push(Row::Line(
                        file_index,
                        (offset < deleted).then_some(start + offset),
                        (offset < added).then_some(added_start + offset),
                    ));
                }
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_pairs_replacements_and_keeps_unpaired_additions() {
        let doc =
            DiffDocument::parse("diff --git a/a b/a\n@@ -1,2 +1,3 @@\n same\n-old\n+new\n+extra\n");
        assert_eq!(
            rows(&doc, View::Split),
            vec![
                Row::File(0),
                Row::Line(0, Some(0), None),
                Row::Line(0, Some(1), Some(1)),
                Row::Line(0, Some(2), Some(3)),
                Row::Line(0, None, Some(4)),
            ]
        );
        assert_eq!(rows(&doc, View::Unified).len(), 6);
    }
}
