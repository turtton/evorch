use runtime::{
    RunId,
    team::{ClaimState, TeamTask},
};
use workbench_protocol::namespaces::teams::TeamTaskState;

use super::team_widgets::{COORDINATOR, EMPTY, HEADING, TaskCardView, owner_tip, task_card};
use crate::theme::text::{h3, muted};

pub fn team_pane(ui: &mut egui::Ui, teams: &[(RunId, Vec<TeamTask>)]) {
    ui.label(h3(HEADING));
    if teams.is_empty() {
        ui.label(muted(EMPTY));
        return;
    }
    for (coordinator, tasks) in teams {
        ui.push_id(coordinator, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(muted(COORDINATOR));
                ui.label(coordinator.to_string());
            });
            for task in tasks {
                ui.push_id(&task.spec.id, |ui| {
                    let (state, owner, tip) = match &task.state {
                        ClaimState::Ready => (TeamTaskState::Ready, None, None),
                        ClaimState::Claimed(lease) => (
                            TeamTaskState::Claimed,
                            Some(lease.owner_id.as_str()),
                            Some(owner_tip(lease.generation, Some(lease.expires_at))),
                        ),
                        ClaimState::Complete => (TeamTaskState::Complete, None, None),
                    };
                    let paths = task
                        .spec
                        .paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>();
                    task_card(
                        ui,
                        &TaskCardView {
                            title: &task.spec.id,
                            state,
                            owner,
                            owner_tip: tip,
                            paths: &paths,
                        },
                        |_| {},
                    );
                });
            }
        });
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs(1));
}
