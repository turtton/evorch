use egui::{Align, Layout, RichText, Sense, Ui};
use workspace_ui::{ProjectRecord, SidebarState, SidebarView};

use crate::theme::icons;
use crate::theme::text::{medium, muted};
use crate::theme::tokens::{FONT_SMALL, ROW_DENSE, SP_1, SP_2, palette};
use crate::theme::widgets::{compact_row, empty_state, icon_button, icon_text, row_title};

use super::{SidebarAction, SidebarUiState};

/// Lists the projects in `order` (a view over `sidebar.projects`); each expands to its
/// threads unless `view` has it folded.
pub fn render(
    ui: &mut Ui,
    sidebar: &SidebarState,
    order: &[&ProjectRecord],
    view: &SidebarView,
    pane_state: &SidebarUiState,
    action: &mut Option<SidebarAction>,
    mut threads: impl FnMut(&mut Ui, &ProjectRecord, &mut Option<SidebarAction>),
) {
    let selected = super::selected_project(sidebar);
    if sidebar.projects.is_empty() {
        // The toolbar above owns the one "Add project" control.
        empty_state(
            ui,
            "No projects yet",
            "Add a repository root to start orchestrating.",
            None,
        );
    }

    for project in order {
        let selected_project = selected.map(|p| &p.id) == Some(&project.id);
        let open = !view.is_collapsed(&project.id);
        ui.push_id(&project.id, |ui| {
            project_row(ui, sidebar, project, selected_project, open, action);
        });
        if open {
            ui.push_id(("threads", &project.id), |ui| threads(ui, project, action));
        } else {
            let limit = super::threads::family_limit_id(&project.id);
            ui.ctx().data_mut(|data| data.remove::<usize>(limit));
        }
    }

    if let Some(error) = &pane_state.error {
        crate::theme::widgets::reason_label(ui, palette().ERROR_FG, error);
    }
}

fn project_row(
    ui: &mut Ui,
    sidebar: &SidebarState,
    project: &ProjectRecord,
    selected_project: bool,
    open: bool,
    action: &mut Option<SidebarAction>,
) {
    compact_row(ui, selected_project, |ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        let (rect, toggle) = ui.allocate_exact_size(egui::vec2(16.0, ROW_DENSE), Sense::click());
        if toggle.clicked() {
            *action = Some(SidebarAction::SetProjectCollapsed {
                project: project.id.clone(),
                collapsed: open,
            });
        }
        toggle.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                true,
                format!(
                    "{} threads of {}",
                    if open { "Collapse" } else { "Expand" },
                    project.name
                ),
            )
        });
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            if open {
                icons::CARET_DOWN
            } else {
                icons::CARET_RIGHT
            },
            egui::FontId::proportional(FONT_SMALL),
            if toggle.hovered() {
                palette().TEXT
            } else {
                palette().TEXT_MUTED
            },
        );
        let (icon, color) = if selected_project {
            (icons::FOLDER_OPEN, palette().ACCENT)
        } else {
            (icons::FOLDER_SIMPLE, palette().TEXT_MUTED)
        };
        ui.label(icon_text(icon).color(color));
        let count = sidebar
            .threads
            .iter()
            .filter(|thread| thread.project_id == project.id && !thread.archived)
            .count();
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = SP_1;
            if icon_button(ui, icons::GEAR_SIX, "Project settings").clicked() {
                *action = Some(SidebarAction::OpenProjectSettings(project.id.clone()));
            }
            let new_thread = format!("New thread in {}", project.name);
            if icon_button(ui, icons::PLUS, &new_thread).clicked() {
                *action = Some(SidebarAction::CreateThreadIn(project.id.clone()));
            }
            if count > 0 {
                ui.label(muted(count.to_string()));
            }
            // Drawn after the count in this right-to-left layout, so it sits to
            // its left on screen. It stays for a project without threads.
            let search = format!("Search threads in {}", project.name);
            if icon_button(ui, icons::MAGNIFYING_GLASS, &search).clicked() {
                *action = Some(SidebarAction::OpenThreadSearch(project.id.clone()));
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
