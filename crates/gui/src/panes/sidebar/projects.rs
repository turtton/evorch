use egui::{Align, Layout, RichText, Sense, Ui};
use workspace_ui::{SidebarState, TrustState};

use crate::theme::icons;
use crate::theme::text::{medium, muted};
use crate::theme::tokens::{FONT_ICON, SP_1, SP_2, palette};
use crate::theme::widgets::{
    badge, compact_row, empty_state, ghost, icon_button_rich, icon_text, primary_button, row_title,
};

use super::{SidebarAction, SidebarUiState};

/// Horizontal offset of row titles: row padding + icon + item spacing.
pub(super) const ROW_TEXT_INDENT: f32 = SP_2 + FONT_ICON + SP_2;

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
            ui.spacing_mut().item_spacing.x = SP_2;
            let (icon, color) = if selected_project {
                (icons::FOLDER_OPEN, palette().ACCENT)
            } else {
                (icons::FOLDER_SIMPLE, palette().TEXT_MUTED)
            };
            ui.label(icon_text(icon).color(color));
            let count = sidebar
                .threads
                .iter()
                .filter(|thread| thread.project_id == project.id)
                .count();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = SP_1;
                let primary = sidebar.primary_project.as_ref() == Some(&project.id);
                let star_label = if primary {
                    "Clear primary project"
                } else {
                    "Set as primary project"
                };
                let star = if primary {
                    icons::filled(ui, icons::STAR)
                        .size(FONT_ICON)
                        .color(palette().WARNING_FG)
                } else {
                    icon_text(icons::STAR).color(palette().TEXT_MUTED)
                };
                if icon_button_rich(ui, star, star_label).clicked() {
                    *action = Some(SidebarAction::SetPrimaryProject(
                        (!primary).then(|| project.id.clone()),
                    ));
                }
                if count > 0 {
                    ui.label(muted(count.to_string()));
                }
                let title = if selected_project {
                    medium(&project.name).color(palette().TEXT)
                } else {
                    RichText::new(&project.name).color(palette().TEXT)
                };
                if row_title(ui, title).clicked() {
                    *action = Some(SidebarAction::SelectProject(project.id.clone()));
                }
            });
        });
        let path = project.repo_root.display().to_string();
        ui.horizontal(|ui| {
            ui.add_space(ROW_TEXT_INDENT);
            ui.add(egui::Label::new(muted(&path)).truncate())
                .on_hover_text(&path);
        });
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
        let browse = ui.add_enabled(
            !pane_state.picker_busy,
            ghost(icons::with_icon(icons::FOLDER_OPEN, "Browse…")),
        );
        browse.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, !pane_state.picker_busy, "Browse…")
        });
        if browse.clicked() {
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
