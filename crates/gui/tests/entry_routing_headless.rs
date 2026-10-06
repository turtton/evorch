use std::sync::{Arc, mpsc};

use async_trait::async_trait;
use event_bus::{
    AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent, OrchestratorEvent,
    ThreadGoalSnapshot,
};
use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::headless::HeadlessWorkbench;
use gui::runtime_sink::RuntimeCommandSink;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor,
    OrchestrationSettings, Role, RunId, RuntimeError, SupervisorHandle,
};
use tools::ToolExecutor;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

/// 最初の通常メッセージだけに応答し、goal 作業は完了させない stub モデル。
///
/// 入力時の goal 登録・会話 root の選択だけを検証するため、完了処理は行わない。
struct HeldModel;

#[async_trait]
impl AgentModel for HeldModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        if messages.last().is_some_and(|message| {
            message.content.iter().any(|block| {
                matches!(block, providers::ContentBlock::Text { text } if text == "Investigate the issue")
            })
        }) {
            return Ok(ChatResponse {
                message: Message {
                    role: providers::Role::Assistant,
                    content: vec![providers::ContentBlock::Text {
                        text: "Ready to investigate".into(),
                    }],
                },
                usage: providers::Usage::default(),
                finish_reason: providers::FinishReason::Stop,
            });
        }
        std::future::pending().await
    }

    fn selected_model(&self, role: Role, _category: Option<&str>) -> String {
        format!("test-{}", role.name().to_lowercase())
    }
}

fn sidebar_with_thread(root: &std::path::Path) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project can be added");
    sidebar
        .select_project(&project_id)
        .expect("project can be selected");
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .expect("thread can be created");
    sidebar
        .switch_thread(&ThreadId::new("thread-1"))
        .expect("thread can be selected");
    sidebar
}

/// Headless workbench wired to the real runtime through the production sink.
struct Fixture {
    runtime: tokio::runtime::Runtime,
    _temp_dir: tempfile::TempDir,
    bus: Arc<EventBus>,
    agent_runtime: AgentRuntime,
    supervisor: SupervisorHandle,
    repaint_rx: mpsc::Receiver<()>,
    harness: HeadlessWorkbench<AgentRuntime>,
}

impl Fixture {
    fn new() -> Self {
        let rt = tokio::runtime::Runtime::new().expect("multi-thread test runtime");
        let bus = Arc::new(EventBus::new(256));
        let executor = Arc::new(ToolExecutor::new(Arc::clone(&bus)));
        let model = Arc::new(HeldModel);
        let runtime =
            AgentRuntime::new(Arc::clone(&bus), executor, model).with_sequential_run_ids();
        let supervisor = rt.block_on(async {
            GoalSupervisor::spawn(
                runtime.clone(),
                Arc::clone(&bus),
                Arc::new(FixtureDeliveryAdapter::default()),
                OrchestrationSettings::default(),
            )
        });
        let (repaint_tx, repaint_rx) = mpsc::channel();
        let pump = EventPump::spawn(
            rt.handle(),
            bus.subscribe(),
            Some(Arc::new(move || {
                let _ = repaint_tx.send(());
            })),
        );
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let state = WorkbenchState::new(runtime.clone(), &UiSettings::default())
            .expect("default state builds")
            .with_pump(pump)
            .with_provider_status(gui::model::composer::ProviderStatus::Configured)
            .with_sidebar(sidebar_with_thread(temp_dir.path()))
            .with_command_sink(Box::new(RuntimeCommandSink::new(
                runtime.clone(),
                rt.handle().clone(),
                supervisor.clone(),
            )));
        let mut harness = HeadlessWorkbench::new(state, [800.0, 600.0]);
        harness.run();
        Self {
            runtime: rt,
            _temp_dir: temp_dir,
            bus,
            agent_runtime: runtime,
            supervisor,
            repaint_rx,
            harness,
        }
    }

    fn wait_for_goal_ui(&mut self, goal: &ThreadGoalSnapshot) {
        while !self.harness.has_label(&format!("Goal: {}", goal.objective)) {
            self.repaint_rx.recv().expect("event pump remains alive");
            self.harness.run();
        }
        assert!(
            self.harness
                .has_label(&format!("accepted: {}", goal.goal_id))
        );
        assert!(self.supervisor.snapshot(&goal.goal_id).is_none());
        assert_no_row_with_role(self, "Worker");
        assert_no_row_with_role(self, "Orchestrator");
    }

    fn stop(&self, root: &str) {
        let root = root.parse::<RunId>().unwrap();
        self.agent_runtime.cancel(root).expect("cancel held root");
        self.runtime
            .block_on(self.agent_runtime.wait(root))
            .expect("held root stops");
    }
}

fn submit_goal(fixture: &mut Fixture, goal: &str) {
    fixture.harness.state_mut().composer_mut().input = format!("/goal {goal}");
    fixture.harness.run();
    while !fixture.harness.has_label("Send") {
        fixture.repaint_rx.recv().expect("event pump remains alive");
        fixture.harness.run();
    }
    fixture.harness.click_label("Send");
    fixture.harness.run();
}

/// The goal registration is the completion signal for the UI's submission.
/// Legacy PR goals and keyword routing must not intercept this generic entry.
fn wait_for_goal(rt: &tokio::runtime::Runtime, rx: &mut EventReceiver) -> ThreadGoalSnapshot {
    loop {
        let event = rt.block_on(rx.recv()).expect("event bus remains open");
        match event.kind {
            EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) => {
                return snapshot;
            }
            EventKind::Orchestrator(OrchestratorEvent::GoalCreated { .. })
            | EventKind::Lifecycle(LifecycleEvent::RoutingDecision { .. }) => {
                panic!("generic goal submission entered legacy PR routing");
            }
            _ => {}
        }
    }
}

fn assert_no_row_with_role(fixture: &Fixture, role: &str) {
    let rows = fixture.harness.state().tasks().rows();
    assert!(
        !rows.iter().any(|row| row.role == role),
        "unexpected {role} row: {rows:?}"
    );
}

#[test]
fn direct_keyword_goal_registers_a_generic_conversation_objective() {
    let mut fixture = Fixture::new();
    let mut goal_rx = fixture.bus.subscribe();
    let objective = "direct: fix the typo in README";
    submit_goal(&mut fixture, objective);

    let goal = wait_for_goal(&fixture.runtime, &mut goal_rx);
    assert_eq!(goal.thread_id, "thread-1");
    assert_eq!(goal.objective, objective);
    assert_eq!(goal.original_request, objective);
    assert_eq!(goal.criteria, [objective]);
    assert!(!goal.review_enabled);
    fixture.wait_for_goal_ui(&goal);
    fixture.stop(&goal.root_run_id);
}

#[test]
fn plain_goal_reuses_an_existing_worker_conversation_root() {
    let mut fixture = Fixture::new();
    let mut goal_rx = fixture.bus.subscribe();
    fixture.harness.state_mut().composer_mut().input = "Investigate the issue".into();
    fixture.harness.run();
    fixture.harness.click_label("Send");
    fixture.harness.run();
    let root = loop {
        let event = fixture.runtime.block_on(goal_rx.recv()).unwrap();
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
            run_id,
            parent_run_id,
            role,
            ..
        }) = event.kind
        {
            assert!(parent_run_id.is_none());
            assert_eq!(role, "worker");
            break run_id;
        }
    };

    let objective = "implement issue #65";
    loop {
        let event = fixture.runtime.block_on(goal_rx.recv()).unwrap();
        if matches!(event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. }) if run_id == root)
        {
            break;
        }
    }
    submit_goal(&mut fixture, objective);
    let goal = wait_for_goal(&fixture.runtime, &mut goal_rx);
    assert_eq!(goal.root_run_id, root, "goal must retain the conversation");
    assert_eq!(goal.objective, objective);
    assert_eq!(goal.original_request, objective);
    fixture.wait_for_goal_ui(&goal);
    let root_id = root.parse::<RunId>().unwrap();
    assert_eq!(
        fixture
            .agent_runtime
            .inspect_agent(root_id)
            .unwrap()
            .role_name,
        Role::Worker.name(),
        "goal creation must preserve the existing conversation role"
    );
    fixture.stop(&root);
}
