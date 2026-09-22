use std::sync::Arc;

#[path = "background_run_headless/equalization.rs"]
mod equalization;

#[test]
fn floating_completion_returns_to_main_region() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut h, _rt) = workbench(temp.path());
    h.state_mut().apply_events([started("a")]);
    let id = PanelId::new("agent-a");
    let path = h.state().dock().find_tab(&id).expect("a");
    h.state_mut().dock_mut().detach_tab(
        path,
        egui::Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(400.0, 300.0)),
    );
    assert_ne!(
        h.state().dock().find_tab(&id).expect("floating a").surface,
        egui_dock::SurfaceIndex::main()
    );
    h.state_mut().apply_events([completed("a")]);
    assert_eq!(pane_node(&h, "a"), egui_dock::NodeIndex(2));
    assert_parked(&h, "a");
    h.run();
    let workspace = saved_workspace(&mut h, &temp.path().join("workspace.json"));
    assert!(workspace.panels.contains_key(&id));
    gui::dock::from_dock_state(h.state().dock(), &workspace.panels)
        .expect("export without missing or empty nodes");
}

#[test]
fn manually_opened_root_is_not_a_subagent_region() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut h, _rt) = workbench(temp.path());
    h.state_mut().open_agent_pane("root");
    let agents = PanelId::new("agents-main");
    let path = h.state().dock().find_tab(&agents).expect("agents");
    let leaf = h.state().dock().leaf(path.node_path()).expect("leaf");
    let before = (leaf.tabs.clone(), leaf.active);
    h.state_mut().apply_events([started("a")]);
    assert_eq!(pane_node(&h, "a"), egui_dock::NodeIndex(2));
    h.state_mut().apply_events([completed("a")]);
    assert_eq!(pane_node(&h, "a"), egui_dock::NodeIndex(2));
    assert_parked(&h, "a");
    let path = h.state().dock().find_tab(&agents).expect("agents");
    let leaf = h.state().dock().leaf(path.node_path()).expect("leaf");
    assert_eq!((leaf.tabs.clone(), leaf.active), before);
    assert_ne!(path.node, pane_node(&h, "a"));
}

use async_trait::async_trait;
use event_bus::{AgentRunPhase, Event, EventBus, LifecycleEvent};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::composer::ProviderStatus;
use gui::model::transcript::TranscriptEntry;
use gui::runtime_sink::RuntimeCommandSink;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor,
    OrchestrationSettings, Role, RuntimeError,
};
use tools::ToolExecutor;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

struct PendingModel;

#[async_trait]
impl AgentModel for PendingModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
        "test-background".into()
    }
}

fn workbench(root: &std::path::Path) -> (HeadlessWorkbench<AgentRuntime>, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus.clone())),
        Arc::new(PendingModel),
    );
    let supervisor = rt.block_on(async {
        GoalSupervisor::spawn(
            runtime.clone(),
            bus,
            Arc::new(FixtureDeliveryAdapter::default()),
            OrchestrationSettings::default(),
        )
    });
    let sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor);
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project added");
    sidebar
        .select_project(&project_id)
        .expect("project selected");
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .expect("thread created");
    sidebar
        .switch_thread(&ThreadId::new("thread-1"))
        .expect("thread selected");
    let state = WorkbenchState::new(runtime, &UiSettings::default())
        .expect("default state builds")
        .with_sidebar(sidebar)
        .with_save_path(root.join("workspace.json"))
        .with_provider_status(ProviderStatus::Configured)
        .with_command_sink(Box::new(sink));
    (HeadlessWorkbench::new(state, [1200.0, 900.0]), rt)
}

#[test]
fn background_run_submission_keeps_focus_and_opens_pane_in_subagent_region() {
    // Given: the same selected project/thread as composer_dispatch_headless.
    let temp = tempfile::tempdir().expect("temp dir");
    let (mut harness, _rt) = workbench(temp.path());
    harness.run();
    let focus = harness.state().focus().clone();
    let keyboard_focus = harness.focused_id();
    // When: the normal composer submission path launches background work.
    harness.state_mut().composer_mut().input = "/run do the thing".into();
    harness.state_mut().submit_composer();
    harness.state_mut().apply_events([started("run-1")]);
    harness.run();
    // Then: only a launch notice is added; the conversation stays unbound.
    assert_eq!(harness.state().focus(), &focus);
    assert_eq!(harness.focused_id(), keyboard_focus);
    assert_eq!(pane_node(&harness, "run-1"), egui_dock::NodeIndex(2));
    assert!(harness.state().composer().input.is_empty());
    assert!(matches!(
        harness.state().transcripts().thread().entries(),
        [TranscriptEntry::Notice { text }] if text.contains("run-")
    ));
    assert!(harness.state().issued().is_empty());
}

fn started(run: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: Some("root".into()),
        agent_name: "worker".into(),
        role: "worker".into(),
    })
}

fn completed(run: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Done,
        reason: None,
    })
}

fn pane_node(harness: &HeadlessWorkbench<AgentRuntime>, run: &str) -> egui_dock::NodeIndex {
    let path = harness
        .state()
        .dock()
        .find_tab(&PanelId::new(format!("agent-{run}")))
        .expect("subagent pane exists");
    assert_eq!(path.surface, egui_dock::SurfaceIndex::main());
    path.node
}

fn assert_parked(harness: &HeadlessWorkbench<AgentRuntime>, run: &str) {
    let path = harness
        .state()
        .dock()
        .find_tab(&PanelId::new(format!("agent-{run}")))
        .expect("parked pane exists");
    let leaf = harness.state().dock().leaf(path.node_path()).expect("leaf");
    if let Some(conversation_index) = leaf.tabs.iter().position(|id| id.as_str() == "agent-main") {
        assert_eq!(path.tab.0, conversation_index + 1);
    } else {
        assert_eq!(path.tab.0, leaf.tabs.len() - 1);
    }
    assert_ne!(path.tab, leaf.active);
}

#[test]
fn three_subagent_panes_share_equal_height_fractions() {
    // Given: the default workbench with its sidebar split.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _rt) = workbench(temp.path());
    // When: three non-conversation children start in order.
    harness
        .state_mut()
        .apply_events([started("a"), started("b"), started("c")]);
    // Then: each top child consumes one of the remaining equal shares.
    let tree = harness.state().dock().main_surface();
    let mut node = egui_dock::NodeIndex::root().right();
    let mut remaining = 1.0;
    for (run, expected) in [("a", 1.0 / 3.0), ("b", 0.5)] {
        let egui_dock::Node::Vertical(split) = &tree[node] else {
            panic!("expected subagent vertical split at {node:?}");
        };
        assert_eq!(pane_node(&harness, run), node.left());
        assert!(
            (split.fraction - expected).abs() < f32::EPSILON,
            "expected {expected}, got {}",
            split.fraction
        );
        assert!((remaining * split.fraction - 1.0 / 3.0).abs() < f32::EPSILON);
        remaining *= 1.0 - split.fraction;
        node = node.right();
    }
    assert_eq!(pane_node(&harness, "c"), node);
    assert!((remaining - 1.0 / 3.0).abs() < f32::EPSILON);
}

#[test]
fn second_subagent_stacks_below_first_running_pane() {
    // Given: the default workbench.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _rt) = workbench(temp.path());
    // When: two children start in order.
    harness
        .state_mut()
        .apply_events([started("a"), started("b")]);
    // Then: the right subtree is a vertical split, A above B.
    let tree = harness.state().dock().main_surface();
    assert!(matches!(
        tree[egui_dock::NodeIndex(0)],
        egui_dock::Node::Horizontal(_)
    ));
    assert!(matches!(
        tree[egui_dock::NodeIndex(2)],
        egui_dock::Node::Vertical(_)
    ));
    assert_eq!(pane_node(&harness, "a"), egui_dock::NodeIndex(5));
    assert_eq!(pane_node(&harness, "b"), egui_dock::NodeIndex(6));
}

#[test]
fn completed_subagent_parks_as_inactive_tab_on_top_running_leaf() {
    // Given: A above B.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _rt) = workbench(temp.path());
    harness
        .state_mut()
        .apply_events([started("a"), started("b")]);
    // When: A completes.
    harness.state_mut().apply_events([completed("a")]);
    // Then: the old split is pruned and B stays active in the top leaf.
    assert_eq!(pane_node(&harness, "b"), egui_dock::NodeIndex(2));
    assert_eq!(pane_node(&harness, "a"), egui_dock::NodeIndex(2));
    assert_parked(&harness, "a");
}

#[test]
fn park_without_prior_started_creates_parked_tab() {
    // Given: no Started was received.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _rt) = workbench(temp.path());
    // When: the terminal event arrives.
    harness.state_mut().apply_events([completed("missing")]);
    // Then: the recovered pane is inactive.
    assert_parked(&harness, "missing");
}

#[test]
fn drill_down_reuses_existing_subagent_surface() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _rt) = workbench(temp.path());
    harness.state_mut().apply_events([started("child")]);
    harness.state_mut().drill_down("child");
    assert_eq!(
        harness.state().focus(),
        &gui::app::ConversationFocus::Thread
    );
    assert_eq!(
        harness
            .state()
            .dock()
            .iter_all_tabs()
            .filter(|(_, id)| id.as_str() == "agent-child")
            .count(),
        1
    );
    assert_eq!(pane_node(&harness, "child"), egui_dock::NodeIndex(2));
}

#[test]
fn new_run_after_park_does_not_split_agents_leaf() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _rt) = workbench(temp.path());
    harness
        .state_mut()
        .apply_events([started("a"), completed("a"), started("b")]);
    assert_eq!(pane_node(&harness, "a"), egui_dock::NodeIndex(5));
    assert_eq!(pane_node(&harness, "b"), egui_dock::NodeIndex(6));
    assert_parked(&harness, "a");
}

fn saved_workspace(
    harness: &mut HeadlessWorkbench<AgentRuntime>,
    path: &std::path::Path,
) -> workspace_ui::Workspace {
    harness.key_press(egui::Modifiers::CTRL, egui::Key::S);
    harness.run();
    workspace_ui::load_from(path).expect("saved workspace")
}

#[test]
fn parking_leaves_no_empty_nodes_in_workspace_export() {
    // Given: two running panes and an export path.
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("workspace.json");
    let (mut harness, _rt) = workbench(temp.path());
    harness
        .state_mut()
        .apply_events([started("a"), started("b")]);
    // When: both finish.
    harness
        .state_mut()
        .apply_events([completed("a"), completed("b")]);
    // Then: export has registered panels and a valid tree.
    let workspace = saved_workspace(&mut harness, &path);
    let dock = gui::dock::to_dock_state(&workspace).expect("valid tree");
    assert_eq!(
        gui::dock::from_dock_state(&dock, &workspace.panels).expect("export"),
        workspace
    );
    assert_parked(&harness, "b");
}

#[test]
fn workspace_roundtrip_preserves_parked_inactive_tab() {
    // Given: a running pane with a completed sibling.
    let temp = tempfile::tempdir().expect("temp");
    let (mut harness, _rt) = workbench(temp.path());
    harness
        .state_mut()
        .apply_events([started("a"), started("b"), completed("a")]);
    // When: the renderer tree is serialized and restored.
    let workspace = saved_workspace(&mut harness, &temp.path().join("workspace.json"));
    let restored = gui::dock::to_dock_state(&workspace).expect("restore dock");
    // Then: tab order and selection are retained.
    let path = restored
        .find_tab(&PanelId::new("agent-a"))
        .expect("parked tab");
    let leaf = restored.leaf(path.node_path()).expect("leaf");
    assert_eq!(
        leaf.tabs,
        vec![PanelId::new("agent-b"), PanelId::new("agent-a")]
    );
    assert_eq!(leaf.active.0, 0);
}

#[test]
fn background_run_is_fire_and_forget() {
    // Given: the runtime cannot progress until explicitly driven by this test.
    let temp = tempfile::tempdir().expect("temp dir");
    let (mut harness, _rt) = workbench(temp.path());
    // When: submit returns synchronously without driving the runtime.
    harness.state_mut().composer_mut().input = "/run do the thing".into();
    harness.state_mut().submit_composer();
    harness.run();
    // Then: the real runtime source contains a still-unfinished worker.
    let run = harness
        .state()
        .tasks()
        .inspect(runtime::RunId::new(1))
        .expect("background run registered in runtime source");
    assert!(matches!(
        run.phase,
        AgentRunPhase::Pending | AgentRunPhase::Running
    ));
    assert_eq!(run.role_name, "Worker");
}

#[test]
fn automatic_open_and_park_preserve_dock_focus() {
    for panel in ["agent-main", "diff-main"] {
        let temp = tempfile::tempdir().expect("temp");
        let (mut h, _rt) = workbench(temp.path());
        let path = h
            .state()
            .dock()
            .find_tab(&PanelId::new(panel))
            .expect("panel");
        h.state_mut()
            .dock_mut()
            .main_surface_mut()
            .set_focused_node(path.node);
        h.state_mut().apply_events([started("a")]);
        let expected = h
            .state()
            .dock()
            .find_tab(&PanelId::new(panel))
            .expect("panel");
        assert_eq!(
            h.state().dock().main_surface().focused_leaf(),
            Some(expected.node)
        );
        h.state_mut()
            .apply_events([started("b"), completed("a"), completed("b")]);
        let expected = h
            .state()
            .dock()
            .find_tab(&PanelId::new(panel))
            .expect("panel");
        assert_eq!(
            h.state().dock().main_surface().focused_leaf(),
            Some(expected.node)
        );
    }
}

fn assert_completion_order(h: &HeadlessWorkbench<AgentRuntime>, runs: &[&str]) {
    let dock = h.state().dock();
    let ids: Vec<_> = runs
        .iter()
        .map(|run| PanelId::new(format!("agent-{run}")))
        .collect();
    let path = dock.find_tab(&ids[0]).expect("completed pane");
    let leaf = dock.leaf(path.node_path()).expect("leaf");
    assert_eq!(&leaf.tabs[leaf.tabs.len() - ids.len()..], ids);
    for id in ids {
        let tab = dock.find_tab(&id).expect("tab");
        assert_eq!(tab.node_path(), path.node_path());
        assert_ne!(tab.tab, leaf.active);
    }
}

#[test]
fn activating_parked_tab_does_not_hide_next_completion() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut h, _rt) = workbench(temp.path());
    h.state_mut()
        .apply_events([started("a"), started("b"), completed("a")]);
    let path = h
        .state()
        .dock()
        .find_tab(&PanelId::new("agent-a"))
        .expect("a");
    h.state_mut()
        .dock_mut()
        .leaf_mut(path.node_path())
        .expect("leaf")
        .active = path.tab;
    h.state_mut().apply_events([completed("b")]);
    assert_completion_order(&h, &["a", "b"]);
}

#[test]
fn reverse_completion_order_is_preserved() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut h, _rt) = workbench(temp.path());
    h.state_mut()
        .apply_events([started("a"), started("b"), started("c")]);
    h.state_mut()
        .apply_events([completed("c"), completed("b"), completed("a")]);
    assert_completion_order(&h, &["c", "b", "a"]);
    assert_eq!(pane_node(&h, "a"), egui_dock::NodeIndex(2));
}

#[test]
fn last_completion_retains_region_before_another_spawn() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut h, _rt) = workbench(temp.path());
    h.state_mut().apply_events([started("a"), completed("a")]);
    assert_eq!(pane_node(&h, "a"), egui_dock::NodeIndex(2));
    assert_parked(&h, "a");
    h.run();
    h.state_mut().apply_events([started("b")]);
    assert_eq!(pane_node(&h, "a"), egui_dock::NodeIndex(5));
    assert_eq!(pane_node(&h, "b"), egui_dock::NodeIndex(6));
    assert_parked(&h, "a");
}

#[test]
fn parked_membership_and_order_survive_workspace_reload_and_tab_activation() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut h, _rt) = workbench(temp.path());
    h.state_mut()
        .apply_events([started("a"), started("b"), started("c"), completed("c")]);
    let workspace = saved_workspace(&mut h, &temp.path().join("workspace.json"));
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(workspace);
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(PendingModel),
    );
    let state = WorkbenchState::new(runtime, &settings).expect("restored state");
    let mut restored = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    let c = restored
        .state()
        .dock()
        .find_tab(&PanelId::new("agent-c"))
        .expect("c");
    restored
        .state_mut()
        .dock_mut()
        .leaf_mut(c.node_path())
        .expect("leaf")
        .active = c.tab;
    restored
        .state_mut()
        .apply_events([completed("b"), completed("a"), completed("c")]);
    assert_completion_order(&restored, &["c", "b", "a"]);
    restored.run();
}

#[test]
fn dragged_running_tab_is_not_swept_into_parked_tabs() {
    let temp = tempfile::tempdir().expect("temp");
    let (mut h, _rt) = workbench(temp.path());
    h.state_mut()
        .apply_events([started("a"), started("b"), started("c")]);
    let b = h
        .state()
        .dock()
        .find_tab(&PanelId::new("agent-b"))
        .expect("b");
    h.state_mut().dock_mut().remove_tab(b);
    let a = h
        .state()
        .dock()
        .find_tab(&PanelId::new("agent-a"))
        .expect("a");
    h.state_mut()
        .dock_mut()
        .leaf_mut(a.node_path())
        .expect("leaf")
        .tabs
        .push(PanelId::new("agent-b"));
    h.state_mut().apply_events([completed("a")]);
    let b = h
        .state()
        .dock()
        .find_tab(&PanelId::new("agent-b"))
        .expect("b");
    let leaf = h.state().dock().leaf(b.node_path()).expect("leaf");
    assert_eq!(leaf.tabs[leaf.active.0], PanelId::new("agent-b"));
    assert_completion_order(&h, &["a"]);
    assert_ne!(pane_node(&h, "b"), pane_node(&h, "c"));
}
