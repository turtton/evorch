//! Diff pane stub.

use crate::diff::DiffModel;

pub fn diff_pane(ui: &mut egui::Ui, _diff: &DiffModel) {
    ui.heading("Diff");
    ui.label("Working tree and branch diff");
}
