//! Merge approval pane stub.

use crate::model::commands::MergeApprovalModel;

pub fn merge_pane(ui: &mut egui::Ui, _merge: &MergeApprovalModel) {
    ui.heading("Merge");
    ui.label("Merge approval status");
}
