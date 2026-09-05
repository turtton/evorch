//! Tasks ペインの描画。

use crate::model::tasks::{AgentRunSource, TasksModel};
use crate::theme::text::muted;
use crate::theme::tokens::{SP_1, TEXT, agent_phase_color};
use crate::theme::widgets::status_dot;

/// タスク一覧を Grid で描画します。
pub fn tasks_pane<S: AgentRunSource>(ui: &mut egui::Ui, model: &TasksModel<S>) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("tasks_grid")
            .spacing([SP_1, SP_1])
            .show(ui, |ui| {
                ui.label(muted("Name").strong());
                ui.label(muted("Run ID").strong());
                ui.label(muted("Role").strong());
                ui.label(muted("Status").strong());
                ui.label(muted("Model").strong());
                ui.end_row();

                for row in model.rows() {
                    ui.label(egui::RichText::new(&row.name).color(TEXT));
                    ui.label(egui::RichText::new(row.run_id.to_string()).color(TEXT));
                    ui.label(egui::RichText::new(&row.role).color(TEXT));
                    ui.horizontal(|ui| {
                        status_dot(ui, agent_phase_color(row.status));
                        ui.label(format!("{:?}", row.status));
                    });
                    ui.label(egui::RichText::new(&row.model).color(TEXT));
                    ui.end_row();
                }
            });
    });
}
