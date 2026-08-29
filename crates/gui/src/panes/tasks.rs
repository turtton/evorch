//! Tasks ペインの描画。

use crate::model::tasks::{AgentRunSource, TasksModel};

/// タスク一覧を Grid で描画します。
pub fn tasks_pane<S: AgentRunSource>(ui: &mut egui::Ui, model: &TasksModel<S>) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("tasks_grid").show(ui, |ui| {
            ui.label("Name");
            ui.label("Run ID");
            ui.label("Role");
            ui.label("Status");
            ui.label("Model");
            ui.end_row();

            for row in model.rows() {
                ui.label(&row.name);
                ui.label(row.run_id.to_string());
                ui.label(&row.role);
                ui.label(format!("{:?}", row.status));
                ui.label(&row.model);
                ui.end_row();
            }
        });
    });
}
