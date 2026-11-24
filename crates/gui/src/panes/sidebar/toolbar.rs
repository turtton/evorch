//! The fixed strip above the project list: add a project, sort, fold everything.

use egui::Ui;
use workspace_ui::{SidebarState, SidebarView};

use crate::panes::sidebar_widgets::{
    LEGACY_SORT_CHOICES, ToolbarAction, ToolbarView, sidebar_toolbar,
};

use super::SidebarAction;

/// Draws the strip across the full width and records the operator's choice in `action`.
pub fn render(
    ui: &mut Ui,
    sidebar: &SidebarState,
    view: &SidebarView,
    action: &mut Option<SidebarAction>,
) {
    let chosen = sidebar_toolbar(
        ui,
        ToolbarView {
            has_projects: !sidebar.projects.is_empty(),
            any_open: view.any_open(sidebar),
            sort: view.project_sort,
            choices: &LEGACY_SORT_CHOICES,
        },
    );
    if let Some(chosen) = chosen {
        *action = Some(match chosen {
            ToolbarAction::AddProject => SidebarAction::OpenAddProject,
            ToolbarAction::Sort(sort) => SidebarAction::SetProjectSort(sort),
            ToolbarAction::SetAllCollapsed(fold) => SidebarAction::SetAllProjectsCollapsed(fold),
        });
    }
}
