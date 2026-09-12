use std::sync::Arc;

use async_trait::async_trait;
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, EventBus, LifecycleEvent};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::{
    composer::ProviderStatus, notifications::NotificationKind, tasks::AgentRunSource,
};
use gui::runtime_sink::RuntimeCommandSink;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, AgentSummary, FixtureDeliveryAdapter,
    GoalSupervisor, OrchestrationSettings, Role, RunId, RuntimeError,
};
use tools::ToolExecutor;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

fn transition(run_id: &str, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run_id.into(),
        from: AgentRunPhase::Pending,
        to,
        reason: None,
    })
}

fn harness<S: AgentRunSource + 'static>(
    state: WorkbenchState<S>,
) -> Harness<'static, WorkbenchState<S>> {
    Harness::builder()
        .with_size(egui::vec2(1280.0, 900.0))
        .build_ui_state(
            |ui, state| state.ui(ui, &mut eframe::Frame::_new_kittest()),
            state,
        )
}

fn activate<S: AgentRunSource>(state: &mut WorkbenchState<S>, id: &str) {
    let path = state
        .dock()
        .find_tab(&PanelId::new(id))
        .expect("tab exists");
    state.dock_mut().set_active_tab(path).expect("activate tab");
}

#[test]
fn run_done_event_surfaces_unread_notification_and_read_after_display() {
    // Given: the Notifications tab is hidden in an explicitly focused viewport.
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).unwrap();
    activate(&mut state, "agents-main");
    let mut harness = harness(state);
    harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .focused = Some(true);
    // When: completion enters the same batch fold used by the frame drain.
    harness
        .state_mut()
        .apply_events([transition("run-7", AgentRunPhase::Done)]);
    harness.run_steps(3);
    // Then: it remains unread until its row is actually displayed.
    let model = harness.state().notifications();
    let item = model.items().next().expect("completion notification");
    assert_eq!(item.run_id.as_deref(), Some("run-7"));
    assert_eq!(item.kind, NotificationKind::RunCompleted);
    assert!(model.is_unread(item.id));
    assert_eq!(model.unread_count(), 1);
    activate(harness.state_mut(), "notifications-main");
    harness.run_steps(3);
    assert!(harness.query_by_label("Run run-7 completed").is_some());
    assert_eq!(harness.state().notifications().unread_count(), 0);
}

#[test]
fn notification_click_opens_run_transcript() {
    // Given: a displayed completion notification with no transcript panel open.
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).unwrap();
    state.apply_events([transition("run-X", AgentRunPhase::Done)]);
    activate(&mut state, "notifications-main");
    let mut harness = harness(state);
    harness.run_steps(3);
    let panel = PanelId::new("agent-run-X");
    assert!(harness.state().dock().find_tab(&panel).is_none());
    // When: the actual notification row is clicked.
    harness.get_by_label("Run run-X completed").click();
    harness.run_steps(3);
    // Then: the targeted transcript exists and is the active tab of its leaf.
    let dock = harness.state().dock();
    let path = dock.find_tab(&panel).expect("run transcript opened");
    assert_eq!(dock.leaf(path.node_path()).unwrap().active, path.tab);
}

#[test]
fn multiple_concurrent_runs_display_states() {
    // Given: three independently registered concurrent runs.
    let phases = [
        AgentRunPhase::Running,
        AgentRunPhase::Waiting,
        AgentRunPhase::Error,
    ];
    let runs = [(1, "running-run"), (2, "waiting-run"), (3, "error-run")]
        .into_iter()
        .zip(phases)
        .map(|((id, name), phase)| AgentSummary {
            run_id: RunId::new(id),
            name: name.into(),
            role_name: "worker".into(),
            phase,
            model: "test-model".into(),
        })
        .collect();
    let mut state = WorkbenchState::new(DemoSource(runs), &UiSettings::default()).unwrap();
    // When: a single frame batch folds all transitions and the real Tasks pane renders.
    state.apply_events(
        ["run-1", "run-2", "run-3"]
            .into_iter()
            .zip(phases)
            .map(|(id, phase)| transition(id, phase)),
    );
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 480.0))
        .build_ui_state(
            |ui, state| gui::panes::tasks::tasks_pane(ui, state.tasks()),
            state,
        );
    harness.run();
    // Then: all names, ids and distinct state indicators are visible; only Error notifies.
    for label in [
        "running-run",
        "waiting-run",
        "error-run",
        "run-1",
        "run-2",
        "run-3",
        "Running",
        "Waiting",
        "Error",
    ] {
        assert!(harness.query_by_label(label).is_some(), "missing {label}");
    }
    let model = harness.state().notifications();
    assert_eq!(model.items().count(), 1);
    let item = model.items().next().unwrap();
    assert_eq!(item.run_id.as_deref(), Some("run-3"));
    assert_eq!(item.kind, NotificationKind::RunFailed { reason: None });
    assert!(model.is_unread(item.id));
}

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

    fn selected_model(&self, _role: Role) -> String {
        "test-background".into()
    }
}

#[test]
fn background_run_then_completion_notification_end_to_end() {
    // Given: the real runtime/sink fixture from background_run_headless, without driving workers.
    let temp = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
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
    let project = ProjectId::new("demo");
    let thread = ThreadId::new("thread-1");
    sidebar
        .add_project(project.clone(), "demo", temp.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(thread.clone(), project, "thread-1")
        .unwrap();
    sidebar.switch_thread(&thread).unwrap();
    let state = WorkbenchState::new(runtime.clone(), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::Configured)
        .with_command_sink(Box::new(sink));
    let mut harness = gui::headless::HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    let focus = harness.state().focus().clone();
    let panels: Vec<_> = harness
        .state()
        .dock()
        .iter_all_tabs()
        .map(|(path, panel)| (path, panel.clone()))
        .collect();
    let active_tabs: Vec<_> = panels
        .iter()
        .map(|(path, _)| {
            harness
                .state()
                .dock()
                .leaf(path.node_path())
                .unwrap()
                .active
        })
        .collect();
    assert!(runtime.list_agents().is_empty());
    // When: composer launches a background run, then that run completes via the frame fold.
    harness.state_mut().composer_mut().input = "/run do the thing".into();
    harness.state_mut().submit_composer();
    harness.run();
    let runs = runtime.list_agents();
    assert_eq!(runs.len(), 1);
    let run_id = runs[0].run_id.to_string();
    for completed in [false, true] {
        if completed {
            harness
                .state_mut()
                .apply_events([transition(&run_id, AgentRunPhase::Done)]);
            harness.run();
        }
        // Then: neither launch nor completion steals conversation focus or changes tabs.
        assert_eq!(harness.state().focus(), &focus);
        let current: Vec<_> = harness
            .state()
            .dock()
            .iter_all_tabs()
            .map(|(path, panel)| (path, panel.clone()))
            .collect();
        assert_eq!(current, panels);
        for ((path, _), active) in panels.iter().zip(&active_tabs) {
            assert_eq!(
                &harness
                    .state()
                    .dock()
                    .leaf(path.node_path())
                    .unwrap()
                    .active,
                active
            );
        }
    }
    let model = harness.state().notifications();
    assert_eq!(model.items().count(), 1);
    let item = model.items().next().unwrap();
    assert_eq!(item.run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(item.kind, NotificationKind::RunCompleted);
    assert!(model.is_unread(item.id));
    assert_eq!(model.unread_count(), 1);
    assert!(harness.state().composer().input.is_empty());
}
