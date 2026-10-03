use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRunPhase, TrustState};

use crate::theme::widgets::pane_root;

mod projects;
pub(crate) mod threads;

const UI_STATE_ID: &str = "sidebar-ui-state";

#[derive(Clone, Default)]
struct SidebarUiState {
    project_path: String,
    error: Option<String>,
    picker_busy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarAction {
    SelectProject(ProjectId),
    SetPrimaryProject(Option<ProjectId>),
    AddProject(PathBuf),
    BrowseForProject,
    CreateThread(String),
    ForkThread(ThreadId),
    SwitchThread(ThreadId),
    TogglePin(ThreadId),
    ToggleArchive(ThreadId),
    SetTrust { path: PathBuf, trust: TrustState },
}

pub fn sidebar_pane(
    ui: &mut egui::Ui,
    sidebar: &SidebarState,
    phases: &BTreeMap<String, ThreadRunPhase>,
    telemetry: &crate::model::telemetry::TelemetryOverlay,
    question_threads: &BTreeSet<ThreadId>,
) -> Option<SidebarAction> {
    let id = egui::Id::new(UI_STATE_ID);
    let mut pane_state = ui
        .ctx()
        .data_mut(|data| data.get_temp::<SidebarUiState>(id))
        .unwrap_or_default();
    let mut action = None;

    pane_root(ui, "Projects", |ui| {
        egui::ScrollArea::vertical()
            .id_salt("sidebar-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The pane itself is the surface; nesting another frame only adds borders.
                let selected = selected_project(sidebar);
                projects::render(ui, sidebar, selected, &mut pane_state, &mut action);
                if let Some(project) = selected {
                    threads::render(
                        ui,
                        sidebar,
                        project,
                        phases,
                        telemetry,
                        question_threads,
                        &mut action,
                    );
                }
            });
    });

    ui.ctx().data_mut(|data| data.insert_temp(id, pane_state));
    action
}

pub fn set_sidebar_error(ctx: &egui::Context, error: Option<String>) {
    let id = egui::Id::new(UI_STATE_ID);
    ctx.data_mut(|data| {
        let state = data.get_temp_mut_or_default::<SidebarUiState>(id);
        state.error = error;
    });
}

pub fn set_picker_busy(ctx: &egui::Context, busy: bool) {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<SidebarUiState>(egui::Id::new(UI_STATE_ID))
            .picker_busy = busy;
    });
}

fn selected_project(sidebar: &SidebarState) -> Option<&workspace_ui::ProjectRecord> {
    let selected = sidebar.selected_project.as_ref()?;
    sidebar
        .projects
        .iter()
        .find(|project| &project.id == selected)
}
