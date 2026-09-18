use egui::{Align, Layout, Sense, Ui};
use workspace_ui::{SidebarState, TrustState};

use crate::theme::tokens::{FONT_SMALL, ROW_DENSE, SP_2, palette};
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
            let dot_color = if selected_project {
                palette().ACCENT
            } else {
                palette().TEXT_MUTED
            };
            status_dot(ui, dot_color);
            let count = sidebar
                .threads
                .iter()
                .filter(|thread| thread.project_id == project.id)
                .count();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let primary = sidebar.primary_project.as_ref() == Some(&project.id);
                let star_label = if primary {
                    "Clear primary project"
                } else {
                    "Set as primary project"
                };
                let star_response = ui
                    .small_button(if primary { "★" } else { "☆" })
                    .on_hover_text(star_label);
                star_response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, star_label)
                });
                if star_response.clicked() {
                    *action = Some(SidebarAction::SetPrimaryProject(
                        (!primary).then(|| project.id.clone()),
                    ));
                }
                if count > 0 {
                    badge(
                        ui,
                        count.to_string(),
                        palette().TEXT_MUTED,
                        palette().SURFACE_RAISED,
                    );
                }
                let title_response = ui.add_sized(
                    egui::vec2(ui.available_width().max(0.0), ROW_DENSE),
                    egui::Label::new(&project.name)
                        .truncate()
                        .halign(Align::LEFT)
                        .sense(Sense::click()),
                );
                if title_response.clicked() {
                    *action = Some(SidebarAction::SelectProject(project.id.clone()));
                }
            });
        });
        let path = project.repo_root.display().to_string();
        ui.add(
            egui::Label::new(
                egui::RichText::new(&path)
                    .size(FONT_SMALL)
                    .color(palette().TEXT_MUTED),
            )
            .truncate(),
        )
        .on_hover_text(&path);
    }

    ui.add_space(SP_2);
    let path_input = ui.add(
        egui::TextEdit::singleline(&mut pane_state.project_path)
            .hint_text("Project path (~ allowed)")
            .desired_width(ui.available_width()),
    );
    path_input.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Project path (~ allowed)")
    });
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!pane_state.picker_busy, egui::Button::new("Browse…"))
            .clicked()
        {
            *action = Some(SidebarAction::BrowseForProject);
        }
        if primary_button(ui, "Add project").clicked() && !pane_state.project_path.trim().is_empty()
        {
            *action = Some(SidebarAction::AddProject(std::path::PathBuf::from(
                pane_state.project_path.trim(),
            )));
            pane_state.project_path.clear();
        }
    });

    if let Some(error) = &pane_state.error {
        ui.colored_label(palette().ERROR_FG, error);
    }

    if let Some(project) = selected
        && !project.allowed_directories.is_empty()
    {
        ui.separator();
        egui::CollapsingHeader::new(format!(
            "Allowed directories ({})",
            project.allowed_directories.len()
        ))
        .id_salt((&project.id, "allowed-directories"))
        .default_open(false)
        .show(ui, |ui| {
            for directory in &project.allowed_directories {
                let path = directory.path.display().to_string();
                if ui
                    .add(egui::Label::new(&path).truncate().sense(Sense::click()))
                    .on_hover_text(format!("{path}\nClick to copy"))
                    .clicked()
                {
                    ui.ctx().copy_text(path);
                }
                ui.horizontal(|ui| match directory.trust {
                    TrustState::Approved => {
                        badge(
                            ui,
                            "trusted",
                            palette().TEXT_MUTED,
                            palette().SURFACE_RAISED,
                        );
                    }
                    TrustState::Unapproved => {
                        badge(ui, "untrusted", palette().WARNING, palette().SURFACE_RAISED);
                        if ui.button("Trust").clicked() {
                            *action = Some(SidebarAction::SetTrust {
                                path: directory.path.clone(),
                                trust: TrustState::Approved,
                            });
                        }
                    }
                });
            }
        });
    }
}
