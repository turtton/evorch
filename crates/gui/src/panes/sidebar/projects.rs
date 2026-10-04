use egui::{Align, Layout, RichText, Ui};
use workspace_ui::SidebarState;

use crate::theme::icons;
use crate::theme::text::{medium, muted};
use crate::theme::tokens::{FONT_ICON, SP_1, SP_2, palette};
use crate::theme::widgets::{
    compact_row, empty_state, ghost_icon_button, icon_button, icon_button_rich, icon_text,
    row_title,
};

use super::{SidebarAction, SidebarUiState};

pub fn render(
    ui: &mut Ui,
    sidebar: &SidebarState,
    selected: Option<&workspace_ui::ProjectRecord>,
    pane_state: &SidebarUiState,
    action: &mut Option<SidebarAction>,
) {
    if sidebar.projects.is_empty()
        && empty_state(
            ui,
            "No projects yet",
            "Add a repository root to start orchestrating.",
            Some("Add project"),
        )
    {
        *action = Some(SidebarAction::OpenAddProject);
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
                if icon_button(ui, icons::GEAR_SIX, "Project settings").clicked() {
                    *action = Some(SidebarAction::OpenProjectSettings(project.id.clone()));
                }
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
                if row_title(ui, title)
                    .on_hover_text(project.repo_root.display().to_string())
                    .clicked()
                {
                    *action = Some(SidebarAction::SelectProject(project.id.clone()));
                }
            });
        });
    }

    if !sidebar.projects.is_empty() {
        ui.add_space(SP_1);
        if ghost_icon_button(ui, icons::PLUS, "Add project").clicked() {
            *action = Some(SidebarAction::OpenAddProject);
        }
    }

    if let Some(error) = &pane_state.error {
        ui.colored_label(palette().ERROR_FG, error);
    }
}
