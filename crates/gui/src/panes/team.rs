use runtime::{
    RunId,
    team::{ClaimState, TeamTask},
};

use super::tasks::{TasksAction, task_label};
use crate::theme::{
    text::{h3, muted},
    tokens::{SP_1, palette},
    widgets::surface_frame,
};

pub fn team_pane(ui: &mut egui::Ui, teams: &[(RunId, Vec<TeamTask>)]) {
    let _ = team_tasks_pane(ui, teams, None);
}

pub fn team_tasks_pane(
    ui: &mut egui::Ui,
    teams: &[(RunId, Vec<TeamTask>)],
    selected_task: Option<&str>,
) -> Option<TasksAction> {
    let mut action = None;
    ui.label(h3("Team"));
    if teams.is_empty() {
        ui.label(muted("Team mode is disabled or no team has started."));
        return None;
    }
    for (coordinator, tasks) in teams {
        ui.push_id(coordinator.get(), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(muted("Coordinator"));
                if ui.link(coordinator.to_string()).clicked() {
                    action = Some(TasksAction::OpenRun(coordinator.to_string()));
                }
            });
            for task in tasks {
                let key = format!("team:{coordinator}:{}", task.spec.id);
                let fill = if selected_task == Some(key.as_str()) {
                    palette().ACTIVE_ROW
                } else {
                    palette().SURFACE
                };
                ui.push_id(&task.spec.id, |ui| {
                    surface_frame(fill).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            task_label(ui, &task.spec.id, &task.spec.id, &key, selected_task);
                            match &task.state {
                                ClaimState::Ready => { ui.label("Ready"); }
                                ClaimState::Claimed(lease) => {
                                    ui.label("Claimed");
                                    if ui.link(&lease.owner_id).on_hover_text(format!(
                                        "Open assigned execution · Generation {} · lease deadline {} ms",
                                        lease.generation, lease.expires_at
                                    )).clicked() {
                                        action = Some(TasksAction::OpenRun(lease.owner_id.clone()));
                                    }
                                }
                                ClaimState::Complete => { ui.label("Complete"); }
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
    action
}
