//! Diff tab pane: working tree / branch unified diff viewer.

use crate::diff::{DiffMode, DiffModel, DiffState};
use crate::theme::text::muted;
use crate::theme::tokens::{ERROR_FG, SURFACE};
use crate::theme::widgets::{empty_state, surface_frame};

/// Diff tab を描画し、click された取得要求を返す。
pub fn diff_pane(ui: &mut egui::Ui, diff: &DiffModel) -> Option<DiffMode> {
    let mode_id = ui.id().with("selected_mode");
    let mut requested = None;
    ui.horizontal(|ui| {
        if ui.button("Working tree").clicked() {
            requested = Some(DiffMode::WorkingTree);
        }
        if ui.button("Branch vs main").clicked() {
            requested = Some(DiffMode::Branch);
        }
    });
    if let Some(mode) = &requested {
        ui.ctx()
            .data_mut(|data| data.insert_temp(mode_id, mode.clone()));
    }
    let mode = ui
        .ctx()
        .data_mut(|data| data.get_temp::<DiffMode>(mode_id))
        .unwrap_or(DiffMode::WorkingTree);
    surface_frame(SURFACE).show(ui, |ui| match diff.state(&mode) {
        DiffState::Idle => {
            empty_state(
                ui,
                "No diff loaded",
                "Choose Working tree or Branch vs main.",
                None,
            );
        }
        DiffState::Empty => {
            ui.label(muted("no changes"));
        }
        DiffState::Loading => {
            ui.label("loading…");
        }
        DiffState::Ready { text } => diff_body(ui, text),
        DiffState::Truncated {
            text,
            total_bytes,
            cap,
        } => {
            ui.label(format!("truncated: showing {cap} of {total_bytes} bytes"));
            diff_body(ui, text);
        }
        DiffState::Error { message } => {
            ui.label(egui::RichText::new(format!("error: {message}")).color(ERROR_FG));
        }
    });
    requested
}

fn diff_body(ui: &mut egui::Ui, text: &str) {
    // The body is a single label: existing tests assert the full multi-line text
    // verbatim (e.g. "first line\nsecond line"), so per-line tinting is not applied.
    egui::ScrollArea::both().show(ui, |ui| {
        ui.add(
            egui::Label::new(egui::RichText::new(text).monospace())
                .wrap_mode(egui::TextWrapMode::Extend),
        );
    });
}
