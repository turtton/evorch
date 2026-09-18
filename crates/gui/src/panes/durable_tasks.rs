use crate::model::durable_tasks::DurableTasksModel;
use crate::theme::{
    text::muted,
    tokens::{SP_1, SP_2, palette},
    widgets::{badge, empty_state, pane_root},
};

pub fn durable_tasks_pane(ui: &mut egui::Ui, model: &DurableTasksModel) {
    pane_root(ui, "Durable Tasks", |ui| {
        if model.rows().next().is_none() {
            empty_state(
                ui,
                "No durable tasks yet",
                "Task progress and the last valid artifact will appear here.",
                None,
            );
            return;
        }
        egui::ScrollArea::both()
            .id_salt("durable_tasks_scroll")
            .show(ui, |ui| {
                egui::Grid::new("durable_tasks_grid")
                    .spacing([SP_2, SP_1])
                    .striped(true)
                    .show(ui, |ui| {
                        for header in [
                            "Task",
                            "Status",
                            "Retry",
                            "Last valid artifact",
                            "Run",
                            "Progress",
                        ] {
                            ui.label(muted(header).strong());
                        }
                        ui.end_row();
                        for row in model.rows() {
                            ui.monospace(&row.id).on_hover_text(&row.title);
                            ui.label(row.status.as_str());
                            if row.attempt > 0 {
                                badge(
                                    ui,
                                    format!("retry {}", row.attempt),
                                    palette().TEXT,
                                    palette().SURFACE,
                                );
                            } else {
                                ui.label(muted("-"));
                            }
                            ui.monospace(row.last_artifact.as_deref().unwrap_or("-"));
                            ui.monospace(row.run_id.as_deref().unwrap_or("-"));
                            ui.label(&row.detail);
                            ui.end_row();
                        }
                    });
            });
    });
}
