use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use event_bus::{EventBus, EventKind, EventReceiver, LifecycleEvent, RoutingSource};
use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::TaskRow;
use gui::runtime_sink::RuntimeCommandSink;
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RuntimeError};
use tokio::time::timeout;
use tools::ToolExecutor;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

/// Prompt-marker keyed scripted model: the goal text becomes the first User
/// message of the launched run, so each key matches exactly one submitted goal.
struct ScriptedModel {
    scripts: Mutex<HashMap<String, VecDeque<ChatResponse>>>,
}

impl ScriptedModel {
    fn new() -> Self {
        Self {
            scripts: Mutex::new(HashMap::from([
                (
                    "direct: fix the typo in README".to_string(),
                    VecDeque::from([text_response("worker done", FinishReason::Stop)]),
                ),
                (
                    "implement issue #65".to_string(),
                    VecDeque::from([text_response("orchestrator done", FinishReason::Stop)]),
                ),
            ])),
        }
    }
}

#[async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let marker = messages
            .iter()
            .find(|message| message.role == MessageRole::User)
            .and_then(|message| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.clone()),
                    ContentBlock::Reasoning { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::ToolResult { .. } => None,
                })
            });
        let mut scripts = self.scripts.lock().expect("script lock must not poison");
        scripts
            .get_mut(marker.as_deref().unwrap_or_default())
            .and_then(VecDeque::pop_front)
            .ok_or_else(|| RuntimeError::Model {
                reason: format!("script exhausted for {marker:?}"),
            })
    }

    fn selected_model(&self, role: Role) -> String {
        format!("test-{}", role.name().to_lowercase())
    }
}

fn text_response(text: &str, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        },
        usage: Usage::default(),
        finish_reason,
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

fn activate_panel(harness: &mut HeadlessWorkbench<AgentRuntime>, panel_id: &str) {
    let dock = harness.state_mut().dock_mut();
    let path = dock
        .find_tab(&PanelId::new(panel_id))
        .expect("panel tab exists");
    let leaf = dock.leaf_mut(path.node_path()).expect("leaf exists");
    leaf.set_active_tab(path.tab.0).expect("tab index is valid");
}

/// Headless workbench wired to the real runtime through the production sink.
struct Fixture {
    runtime: tokio::runtime::Runtime,
    _temp_dir: tempfile::TempDir,
    bus: Arc<EventBus>,
    repaint_rx: mpsc::Receiver<()>,
    harness: HeadlessWorkbench<AgentRuntime>,
}

impl Fixture {
    fn new() -> Self {
        let rt = tokio::runtime::Runtime::new().expect("multi-thread test runtime");
        let bus = Arc::new(EventBus::new(256));
        let executor = Arc::new(ToolExecutor::new(Arc::clone(&bus)));
        let model = Arc::new(ScriptedModel::new());
        let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);
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
            .with_sidebar(sidebar_with_thread(temp_dir.path()))
            .with_command_sink(Box::new(RuntimeCommandSink::new(
                runtime.clone(),
                rt.handle().clone(),
            )));
        let mut harness = HeadlessWorkbench::new(state, [800.0, 600.0]);
        activate_panel(&mut harness, "goal-main");
        harness.run();
        Self {
            runtime: rt,
            _temp_dir: temp_dir,
            bus,
            repaint_rx,
            harness,
        }
    }
}

fn submit_goal(fixture: &mut Fixture, goal: &str) {
    fixture.harness.state_mut().goal_form_mut().goal = goal.into();
    fixture.harness.run();
    fixture.harness.click_label("Submit");
    fixture.harness.run();
}

/// Runs frames until a task row matching the predicate appears, or panics with
/// the current row dump after the 5s deadline.
fn wait_for_row_matching(
    fixture: &mut Fixture,
    predicate: impl Fn(&TaskRow) -> bool,
    description: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fixture
        .harness
        .state()
        .tasks()
        .rows()
        .iter()
        .any(&predicate)
    {
        assert!(
            Instant::now() < deadline,
            "{description} did not appear within 5s: {:?}",
            fixture.harness.state().tasks().rows()
        );
        let _ = fixture.repaint_rx.recv_timeout(Duration::from_millis(200));
        fixture.harness.run();
    }
}

/// Skips unrelated events until the RoutingDecision lifecycle event arrives and
/// returns its shape and source, or panics after the 5s deadline.
fn wait_for_routing_decision(
    rt: &tokio::runtime::Runtime,
    rx: &mut EventReceiver,
) -> (String, RoutingSource) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "RoutingDecision event did not arrive within 5s"
        );
        let received = rt.block_on(async { timeout(Duration::from_secs(2), rx.recv()).await });
        let event = received
            .expect("RoutingDecision event must arrive within the 2s recv timeout")
            .expect("event bus remains open");
        if let EventKind::Lifecycle(LifecycleEvent::RoutingDecision { shape, source, .. }) =
            event.kind
        {
            return (shape, source);
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
fn direct_keyword_goal_starts_worker_run_through_the_ui() {
    // Given: a workbench with the production sink and a routing subscription
    // created before submission
    // When: the direct-keyword goal is submitted through the Submit button
    let mut fixture = Fixture::new();
    let mut routing_rx = fixture.bus.subscribe();
    submit_goal(&mut fixture, "direct: fix the typo in README");

    // Then: the goal is accepted, a Worker run named goal-1 appears with no
    // Orchestrator row, and the Direct local-rule decision is published
    assert!(fixture.harness.has_label("accepted: goal-1"));
    wait_for_row_matching(
        &mut fixture,
        |row| row.role == "Worker" && row.name == "goal-1",
        "Worker row named goal-1",
    );
    assert_no_row_with_role(&fixture, "Orchestrator");
    let (shape, source) = wait_for_routing_decision(&fixture.runtime, &mut routing_rx);
    assert_eq!(shape, "Direct");
    assert_eq!(
        source,
        RoutingSource::LocalRule {
            rule: "direct-keyword:direct".into()
        }
    );
}

#[test]
fn plain_goal_starts_orchestrator_run_through_the_ui() {
    // Given: a workbench with the production sink and a routing subscription
    // created before submission
    // When: the plain goal is submitted through the Submit button
    let mut fixture = Fixture::new();
    let mut routing_rx = fixture.bus.subscribe();
    submit_goal(&mut fixture, "implement issue #65");

    // Then: an Orchestrator run named goal-1 appears with no Worker row, and
    // the Coordinated local-rule decision is published
    wait_for_row_matching(
        &mut fixture,
        |row| row.role == "Orchestrator" && row.name == "goal-1",
        "Orchestrator row named goal-1",
    );
    assert_no_row_with_role(&fixture, "Worker");
    let (shape, source) = wait_for_routing_decision(&fixture.runtime, &mut routing_rx);
    assert_eq!(shape, "Coordinated");
    assert_eq!(
        source,
        RoutingSource::LocalRule {
            rule: "no-direct-keyword".into()
        }
    );
}
