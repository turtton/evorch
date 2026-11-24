use egui::Ui;
use workspace_ui::{ProjectRecord, SidebarState, SidebarView};

use crate::panes::sidebar_widgets::{self, ProjectRowView};
use crate::theme::tokens::palette;
use crate::theme::widgets::empty_state;

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
            let limit = sidebar_widgets::family_limit_id(&project.id);
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
    let count = sidebar
        .threads
        .iter()
        .filter(|thread| thread.project_id == project.id && !thread.archived)
        .count();
    let root = project.repo_root.display().to_string();
    let clicks = sidebar_widgets::project_row(
        ui,
        ProjectRowView {
            name: &project.name,
            hover_root: Some(&root),
            selected: selected_project,
            open,
            count,
            search: true,
            settings: true,
        },
    );
    if clicks.toggle {
        *action = Some(SidebarAction::SetProjectCollapsed {
            project: project.id.clone(),
            collapsed: open,
        });
    }
    if clicks.settings {
        *action = Some(SidebarAction::OpenProjectSettings(project.id.clone()));
    }
    if clicks.new_thread {
        *action = Some(SidebarAction::CreateThreadIn(project.id.clone()));
    }
    if clicks.search {
        *action = Some(SidebarAction::OpenThreadSearch(project.id.clone()));
    }
    if clicks.select {
        *action = Some(SidebarAction::SelectProject(project.id.clone()));
    }
}
