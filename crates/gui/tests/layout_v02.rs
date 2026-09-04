use std::collections::BTreeSet;

use egui::{Key, Modifiers};
use egui_dock::SurfaceIndex;
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use runtime::{AgentSummary, RunId};
use workspace_ui::{LayoutNode, PanelId, UiSettings, Workspace};

#[derive(Clone)]
struct Source(Vec<AgentSummary>);

impl AgentRunSource for Source {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

fn leaf_panels(node: &LayoutNode, leaves: &mut Vec<Vec<PanelId>>) {
    match node {
        LayoutNode::Split(split) => {
            leaf_panels(&split.first, leaves);
            leaf_panels(&split.second, leaves);
        }
        LayoutNode::Tabs(tabs) => leaves.push(tabs.panels.clone()),
    }
}

#[test]
fn default_layout_is_sidebar_center_right_tabs() {
    // Given: the default v0.2 workspace
    let workspace = Workspace::default();
    let mut leaves = Vec::new();

    // When: its dock leaves are collected from left to right
    leaf_panels(&workspace.main.root, &mut leaves);

    // Then: sidebar, conversation, and workbench tabs occupy the three leaves
    assert_eq!(
        leaves,
        vec![
            vec![PanelId::new("sidebar-main")],
            vec![PanelId::new("agent-main")],
            vec![
                PanelId::new("agents-main"),
                PanelId::new("diff-main"),
                PanelId::new("terminal-main"),
                PanelId::new("goal-main"),
                PanelId::new("merge-main"),
            ],
        ]
    );
}

#[test]
fn v1_layout_file_loads_via_migration_into_workbench() {
    // Given: a persisted v0.1 workspace fixture
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let path = temp_dir.path().join("workspace.json");
    std::fs::write(
        &path,
        include_str!("../../workspace-ui/tests/fixtures/workspace_v1.json"),
    )
    .expect("write fixture");

    // When: it is loaded through workspace-ui and used as GUI settings
    let workspace = workspace_ui::load_from(&path).expect("migrate v1 fixture");
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(workspace);
    let state = WorkbenchState::new(Source(Vec::new()), &settings).expect("build workbench");

    // Then: the migrated legacy panels remain available in the dock
    assert!(state.dock().find_tab(&PanelId::new("tasks-main")).is_some());
    assert!(state.dock().find_tab(&PanelId::new("agent-main")).is_some());
    assert!(
        state
            .dock()
            .find_tab(&PanelId::new("terminal-main"))
            .is_some()
    );
}

#[test]
fn reset_layout_restores_v02_default() {
    // Given: a workbench whose terminal tab has been undocked
    let state =
        WorkbenchState::new(Source(Vec::new()), &UiSettings::default()).expect("build workbench");
    let mut workbench = HeadlessWorkbench::new(state, [800.0, 600.0]);
    workbench.run();
    let terminal = PanelId::new("terminal-main");
    let path = workbench
        .state()
        .dock()
        .find_tab(&terminal)
        .expect("terminal tab");
    workbench.state_mut().dock_mut().detach_tab(
        path,
        egui::Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(320.0, 240.0)),
    );

    // When: the reset keybind is dispatched
    workbench.key_press(Modifiers::COMMAND | Modifiers::SHIFT, Key::R);
    workbench.run();

    // Then: all default v0.2 panels are back on the main surface
    for id in [
        "sidebar-main",
        "agent-main",
        "agents-main",
        "diff-main",
        "terminal-main",
        "goal-main",
        "merge-main",
    ] {
        let tab = workbench
            .state()
            .dock()
            .find_tab(&PanelId::new(id))
            .expect("default tab");
        assert_eq!(tab.surface, SurfaceIndex::main());
    }
}

#[test]
fn dynamic_agent_pane_roundtrips_through_save_load() {
    // Given: representative orchestrator, worker, and reviewer runs
    let source = Source(vec![
        summary(1, "Orchestrator", "orchestrator"),
        summary(2, "Worker old", "worker"),
        summary(3, "Worker latest", "worker"),
        summary(4, "Reviewer", "reviewer"),
    ]);
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let path = temp_dir.path().join("workspace.json");
    let mut state = WorkbenchState::new(source.clone(), &UiSettings::default())
        .expect("build workbench")
        .with_save_path(&path);

    // When: default agent panes are opened twice and the layout is saved
    state.open_default_agent_panes();
    state.open_default_agent_panes();
    let mut workbench = HeadlessWorkbench::new(state, [800.0, 600.0]);
    workbench.key_press(Modifiers::COMMAND, Key::S);
    workbench.run();
    let loaded = workspace_ui::load_from(&path).expect("load saved workspace");

    // Then: exactly one pane per selected role round-trips with its target
    let dynamic = loaded
        .panels
        .values()
        .filter(|panel| panel.id.as_str().starts_with("agent-run-"))
        .map(|panel| (panel.id.clone(), panel.target.clone()))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        dynamic,
        BTreeSet::from([
            (PanelId::new("agent-run-1"), Some("run-1".into())),
            (PanelId::new("agent-run-3"), Some("run-3".into())),
            (PanelId::new("agent-run-4"), Some("run-4".into())),
        ])
    );

    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(loaded);
    let reloaded = WorkbenchState::new(source, &settings).expect("reload workbench");
    for id in ["agent-run-1", "agent-run-3", "agent-run-4"] {
        assert!(reloaded.dock().find_tab(&PanelId::new(id)).is_some());
    }
}

#[test]
fn undock_to_floating_and_reload_preserves_v02_panels() {
    // Given: a default v0.2 workbench with Diff detached to a floating window
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let path = temp_dir.path().join("workspace.json");
    let state = WorkbenchState::new(Source(Vec::new()), &UiSettings::default())
        .expect("build workbench")
        .with_save_path(&path);
    let mut workbench = HeadlessWorkbench::new(state, [800.0, 600.0]);
    workbench.run();
    let diff = PanelId::new("diff-main");
    let diff_path = workbench.state().dock().find_tab(&diff).expect("diff tab");
    workbench.state_mut().dock_mut().detach_tab(
        diff_path,
        egui::Rect::from_min_size(egui::pos2(40.0, 40.0), egui::vec2(400.0, 300.0)),
    );

    // When: the layout is saved and loaded into a fresh WorkbenchState
    workbench.key_press(Modifiers::COMMAND, Key::S);
    workbench.run();
    let loaded = workspace_ui::load_from(&path).expect("load saved workspace");
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(loaded);
    let reloaded = WorkbenchState::new(Source(Vec::new()), &settings).expect("reload workbench");

    // Then: every v0.2 panel remains registered and Diff remains floating
    for id in [
        "sidebar-main",
        "agent-main",
        "agents-main",
        "diff-main",
        "terminal-main",
        "goal-main",
        "merge-main",
    ] {
        assert!(reloaded.dock().find_tab(&PanelId::new(id)).is_some());
    }
    assert_ne!(
        reloaded
            .dock()
            .find_tab(&diff)
            .expect("reloaded diff")
            .surface,
        SurfaceIndex::main()
    );
}

fn summary(id: u64, name: &str, role: &str) -> AgentSummary {
    AgentSummary {
        run_id: RunId::new(id),
        name: name.into(),
        role_name: role.into(),
        phase: event_bus::AgentRunPhase::Running,
        model: "fixture".into(),
    }
}
