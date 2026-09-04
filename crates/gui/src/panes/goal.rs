//! Goal submission pane stub.

use crate::model::commands::GoalFormModel;

pub fn goal_pane(ui: &mut egui::Ui, _goal: &GoalFormModel) {
    ui.heading("Goal");
    ui.label("Goal submission form");
}
