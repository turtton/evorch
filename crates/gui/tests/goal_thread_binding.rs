use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use event_bus::{
    Event, EventBus, EventKind, LifecycleEvent, MessageEvent, ProviderEvent, ToolEvent,
};
use gui::{
    app::WorkbenchState,
    fixture::DemoSource,
    headless::HeadlessWorkbench,
    model::commands::{CommandSink, GoalSubmission, WorkbenchCommand},
    runtime_sink::RuntimeCommandSink,
};
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor,
    OrchestrationSettings, Role, RunConfig, RuntimeError,
};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

struct HeldModel;

#[async_trait]
impl AgentModel for HeldModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "claude".into()
    }
}

#[test]
fn goal_root_and_children_belong_to_submitting_thread_when_another_is_active() {
    // Given: real runtime/supervisor/sink, two threads, A active.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bus = Arc::new(EventBus::new(256));
    let mut receiver = bus.subscribe();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(HeldModel),
    );
    let supervisor = rt.block_on(async {
        GoalSupervisor::spawn(
            runtime.clone(),
            bus.clone(),
            Arc::new(FixtureDeliveryAdapter::default()),
            OrchestrationSettings::default(),
        )
    });
    let mut sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor);
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", dir.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["A", "B"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("A")).unwrap();
    let mut config = gui::fixture::demo_provider_config();
    let profile = config.providers.get_mut("local").unwrap();
    profile.models[0].input_price = Some(1.0);
    profile.models[0].cache_read_price = Some(0.0);
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_settings(
            gui::model::provider_settings::ProviderSettingsModel::seed_from_config(&config),
        );

    // When: B submits through the production sink, then its root spawns children.
    for event in sink.submit(WorkbenchCommand::SubmitGoal(GoalSubmission {
        delegation_value: None,
        project_id: "project".into(),
        thread_id: "B".into(),
        goal: "direct: inspect the project".into(),
        references: Vec::new(),
        constraints: Vec::new(),
    })) {
        state.apply_loop_event(event);
    }
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = receiver.recv().await.unwrap();
                let started = matches!(
                    event.kind,
                    EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                        parent_run_id: None,
                        ..
                    })
                );
                state.apply_events([event]);
                if started {
                    break;
                }
            }
        })
        .await
        .unwrap();
    });
    let root = runtime.list_agents()[0].run_id;
    let children = rt.block_on(async {
        [Role::Worker, Role::Reviewer].map(|role| {
            runtime.spawn_reserved(
                runtime.reserve_run_id(),
                Some(root),
                role,
                "child",
                RunConfig::default(),
            )
        })
    });
    let runs = [root, children[0], children[1]].map(|id| id.to_string());
    for run in &runs {
        bus.emit(Event::new(MessageEvent::MessageDelta {
            run_id: Some(run.clone()),
            delta: format!("content-{run}"),
        }));
        bus.emit(Event::new(ToolEvent::ToolStarted {
            run_id: Some(run.clone()),
            call_id: format!("call-{run}"),
            tool_name: format!("tool-{run}"),
            input: None,
        }));
        bus.emit(Event::new(ProviderEvent::RequestCompleted {
            request_id: format!("request-{run}"),
            provider: "local".into(),
            profile: None,
            protocol: "fixture".into(),
            model: "gpt-4.1".into(),
            streaming: true,
            duration_ms: 10,
            input_tokens: 1000,
            output_tokens: 0,
            cache_read_tokens: 1000,
            cache_write_tokens: 0,
            finish_reason: "stop".into(),
            run_id: Some(run.clone()),
        }));
    }
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut billed = 0;
            while billed < 3 {
                let event = receiver.recv().await.unwrap();
                if matches!(
                    event.kind,
                    EventKind::Provider(ProviderEvent::RequestCompleted { .. })
                ) {
                    billed += 1;
                }
                state.apply_events([event]);
            }
        })
        .await
        .unwrap();
    });

    // Then: A owns neither usage nor transcript; B owns root and both children.
    assert!(state.sidebar().threads[0].run_ids.is_empty());
    assert_eq!(state.sidebar().threads[1].run_ids, runs);
    state.apply_events(runs.iter().map(|run| {
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: run.clone(),
            from: event_bus::AgentRunPhase::Running,
            to: event_bus::AgentRunPhase::Done,
            reason: None,
        })
    }));
    let mut gui = HeadlessWorkbench::new(state, [1600.0, 2000.0]);
    gui.run();
    for run in &runs {
        assert!(!gui.has_label(&format!("content-{run}")));
    }
    assert_eq!(gui.state().telemetry().thread_metrics(&[]).cost, None);
    gui.state_mut().switch_thread(ThreadId::new("B")).unwrap();
    gui.run();
    let metrics = gui.state().telemetry().thread_metrics(&runs);
    assert_eq!(metrics.cost, Some(0.0));
    let cost_label = format!("${:.3}", metrics.cost.unwrap());
    assert!(
        gui.has_label(&cost_label),
        "missing status line {cost_label}"
    );
    assert!(
        gui.has_label("cache 100%"),
        "missing status line cache 100%"
    );
    for (index, run) in runs.iter().enumerate() {
        assert_eq!(gui.has_label(&format!("content-{run}")), index == 0);
        assert_eq!(
            gui.state()
                .transcript()
                .entries()
                .iter()
                .any(|entry| matches!(
                    entry,
                    gui::model::transcript::TranscriptEntry::Tool { tool_name, .. }
                        if tool_name == &format!("tool-{run}")
                )),
            index == 0
        );
        assert!(gui.state().transcripts().run(run).unwrap().entries().iter().any(|entry|
            matches!(entry, gui::model::transcript::TranscriptEntry::Message { text, .. }
                if text == &format!("content-{run}"))));
    }
}
