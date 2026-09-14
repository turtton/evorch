use runtime::{
    RunId,
    team::{ClaimState, TeamTask},
};

pub fn team_pane(ui: &mut egui::Ui, teams: &[(RunId, Vec<TeamTask>)]) {
    ui.label(crate::theme::text::h3("Team"));
    if teams.is_empty() {
        ui.label(crate::theme::text::muted(
            "Team mode is disabled or no team has started.",
        ));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("team_claims")
        .max_height(240.0)
        .show(ui, |ui| {
            for (coordinator, tasks) in teams {
                ui.push_id(coordinator.get(), |ui| {
                    ui.label(crate::theme::text::badge(format!(
                        "Coordinator {coordinator}"
                    )));
                    egui::Grid::new("claims").striped(true).show(ui, |ui| {
                        ui.label(crate::theme::text::badge("Task").strong());
                        ui.label(crate::theme::text::badge("State").strong());
                        ui.label(crate::theme::text::badge("Owner").strong());
                        ui.end_row();
                        for task in tasks {
                            ui.monospace(&task.spec.id);
                            match &task.state {
                                ClaimState::Ready => {
                                    ui.label("Ready");
                                    ui.label("—");
                                }
                                ClaimState::Claimed(lease) => {
                                    ui.label("Claimed");
                                    ui.monospace(&lease.owner_id).on_hover_text(format!(
                                        "Generation {} · lease deadline {} ms",
                                        lease.generation, lease.expires_at
                                    ));
                                }
                                ClaimState::Complete => {
                                    ui.label("Complete");
                                    ui.label("—");
                                }
                            }
                            ui.end_row();
                        }
                    });
                });
            }
        });
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs(1));
}
