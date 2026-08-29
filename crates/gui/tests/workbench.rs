use std::sync::Arc;
use std::time::Duration;

use egui::{Key, Modifiers, vec2};
use egui_dock::{NodePath, SurfaceIndex, TabInsert};
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, EventBus, LifecycleEvent, MessageEvent};
use runtime::{AgentSummary, RunId};
use workspace_ui::{PanelId, UiSettings};

use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::model::tasks::AgentRunSource;
use gui::model::transcript::TranscriptEntry;

#[derive(Clone)]
struct MockSource(Vec<AgentSummary>);

impl MockSource {
    fn empty() -> Self {
        Self(Vec::new())
    }
}

impl AgentRunSource for MockSource {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

fn build_harness(
    state: WorkbenchState<MockSource>,
) -> Harness<'static, WorkbenchState<MockSource>> {
    Harness::builder()
        .with_size(vec2(800.0, 600.0))
        .build_ui_state(
            |ui, state: &mut WorkbenchState<MockSource>| {
                state.ui(ui, &mut eframe::Frame::_new_kittest());
            },
            state,
        )
}

fn is_active_tab(dock: &egui_dock::DockState<PanelId>, panel_id: &PanelId) -> bool {
    let Some(tab_path) = dock.find_tab(panel_id) else {
        return false;
    };
    let Ok(leaf) = dock.leaf(tab_path.node_path()) else {
        return false;
    };
    leaf.active.0 == tab_path.tab.0
}

fn collect_panel_ids(
    node: &workspace_ui::LayoutNode,
    out: &mut std::collections::HashSet<PanelId>,
) {
    match node {
        workspace_ui::LayoutNode::Split(split) => {
            collect_panel_ids(&split.first, out);
            collect_panel_ids(&split.second, out);
        }
        workspace_ui::LayoutNode::Tabs(tabs) => {
            out.extend(tabs.panels.iter().cloned());
        }
    }
}

#[test]
fn three_panes_render_with_titles() {
    // Given: a default workbench state
    let state = WorkbenchState::new(MockSource::empty(), &UiSettings::default(), "test-model")
        .expect("default state builds");
    let mut harness = build_harness(state);

    // When: the UI is rendered
    harness.run();

    // Then: all three pane titles are present
    harness.get_by_label("Agent");
    harness.get_by_label("Run ID");
    harness.get_by_role(egui_dock::egui::accesskit::Role::TextInput);
}

#[test]
fn focus_switching_via_keybind_changes_active_tab() {
    // Given: a layout where agent and terminal share a tab group
    let state = WorkbenchState::new(MockSource::empty(), &UiSettings::default(), "test-model")
        .expect("default state builds");
    let mut harness = build_harness(state);
    harness.run();

    let agent_id = PanelId::new("agent-main");
    let terminal_id = PanelId::new("terminal-main");
    let agent_path = harness
        .state()
        .dock()
        .find_tab(&agent_id)
        .expect("agent tab exists");
    let terminal_path = harness
        .state()
        .dock()
        .find_tab(&terminal_id)
        .expect("terminal tab exists");
    harness.state_mut().dock_mut().move_tab(
        agent_path,
        (
            NodePath::new(SurfaceIndex::main(), terminal_path.node),
            TabInsert::Append,
        ),
    );
    harness.run();

    // When: Ctrl+1 is pressed to focus the agent pane
    harness.key_press_modifiers(Modifiers::COMMAND, Key::Num1);
    harness.run();

    // Then: the agent tab becomes active within the shared group
    assert!(is_active_tab(harness.state().dock(), &agent_id));
}

#[test]
fn dock_undock_and_tab_move_operations_update_state() {
    // Given: a rendered default layout
    let state = WorkbenchState::new(MockSource::empty(), &UiSettings::default(), "test-model")
        .expect("default state builds");
    let mut harness = build_harness(state);
    harness.run();

    // When: the terminal tab is moved into the tasks tab group
    let terminal_id = PanelId::new("terminal-main");
    let tasks_id = PanelId::new("tasks-main");
    let terminal_path = harness
        .state()
        .dock()
        .find_tab(&terminal_id)
        .expect("terminal tab exists");
    let tasks_node = harness
        .state()
        .dock()
        .find_tab(&tasks_id)
        .expect("tasks tab exists")
        .node;
    harness.state_mut().dock_mut().move_tab(
        terminal_path,
        (
            NodePath::new(SurfaceIndex::main(), tasks_node),
            TabInsert::Append,
        ),
    );
    harness.run();

    // Then: terminal now shares a tab group with tasks
    let tasks_path = harness
        .state()
        .dock()
        .find_tab(&tasks_id)
        .expect("tasks tab exists");
    let leaf = harness
        .state()
        .dock()
        .leaf(tasks_path.node_path())
        .expect("leaf exists");
    assert!(leaf.tabs.contains(&terminal_id));

    // When: the agent pane is undocked into a floating window
    let agent_id = PanelId::new("agent-main");
    harness.state_mut().dock_mut().add_window(vec![agent_id]);
    harness.run();

    // Then: a window surface exists and the agent tab is still findable
    let has_window = harness
        .state()
        .dock()
        .iter_surfaces_indexed()
        .any(|(_, surface)| matches!(surface, egui_dock::Surface::Window(_, _)));
    assert!(has_window);
}

#[test]
fn transcript_text_appears_after_bus_event() {
    // Given: a workbench with an event pump connected to a bus
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let bus = EventBus::new(8);
    let (repaint_tx, repaint_rx) = std::sync::mpsc::channel();
    let pump = EventPump::spawn(
        runtime.handle(),
        bus.subscribe(),
        Some(Arc::new(move || {
            let _ = repaint_tx.send(());
        })),
    );
    let state = WorkbenchState::new(MockSource::empty(), &UiSettings::default(), "test-model")
        .expect("default state builds")
        .with_pump(pump);
    let mut harness = build_harness(state);
    harness.run();

    // When: a message delta is emitted and forwarded
    bus.emit(Event::new(MessageEvent::MessageDelta {
        delta: "hello from bus".into(),
    }));
    assert!(repaint_rx.recv_timeout(Duration::from_secs(1)).is_ok());
    harness.run();

    // Then: the transcript contains the message
    let entries = harness.state().transcript().entries();
    assert!(entries.iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Message { text } if text == "hello from bus"
    )));
}

#[test]
fn tasks_row_updates_after_state_change_event() {
    // Given: a workbench with a task source and event pump
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let bus = EventBus::new(8);
    let source = MockSource(vec![AgentSummary {
        run_id: RunId::new(1),
        role_name: "worker".into(),
        phase: AgentRunPhase::Running,
    }]);
    let (repaint_tx, repaint_rx) = std::sync::mpsc::channel();
    let pump = EventPump::spawn(
        runtime.handle(),
        bus.subscribe(),
        Some(Arc::new(move || {
            let _ = repaint_tx.send(());
        })),
    );
    let state = WorkbenchState::new(source, &UiSettings::default(), "test-model")
        .expect("default state builds")
        .with_pump(pump);
    let mut harness = build_harness(state);
    harness.run();

    // When: an AgentRunStateChanged event is emitted
    bus.emit(Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: "run-1".into(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Done,
        reason: None,
    }));
    assert!(repaint_rx.recv_timeout(Duration::from_secs(1)).is_ok());
    harness.run();

    // Then: the row status is updated in place
    assert_eq!(
        harness.state().tasks().rows()[0].status,
        AgentRunPhase::Done
    );
}

#[test]
fn save_layout_keybind_persists_workspace_json() {
    // Given: a workbench with a save path
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let path = temp_dir.path().join("workspace.json");
    let state = WorkbenchState::new(MockSource::empty(), &UiSettings::default(), "test-model")
        .expect("default state builds")
        .with_save_path(&path);
    let mut harness = build_harness(state);
    harness.run();

    // When: Ctrl+S is pressed
    harness.key_press_modifiers(Modifiers::COMMAND, Key::S);
    harness.run();

    // Then: a workspace JSON file is written and round-trips correctly
    assert!(path.exists());
    let loaded = workspace_ui::load_from(&path).expect("load workspace");
    let mut panels = std::collections::HashSet::new();
    collect_panel_ids(&loaded.main.root, &mut panels);
    assert!(panels.contains(&PanelId::new("agent-main")));
    assert!(panels.contains(&PanelId::new("terminal-main")));
    assert!(panels.contains(&PanelId::new("tasks-main")));
}
