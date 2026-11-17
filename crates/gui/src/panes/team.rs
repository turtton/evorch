use runtime::{
    RunId,
    team::{ClaimState, TeamTask},
};

use crate::theme::{
    text::{h3, muted},
    tokens::{SP_1, palette},
    widgets::surface_frame,
};

pub fn team_pane(ui: &mut egui::Ui, teams: &[(RunId, Vec<TeamTask>)]) {
    ui.label(h3("Team"));
    if teams.is_empty() {
        ui.label(muted("Team mode is disabled or no team has started."));
        return;
    }
    for (coordinator, tasks) in teams {
        ui.push_id(coordinator, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(muted("Coordinator"));
                ui.label(coordinator.to_string());
            });
            for task in tasks {
                ui.push_id(&task.spec.id, |ui| {
                    surface_frame(palette().SURFACE).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&task.spec.id)
                                        .strong()
                                        .color(palette().TEXT),
                                )
                                .wrap(),
                            );
                            match &task.state {
                                ClaimState::Ready => {
                                    ui.label("Ready");
                                }
                                ClaimState::Claimed(lease) => {
                                    ui.label("Claimed");
                                    ui.label(&lease.owner_id).on_hover_text(format!(
                                        "Assigned execution · Generation {} · lease deadline {} ms",
                                        lease.generation, lease.expires_at
                                    ));
                                }
                                ClaimState::Complete => {
                                    ui.label("Complete");
                                }
                            }
                        });
                        if !task.spec.paths.is_empty() {
                            ui.label(muted("Paths").strong());
                            for path in &task.spec.paths {
                                ui.add(egui::Label::new(path.display().to_string()).wrap());
                            }
                        }
                    });
                    ui.add_space(SP_1);
                });
            }
        });
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs(1));
}
