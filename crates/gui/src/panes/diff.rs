//! Diff tab pane: working tree / branch unified diff viewer.

use crate::diff::{DiffMode, DiffModel, DiffState};
use crate::theme::tokens::palette;
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
    surface_frame(palette().SURFACE).show(ui, |ui| match diff.state(&mode) {
        DiffState::Idle => {
            empty_state(
                ui,
                "No diff loaded",
                "Choose Working tree or Branch vs main.",
                None,
            );
        }
        DiffState::Empty => {
            empty_state(
                ui,
                "no changes",
                "Edit files, then choose Working tree or Branch vs main to refresh.",
                None,
            );
        }
        DiffState::Loading => {
            ui.label(match mode {
                DiffMode::WorkingTree => "Loading working tree diff…",
                DiffMode::Branch => "Loading branch diff against main…",
            });
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
            ui.label(egui::RichText::new(format!("error: {message}")).color(palette().ERROR_FG));
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
