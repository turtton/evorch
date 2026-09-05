use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use event_bus::{AgentRunPhase, Event, EventBus, LifecycleEvent};
use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::fixture::{demo_events, demo_sidebar};
use gui::headless::HeadlessWorkbench;
use gui::model::project_bridge::run_membership;
use gui::model::tasks::AgentRunSource;
use runtime::{
    AgentInspection, AgentSummary, MergeMode, RunId, WorkspaceInspection, WorkspaceMode,
};
use workspace_ui::{Membership, ProjectId, SidebarState, TrustState, UiSettings};

#[derive(Clone, Default)]
struct MockSource {
    summaries: Vec<AgentSummary>,
    inspections: HashMap<RunId, AgentInspection>,
}

impl AgentRunSource for MockSource {
    fn list(&self) -> Vec<AgentSummary> {
        self.summaries.clone()
    }

    fn inspect(&self, run_id: RunId) -> Option<AgentInspection> {
        self.inspections.get(&run_id).cloned()
    }
}

fn state(source: MockSource, sidebar: SidebarState) -> WorkbenchState<MockSource> {
    WorkbenchState::new(source, &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar)
}

fn sidebar_with_project(root: &std::path::Path) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project can be added");
    sidebar
        .select_project(&project_id)
        .expect("project can be selected");
    sidebar
}

fn inspection(run_id: u64, branch: &str, worktree_path: std::path::PathBuf) -> AgentInspection {
    AgentInspection {
        run_id: RunId::new(run_id),
        role_name: "Worker".into(),
        phase: AgentRunPhase::Running,
        message_count: 0,
        workspace: Some(WorkspaceInspection {
            mode: WorkspaceMode::Isolated,
            branch: Some(branch.into()),
            worktree_path: Some(worktree_path),
            merge_mode: MergeMode::Branch,
        }),
    }
}

#[test]
fn add_and_select_project_persists_to_sidebar_file() {
    // Given: an empty workbench with a sidebar persistence path
    let temp = tempfile::tempdir().expect("temp dir");
    let project = temp.path().join("demo");
    std::fs::create_dir(&project).expect("project directory");
    let sidebar_path = temp.path().join("sidebar.json");
    let workbench = WorkbenchState::new(MockSource::default(), &UiSettings::default())
        .expect("default state builds")
        .with_sidebar_path(sidebar_path.clone());
    let mut harness = HeadlessWorkbench::new(workbench, [800.0, 600.0]);

    // When: a project is added, selected, and saved through the public state API
    let project_id = harness
        .state_mut()
        .add_project(&project)
        .expect("project can be added");
    harness
        .state_mut()
        .select_project(project_id)
        .expect("project can be selected");
    harness.state().save_sidebar();

    // Then: the persisted project list equals the in-memory project list
    let loaded = workspace_ui::load_sidebar(&sidebar_path).expect("sidebar can be loaded");
    assert_eq!(loaded.projects, harness.state().sidebar().projects);
}

#[test]
fn trust_click_approves_allowed_directory_and_persists_sidebar() {
    // Given: a selected project with one unapproved allowed directory and persistence path.
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("demo");
    let allowed = temp.path().join("external");
    std::fs::create_dir(&root).expect("project directory");
    std::fs::create_dir(&allowed).expect("allowed directory");
    let mut sidebar = sidebar_with_project(&root);
    sidebar
        .add_allowed_directory(&ProjectId::new("demo"), &allowed, TrustState::Unapproved)
        .expect("allowed directory can be added");
    let sidebar_path = temp.path().join("sidebar.json");
    let workbench = state(MockSource::default(), sidebar).with_sidebar_path(sidebar_path.clone());
    let mut harness = HeadlessWorkbench::new(workbench, [800.0, 600.0]);
    harness.run();

    // When: the operator clicks Trust and saves through the public state surface.
    harness.click_label("Trust");
    harness.run();
    harness.state().save_sidebar();

    // Then: both rendered state and persisted state report approved trust.
    assert_eq!(
        harness.state().sidebar().projects[0].allowed_directories[0].trust,
        TrustState::Approved
    );
    assert!(harness.has_label("trusted"));
    let loaded = workspace_ui::load_sidebar(&sidebar_path).expect("sidebar can be loaded");
    assert_eq!(
        loaded.projects[0].allowed_directories[0].trust,
        TrustState::Approved
    );
}

#[test]
fn run_cwd_under_runtime_worktree_is_auto_allowed() {
    // Given: a registered project and an inspected run under its runtime worktree root
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("demo");
    let worktree = root.join(".evorch/worktrees/run-2");
    std::fs::create_dir_all(&worktree).expect("worktree directory");
    let sidebar = sidebar_with_project(&root);
    let run = inspection(2, "evorch/task/run-2", worktree.clone());

    // When: run membership is resolved
    let membership = run_membership(&sidebar, &run);

    // Then: the runtime-owned worktree is allowed without explicit trust
    assert_eq!(
        membership,
        Membership::RuntimeWorktree { run_dir: worktree }
    );
}

#[test]
fn external_path_is_outside_without_explicit_trust() {
    // Given: a registered project and an inspected run in an unrelated sibling directory
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("demo");
    let external = temp.path().join("external");
    std::fs::create_dir(&root).expect("project directory");
    std::fs::create_dir(&external).expect("external directory");
    let sidebar = sidebar_with_project(&root);
    let run = inspection(3, "external", external);

    // When: run membership is resolved
    let membership = run_membership(&sidebar, &run);

    // Then: the unrelated directory remains outside the project trust boundary
    assert_eq!(membership, Membership::Outside);
}

#[test]
fn create_switch_pin_thread_via_ui_clicks() {
    // Given: a rendered sidebar with one selected project
    let temp = tempfile::tempdir().expect("temp dir");
    let sidebar = sidebar_with_project(temp.path());
    let mut harness = HeadlessWorkbench::new(state(MockSource::default(), sidebar), [800.0, 600.0]);
    harness.run();

    // When: a thread is created, selected by title, and pinned through the UI
    harness.click_label("New thread");
    harness.run();
    harness.click_label("thread-1");
    harness.run();
    harness.click_label("☆");
    harness.run();

    // Then: the sidebar records the active pinned thread
    let sidebar = harness.state().sidebar();
    assert_eq!(
        sidebar.active_thread.as_ref().map(ToString::to_string),
        Some("thread-1".into())
    );
    assert_eq!(sidebar.threads.len(), 1);
    assert!(sidebar.threads[0].pinned);
}

#[test]
fn thread_state_follows_lifecycle_events() {
    // Given: an active thread connected to an event pump
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let bus = EventBus::new(16);
    let (repaint_tx, repaint_rx) = mpsc::channel();
    let pump = EventPump::spawn(
        runtime.handle(),
        bus.subscribe(),
        Some(Arc::new(move || {
            let _ = repaint_tx.send(());
        })),
    );
    let temp = tempfile::tempdir().expect("temp dir");
    let mut sidebar = sidebar_with_project(temp.path());
    sidebar
        .create_thread(
            workspace_ui::ThreadId::new("thread-1"),
            ProjectId::new("demo"),
            "thread-1",
        )
        .expect("thread can be created");
    sidebar
        .switch_thread(&workspace_ui::ThreadId::new("thread-1"))
        .expect("thread can be selected");
    let workbench = state(MockSource::default(), sidebar).with_pump(pump);
    let mut harness = HeadlessWorkbench::new(workbench, [800.0, 600.0]);
    harness.run();

    // When: the run is attached and progresses through runtime lifecycle phases
    bus.emit(Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "run-1".into(),
        parent_run_id: None,
        agent_name: "worker".into(),
        role: "worker".into(),
    }));
    repaint_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("start repaint");
    harness.run();
    for (from, to, badge) in [
        (AgentRunPhase::Pending, AgentRunPhase::Running, "Running"),
        (AgentRunPhase::Running, AgentRunPhase::Waiting, "Waiting"),
        (AgentRunPhase::Waiting, AgentRunPhase::Done, "Done"),
        (AgentRunPhase::Done, AgentRunPhase::Error, "Error"),
    ] {
        bus.emit(Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from,
            to,
            reason: None,
        }));
        repaint_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("state repaint");
        harness.run();
        assert!(harness.has_label(badge), "missing {badge} badge");
    }
    harness.click_label("Pause");
    harness.run();

    // Then: operator pause overrides the runtime phase badge
    assert!(harness.has_label("Paused"));
}

#[test]
fn thread_shows_branch_and_worktree_indicator() {
    // Given: an active thread whose run inspection has branch and worktree metadata
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("demo");
    let worktree = root.join(".evorch/worktrees/run-2");
    std::fs::create_dir_all(&worktree).expect("worktree directory");
    let mut sidebar = sidebar_with_project(&root);
    sidebar
        .create_thread(
            workspace_ui::ThreadId::new("thread-1"),
            ProjectId::new("demo"),
            "thread-1",
        )
        .expect("thread can be created");
    sidebar
        .switch_thread(&workspace_ui::ThreadId::new("thread-1"))
        .expect("thread can be selected");
    sidebar.threads[0].run_ids.push("run-2".into());
    let source = MockSource {
        summaries: Vec::new(),
        inspections: HashMap::from([(
            RunId::new(2),
            inspection(2, "evorch/task/run-2", worktree.clone()),
        )]),
    };
    let mut harness = HeadlessWorkbench::new(state(source, sidebar), [800.0, 600.0]);

    // When: the workbench renders and refreshes inspection metadata
    harness.run();

    // Then: the branch and worktree are shown together
    let indicator = format!("evorch/task/run-2 @ {}", worktree.display());
    assert!(harness.has_label(&indicator));
}

#[test]
fn sidebar_without_projects_shows_placeholder_and_single_add_project_cta() {
    // Given: a default workbench with no projects
    let workbench = WorkbenchState::new(MockSource::default(), &UiSettings::default())
        .expect("default state builds");
    let mut harness = HeadlessWorkbench::new(workbench, [800.0, 600.0]);
    harness.run();

    // Then: the placeholder is visible and only one "Add project" node exists
    assert!(harness.has_label("No projects yet"));
    assert_eq!(harness.count_labels("Add project"), 1);
    assert!(harness.has_label("Projects"));
}

#[test]
fn sidebar_with_project_but_no_threads_shows_thread_placeholder() {
    // Given: a rendered sidebar with one selected project and no threads
    let temp = tempfile::tempdir().expect("temp dir");
    let sidebar = sidebar_with_project(temp.path());
    let mut harness = HeadlessWorkbench::new(state(MockSource::default(), sidebar), [800.0, 600.0]);
    harness.run();

    // Then: the thread placeholder is shown with a single "New thread" CTA
    assert!(harness.has_label("No threads yet"));
    assert_eq!(harness.count_labels("New thread"), 1);

    // When: the operator clicks the CTA
    harness.click_label("New thread");
    harness.run();

    // Then: the placeholder disappears and the new thread title is rendered
    assert!(!harness.has_label("No threads yet"));
    assert!(harness.has_label("thread-1"));
}

#[test]
fn sidebar_thread_rows_expose_state_text() {
    // Given: a demo sidebar populated with lifecycle events
    let temp = tempfile::tempdir().expect("temp dir");
    let sidebar = demo_sidebar(temp.path()).expect("demo sidebar builds");
    let workbench = state(MockSource::default(), sidebar);
    let mut harness = HeadlessWorkbench::new(workbench, [800.0, 600.0]);
    harness.run();
    harness.state_mut().apply_events(demo_events());
    harness.run();

    // Then: running and paused thread states are both exposed as labels
    assert!(harness.has_label("Running"));
    assert!(harness.has_label("Paused"));
}
