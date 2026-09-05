use egui::{Sense, Ui};
use workspace_ui::{SidebarState, TrustState};

use crate::theme::tokens::{ACCENT, ERROR_FG, ROW_COMPACT, SP_1, SP_2, SURFACE_RAISED, TEXT_MUTED};
use crate::theme::widgets::{badge, compact_row, empty_state, primary_button, status_dot};

use super::{SidebarAction, SidebarUiState};

pub fn render(
    ui: &mut Ui,
    sidebar: &SidebarState,
    selected: Option<&workspace_ui::ProjectRecord>,
    pane_state: &mut SidebarUiState,
    action: &mut Option<SidebarAction>,
) {
    if sidebar.projects.is_empty() {
        empty_state(
            ui,
            "No projects yet",
            "Add a repository root to start orchestrating.",
            None,
        );
    }

    for project in &sidebar.projects {
        let selected_project = selected.map(|p| &p.id) == Some(&project.id);
        compact_row(ui, selected_project, |ui| {
            let dot_color = if selected_project { ACCENT } else { TEXT_MUTED };
            status_dot(ui, dot_color);
            let count = sidebar
                .threads
                .iter()
                .filter(|thread| thread.project_id == project.id)
                .count();
            let right_width = if count > 0 { 40.0 } else { 0.0 };
            let title_width = (ui.available_width() - right_width).max(40.0);
            let title_response = ui.add_sized(
                egui::vec2(title_width, ROW_COMPACT),
                egui::Label::new(&project.name)
                    .truncate()
                    .sense(Sense::click()),
            );
            if title_response.clicked() {
                *action = Some(SidebarAction::SelectProject(project.id.clone()));
            }
            if count > 0 {
                ui.add_space(SP_1);
                badge(ui, count.to_string(), TEXT_MUTED, SURFACE_RAISED);
            }
        });
    }

    ui.add_space(SP_2);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut pane_state.project_path)
                .hint_text("Project path")
                .desired_width(140.0),
        );
        if primary_button(ui, "Add project").clicked() && !pane_state.project_path.trim().is_empty()
        {
            *action = Some(SidebarAction::AddProject(std::path::PathBuf::from(
                pane_state.project_path.trim(),
            )));
            pane_state.project_path.clear();
        }
    });

    if let Some(error) = &pane_state.error {
        ui.colored_label(ERROR_FG, error);
    }

    if let Some(project) = selected {
        ui.separator();
        ui.label("Allowed directories");
        for directory in &project.allowed_directories {
            ui.label(directory.path.display().to_string());
            ui.horizontal(|ui| match directory.trust {
                TrustState::Approved => {
                    ui.label("trusted");
                }
                TrustState::Unapproved => {
                    ui.label("untrusted");
                    if ui.button("Trust").clicked() {
                        *action = Some(SidebarAction::SetTrust {
                            path: directory.path.clone(),
                            trust: TrustState::Approved,
                        });
                    }
                }
            });
        }
    }
}
