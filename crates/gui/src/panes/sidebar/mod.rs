use std::collections::{BTreeMap, BTreeSet};

use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRunPhase};

use crate::theme::widgets::pane_root;

mod projects;
pub(crate) mod threads;

const UI_STATE_ID: &str = "sidebar-ui-state";

#[derive(Clone, Default)]
struct SidebarUiState {
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarAction {
    SelectProject(ProjectId),
    OpenAddProject,
    OpenProjectSettings(ProjectId),
    CreateThread(String),
    /// Start a thread in the given project and make it the active one.
    CreateThreadIn(ProjectId),
    /// Fork a child thread from a completed turn (transcript entry id).
    ForkAtTurn {
        thread: ThreadId,
        entry_id: usize,
    },
    /// Rewind to a completed turn, keeping the current conversation as a version.
    RewindToTurn {
        thread: ThreadId,
        entry_id: usize,
    },
    /// Rewind to before a turn's first message and return it to the composer.
    EditFromMessage {
        thread: ThreadId,
        entry_id: usize,
    },
    /// Show another version of a rewound conversation.
    SwitchVersion(ThreadId),
    SwitchThread(ThreadId),
    TogglePin(ThreadId),
    ToggleArchive(ThreadId),
}

pub fn sidebar_pane(
    ui: &mut egui::Ui,
    sidebar: &SidebarState,
    phases: &BTreeMap<String, ThreadRunPhase>,
    telemetry: &crate::model::telemetry::TelemetryOverlay,
    question_threads: &BTreeSet<ThreadId>,
    unread_threads: &BTreeSet<ThreadId>,
) -> Option<SidebarAction> {
    let indicators = threads::ThreadIndicators {
        phases,
        telemetry,
        question_threads,
        unread_threads,
    };
    let pane_state = ui
        .ctx()
        .data(|data| data.get_temp::<SidebarUiState>(egui::Id::new(UI_STATE_ID)))
        .unwrap_or_default();
    let mut action = None;

    pane_root(ui, "Projects", |ui| {
        egui::ScrollArea::vertical()
            .id_salt("sidebar-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The pane itself is the surface; nesting another frame only adds borders.
                let selected = selected_project(sidebar);
                projects::render(
                    ui,
                    sidebar,
                    selected,
                    &pane_state,
                    &mut action,
                    |ui, project, action| {
                        threads::render(ui, sidebar, project, &indicators, action);
                    },
                );
            });
    });

    action
}

pub fn set_sidebar_error(ctx: &egui::Context, error: Option<String>) {
    let id = egui::Id::new(UI_STATE_ID);
    ctx.data_mut(|data| {
        let state = data.get_temp_mut_or_default::<SidebarUiState>(id);
        state.error = error;
    });
}

fn selected_project(sidebar: &SidebarState) -> Option<&workspace_ui::ProjectRecord> {
    let selected = sidebar.selected_project.as_ref()?;
    sidebar
        .projects
        .iter()
        .find(|project| &project.id == selected)
}
