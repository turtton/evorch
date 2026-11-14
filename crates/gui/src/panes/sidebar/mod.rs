use std::collections::{BTreeMap, BTreeSet};

use workspace_ui::{ProjectId, ProjectSort, SidebarState, SidebarView, ThreadId, ThreadRunPhase};

use crate::theme::widgets::pane_root;

mod projects;
pub(crate) mod threads;
mod toolbar;

const UI_STATE_ID: &str = "sidebar-ui-state";

#[derive(Clone, Default)]
struct SidebarUiState {
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarAction {
    SelectProject(ProjectId),
    OpenAddProject,
    /// Choose how the project list is ordered; the stored order is unchanged.
    SetProjectSort(ProjectSort),
    /// Fold or unfold one project's thread list.
    SetProjectCollapsed {
        project: ProjectId,
        collapsed: bool,
    },
    /// Fold (`true`) or unfold (`false`) every project at once.
    SetAllProjectsCollapsed(bool),
    /// Search the threads of this project; the selection is left as it is.
    OpenThreadSearch(ProjectId),
    OpenProjectSettings(ProjectId),
    /// Start an untitled thread in the selected project and make it active.
    CreateThread,
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
    ManageLane(storage::lane_management::LaneManagementRequest),
    /// Open the confirmation for ending a managed lane by hand.
    CloseLane {
        thread: ThreadId,
        choice: crate::model::lane_close::CloseChoice,
    },
    ReattachLane(workspace_ui::lane_management::LaneDropIntent),
    /// Open the "Link PR…" dialog for a thread.
    LinkPullRequest(ThreadId),
}

/// What the thread rows show about pull requests: their badges, and whether the feature is
/// on (the right-click "Link PR…" is offered only then).
#[derive(Clone, Copy, Default)]
pub struct PullRequestsView<'a> {
    pub enabled: bool,
    pub badges: Option<&'a BTreeMap<ThreadId, crate::model::pull_requests::PullRequestBadge>>,
}

/// What the thread rows show about ended lanes: their badges, and whether this window can
/// end a lane by hand (the menu entries are offered only then).
#[derive(Clone, Copy, Default)]
pub struct LaneCloseView<'a> {
    pub can_close: bool,
    pub badges: Option<&'a BTreeMap<ThreadId, crate::model::lane_close::LaneCloseBadge>>,
}

pub fn sidebar_pane(
    ui: &mut egui::Ui,
    sidebar: &SidebarState,
    phases: &BTreeMap<String, ThreadRunPhase>,
    question_threads: &BTreeSet<ThreadId>,
    approval_threads: &BTreeSet<ThreadId>,
    unread_threads: &BTreeSet<ThreadId>,
) -> Option<SidebarAction> {
    sidebar_pane_with_retention(
        ui,
        sidebar,
        phases,
        question_threads,
        approval_threads,
        unread_threads,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &SidebarView::default(),
    )
}

/// [`sidebar_pane`] that also explains, on hover over an archived thread, when it
/// is deleted or why it is kept. `view` is the local project sort and folding.
#[allow(clippy::too_many_arguments)]
pub fn sidebar_pane_with_retention(
    ui: &mut egui::Ui,
    sidebar: &SidebarState,
    phases: &BTreeMap<String, ThreadRunPhase>,
    question_threads: &BTreeSet<ThreadId>,
    approval_threads: &BTreeSet<ThreadId>,
    unread_threads: &BTreeSet<ThreadId>,
    externally_working_threads: &BTreeSet<ThreadId>,
    retention: &BTreeMap<ThreadId, storage::thread_retention::RetentionStatus>,
    view: &SidebarView,
) -> Option<SidebarAction> {
    sidebar_pane_with_pull_requests(
        ui,
        sidebar,
        (phases, question_threads, approval_threads, unread_threads),
        (externally_working_threads, retention),
        view,
        PullRequestsView::default(),
    )
}

/// [`sidebar_pane_with_retention`] that also shows each linked thread's pull request badge.
#[allow(clippy::too_many_arguments)]
pub fn sidebar_pane_with_pull_requests(
    ui: &mut egui::Ui,
    sidebar: &SidebarState,
    (phases, question_threads, approval_threads, unread_threads): (
        &BTreeMap<String, ThreadRunPhase>,
        &BTreeSet<ThreadId>,
        &BTreeSet<ThreadId>,
        &BTreeSet<ThreadId>,
    ),
    (externally_working_threads, retention): (
        &BTreeSet<ThreadId>,
        &BTreeMap<ThreadId, storage::thread_retention::RetentionStatus>,
    ),
    view: &SidebarView,
    pull_requests: PullRequestsView<'_>,
) -> Option<SidebarAction> {
    sidebar_pane_with_lane_close(
        ui,
        sidebar,
        (phases, question_threads, approval_threads, unread_threads),
        (externally_working_threads, retention),
        view,
        pull_requests,
        LaneCloseView::default(),
    )
}

/// [`sidebar_pane_with_pull_requests`] that also shows ended lanes and offers to end one.
#[allow(clippy::too_many_arguments)]
pub fn sidebar_pane_with_lane_close(
    ui: &mut egui::Ui,
    sidebar: &SidebarState,
    (phases, question_threads, approval_threads, unread_threads): (
        &BTreeMap<String, ThreadRunPhase>,
        &BTreeSet<ThreadId>,
        &BTreeSet<ThreadId>,
        &BTreeSet<ThreadId>,
    ),
    (externally_working_threads, retention): (
        &BTreeSet<ThreadId>,
        &BTreeMap<ThreadId, storage::thread_retention::RetentionStatus>,
    ),
    view: &SidebarView,
    pull_requests: PullRequestsView<'_>,
    lane_close: LaneCloseView<'_>,
) -> Option<SidebarAction> {
    let active_runs = threads::threads_with_active_runs(&sidebar.threads, phases);
    let indicators = threads::ThreadIndicators {
        phases,
        question_threads,
        approval_threads,
        unread_threads,
        externally_working: externally_working_threads,
        active_runs: &active_runs,
        retention,
        pull_requests,
        lane_close,
    };
    let pane_state = ui
        .ctx()
        .data(|data| data.get_temp::<SidebarUiState>(egui::Id::new(UI_STATE_ID)))
        .unwrap_or_default();
    let mut action = None;
    // Version groups are resolved once per frame and shared by every project.
    let lineage = workspace_ui::LineageIndex::new(&sidebar.threads);

    // The order is a view over `sidebar.projects`, built once per frame.
    let order = workspace_ui::ordered_projects(sidebar, view.project_sort);

    pane_root(ui, "Projects", |ui| {
        // Outside the scroll area, so it stays put however long the list is.
        toolbar::render(ui, sidebar, view, &mut action);
        egui::ScrollArea::vertical()
            .id_salt("sidebar-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The pane itself is the surface; nesting another frame only adds borders.
                projects::render(
                    ui,
                    sidebar,
                    &order,
                    view,
                    &pane_state,
                    &mut action,
                    |ui, project, action| {
                        threads::render(ui, sidebar, project, &lineage, &indicators, action);
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
