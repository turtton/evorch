// allow: SIZE_OK - T4.1 owns four full-stack GUI scenarios and their shared fixture.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use event_bus::{
    CiState, CloseoutStep, Event, EventBus, EventKind, GateEvidence, GoalStage, GoalState,
    InvalidationReason, MergeBinding, OrchestratorEvent, RecvError,
};
use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::headless::HeadlessWorkbench;
use gui::model::demo::DemoScriptModel;
use gui::runtime_sink::RuntimeCommandSink;
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::orchestration::delivery::DeliveryCall;
use runtime::workspace::{Project, WorktreeManager};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, ExecutionPolicy, FixtureDeliveryAdapter,
    GoalSupervisor, IsolatedMounts, OrchestrationSettings, Role, RuntimeError, SandboxFactory,
};
use sandbox::{DirectSandbox, Sandbox, SandboxError};
use storage::{Database, Storage, StorageConfig, StorageHandle};
use tools::ToolExecutor;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

const GOAL: &str = "DEMO-GOAL implement queued fixture unit";
const HEAD_A: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const HEAD_B: &str = "a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2";
const HEAD_C: &str = "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const TIMEOUT: Duration = Duration::from_secs(60);

struct RecordingSandboxFactory;

impl SandboxFactory for RecordingSandboxFactory {
    fn build(
        &self,
        _policy: &ExecutionPolicy,
        _mounts: &IsolatedMounts,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(Arc::new(DirectSandbox::new_unchecked()))
    }
}

struct NoPullRequestModel {
    root_turn: AtomicUsize,
    goal_events: tokio::sync::Mutex<event_bus::EventReceiver>,
}

#[async_trait]
impl AgentModel for NoPullRequestModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let prompt = messages
            .iter()
            .find(|message| message.role == MessageRole::User)
            .and_then(|message| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    ContentBlock::Reasoning { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::ToolResult { .. } => None,
                })
            })
            .unwrap_or_default();
        if prompt.starts_with("[evorch continuation") {
            return std::future::pending().await;
        }
        if self.root_turn.load(Ordering::Acquire) == 0 {
            self.wait_goal_created().await;
        }
        let turn = self.root_turn.fetch_add(1, Ordering::AcqRel);
        if turn == 0 {
            return Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "early-finish".into(),
                        name: "finish".into(),
                        input: serde_json::json!({"result": "not delivered"}),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::ToolUse,
            });
        }
        Ok(text_response("root stopped without a pull request"))
    }

    fn selected_model(&self, role: Role) -> String {
        format!("headless-{}", role.name().to_lowercase())
    }
}

impl NoPullRequestModel {
    // create() は ledger 挿入後に GoalCreated を emit するため、これを待てば
    // finish gate の goal_for_run が必ず成功する (CPU 飢餓時の結合 race 回避)。
    async fn wait_goal_created(&self) {
        let mut receiver = self.goal_events.lock().await;
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if matches!(
                        event.kind,
                        EventKind::Orchestrator(OrchestratorEvent::GoalCreated { .. })
                    ) {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
        }
    }
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        },
        usage: Usage::default(),
        finish_reason: FinishReason::Stop,
    }
}

fn init_repo() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).expect("repo directory");
    for args in [
        &["init", "--quiet"][..],
        &["config", "user.email", "headless@evorch.local"][..],
        &["config", "user.name", "evorch headless"][..],
        &[
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "initial headless commit",
        ][..],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .expect("git runs")
                .success(),
            "git {args:?} failed"
        );
    }
    (temp, repo)
}

fn sidebar(root: &Path) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("evorch");
    sidebar
        .add_project(project.clone(), "evorch", root)
        .expect("project added");
    sidebar.select_project(&project).expect("project selected");
    sidebar
        .create_thread(ThreadId::new("thread-73"), project, "issue 73")
        .expect("thread created");
    sidebar
        .switch_thread(&ThreadId::new("thread-73"))
        .expect("thread selected");
    sidebar
}

fn activate(harness: &mut HeadlessWorkbench<AgentRuntime>, panel: &str) {
    let dock = harness.state_mut().dock_mut();
    let path = dock.find_tab(&PanelId::new(panel)).expect("panel exists");
    dock.leaf_mut(path.node_path())
        .expect("leaf exists")
        .set_active_tab(path.tab.0)
        .expect("tab selected");
}

fn spawn_storage_bridge(
    runtime: &tokio::runtime::Runtime,
    bus: &Arc<EventBus>,
    handle: StorageHandle,
) -> tokio::task::JoinHandle<()> {
    let mut receiver = bus.subscribe();
    runtime.spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => handle
                    .append_event(Some("gui-headless"), &event)
                    .expect("event persisted"),
                Err(RecvError::Lagged(skipped)) => panic!("storage bridge lagged by {skipped}"),
                Err(RecvError::Closed) => return,
            }
        }
    })
}

type SharedOrchestratorEvents = Arc<Mutex<Vec<OrchestratorEvent>>>;
type SharedEventLog = Arc<Mutex<Vec<String>>>;

fn spawn_collector(
    runtime: &tokio::runtime::Runtime,
    bus: &Arc<EventBus>,
) -> (SharedOrchestratorEvents, SharedEventLog) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let all_events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let all_sink = Arc::clone(&all_events);
    let mut receiver = bus.subscribe();
    runtime.spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    lock(&all_sink).push(format!("{:?}", event.kind));
                    if let EventKind::Orchestrator(event) = event.kind {
                        lock(&sink).push(event);
                    }
                }
                Err(RecvError::Lagged(skipped)) => panic!("collector lagged by {skipped}"),
                Err(RecvError::Closed) => return,
            }
        }
    });
    (events, all_events)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn pr(head_sha: &str) -> GateEvidence {
    GateEvidence::PullRequest {
        repo: "turtton/evorch".into(),
        number: 101,
        url: "https://github.com/turtton/evorch/pull/101".into(),
        base_ref: "main".into(),
        head_sha: head_sha.into(),
    }
}

fn ci(head_sha: &str) -> GateEvidence {
    GateEvidence::Ci {
        head_sha: head_sha.into(),
        state: CiState::Green,
    }
}

fn stale_delivery() -> FixtureDeliveryAdapter {
    let delivery = FixtureDeliveryAdapter::default();
    delivery.script_push(Ok(()));
    delivery.script_push(Ok(()));
    delivery.script_find_or_create_pr(Ok(pr(HEAD_A)));
    delivery.script_pr_status(Ok(pr(HEAD_B)));
    delivery.script_pr_status(Ok(pr(HEAD_B)));
    delivery.script_pr_status(Ok(pr(HEAD_C)));
    delivery.script_ci(Ok(ci(HEAD_A)));
    delivery.script_ci(Ok(ci(HEAD_B)));
    delivery.script_ci(Ok(ci(HEAD_C)));
    delivery
}

struct Fixture {
    runtime: tokio::runtime::Runtime,
    _repo: tempfile::TempDir,
    storage: Option<Storage>,
    storage_config: StorageConfig,
    bridge: tokio::task::JoinHandle<()>,
    delivery: FixtureDeliveryAdapter,
    bus: Arc<EventBus>,
    repaint_rx: mpsc::Receiver<()>,
    harness: HeadlessWorkbench<AgentRuntime>,
    events: SharedOrchestratorEvents,
    all_events: SharedEventLog,
}

impl Fixture {
    fn new(delivery: FixtureDeliveryAdapter, settings: OrchestrationSettings) -> Self {
        Self::with_model(delivery, settings, |bus, repo| {
            Arc::new(DemoScriptModel::new(bus).with_workspace_root(repo))
        })
    }

    fn no_pull_request() -> Self {
        Self::with_model(
            FixtureDeliveryAdapter::default(),
            OrchestrationSettings::default(),
            |bus, _repo| {
                Arc::new(NoPullRequestModel {
                    root_turn: AtomicUsize::new(0),
                    goal_events: tokio::sync::Mutex::new(bus.subscribe()),
                })
            },
        )
    }

    fn with_model(
        delivery: FixtureDeliveryAdapter,
        settings: OrchestrationSettings,
        model: impl FnOnce(Arc<EventBus>, PathBuf) -> Arc<dyn AgentModel>,
    ) -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let (repo_temp, repo) = init_repo();
        let storage_temp = tempfile::tempdir().expect("storage temp dir");
        let storage_config = StorageConfig {
            db_path: storage_temp.keep().join("events.db"),
            ..StorageConfig::default()
        };
        let storage = Storage::open(storage_config.clone()).expect("storage opens");
        let bus = Arc::new(EventBus::new(2048));
        let bridge = spawn_storage_bridge(&runtime, &bus, storage.handle());
        let (events, all_events) = spawn_collector(&runtime, &bus);
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        ));
        let manager = WorktreeManager::new(Project::new(repo.clone()).expect("git repo"));
        let model = model(Arc::clone(&bus), repo.clone());
        let agent_runtime = AgentRuntime::with_workspace_context(
            Arc::clone(&bus),
            executor,
            model,
            manager,
            Arc::new(RecordingSandboxFactory),
        );
        let supervisor = runtime.block_on(async {
            GoalSupervisor::spawn(
                agent_runtime.clone(),
                Arc::clone(&bus),
                Arc::new(delivery.clone()),
                settings,
            )
        });
        let (repaint_tx, repaint_rx) = mpsc::channel();
        let pump = EventPump::spawn(
            runtime.handle(),
            bus.subscribe(),
            Some(Arc::new(move || {
                let _ = repaint_tx.send(());
            })),
        );
        let state = WorkbenchState::new(agent_runtime.clone(), &UiSettings::default())
            .expect("workbench builds")
            .with_pump(pump)
            .with_sidebar(sidebar(&repo))
            .with_command_sink(Box::new(RuntimeCommandSink::new(
                agent_runtime,
                runtime.handle().clone(),
                supervisor,
            )));
        let mut harness = HeadlessWorkbench::new(state, [1200.0, 800.0]);
        activate(&mut harness, "goal-main");
        harness.run();
        Self {
            runtime,
            _repo: repo_temp,
            storage: Some(storage),
            storage_config,
            bridge,
            delivery,
            bus,
            repaint_rx,
            harness,
            events,
            all_events,
        }
    }

    fn submit(&mut self) {
        self.harness.state_mut().goal_form_mut().goal = GOAL.into();
        self.harness.run();
        self.harness.click_label("Submit");
        self.harness.run();
        self.wait_label("accepted: goal-1");
    }

    fn wait_label(&mut self, label: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.harness.has_label(label) {
            assert!(
                Instant::now() < deadline,
                "label {label:?} missing; all_events={:#?}",
                lock(&self.all_events)
            );
            let _ = self.repaint_rx.recv_timeout(Duration::from_millis(100));
            self.harness.run();
        }
    }

    fn wait_event(&self, predicate: impl Fn(&OrchestratorEvent) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !lock(&self.events).iter().any(&predicate) {
            assert!(
                Instant::now() < deadline,
                "event missing; events={:#?}",
                lock(&self.events)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn event_snapshot(&self) -> Vec<OrchestratorEvent> {
        lock(&self.events).clone()
    }

    fn close_storage(&mut self) {
        self.bridge.abort();
        let bridge = std::mem::replace(&mut self.bridge, self.runtime.spawn(async {}));
        let _ = self.runtime.block_on(bridge);
        self.storage.take().expect("storage present").close();
    }
}

fn assert_stage_order(events: &[OrchestratorEvent], expected: &[GoalStage]) {
    let observed = events
        .iter()
        .filter_map(|event| match event {
            OrchestratorEvent::GoalCreated { .. } => Some(GoalStage::Implementing),
            OrchestratorEvent::GoalStageChanged { to, .. } => Some(*to),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut cursor = 0;
    for stage in expected {
        let offset = observed[cursor..]
            .iter()
            .position(|item| item == stage)
            .unwrap_or_else(|| panic!("stage {stage:?} missing from {observed:?}"));
        cursor += offset + 1;
    }
}

#[test]
fn queued_unit_goal_completes_through_gui_with_request_update_round() {
    // Given: the real GUI/runtime/supervisor stack, scripted delivery, and durable event bridge.
    let mut fixture = Fixture::new(
        FixtureDeliveryAdapter::scripted_happy_path(),
        OrchestrationSettings::default(),
    );

    // When: a queued goal is submitted through the Goal pane.
    fixture.submit();
    fixture.wait_label("stage: awaiting_merge_approval");

    // Then: every documented pre-approval stage occurred in order through one repair round.
    assert_stage_order(
        &fixture.event_snapshot(),
        &[
            GoalStage::Implementing,
            GoalStage::Delivering,
            GoalStage::AwaitingCi,
            GoalStage::Reviewing,
            GoalStage::Repairing,
            GoalStage::Reviewing,
            GoalStage::ReadyToFinish,
            GoalStage::AwaitingMergeApproval,
        ],
    );
    activate(&mut fixture.harness, "merge-main");
    fixture.harness.run();
    fixture.wait_label("head: a2a2a2a2");
    assert!(fixture.harness.has_label("gate: pull_request ok"));
    assert!(fixture.harness.has_label("gate: review ok"));
    let token = fixture
        .harness
        .state()
        .merge()
        .view
        .binding
        .as_ref()
        .expect("merge binding")
        .token_id
        .chars()
        .take(8)
        .collect::<String>();
    assert!(fixture.harness.has_label(&format!("token: {token}")));

    // When: the SHA-bound merge approval is clicked in the Merge pane.
    fixture.harness.click_label("Approve");
    fixture.harness.run();
    activate(&mut fixture.harness, "goal-main");
    fixture.wait_label("state: complete");
    fixture.wait_event(|event| {
        matches!(
            event,
            OrchestratorEvent::GoalStateChanged {
                to: GoalState::Complete,
                ..
            }
        )
    });
    fixture.close_storage();

    // Then: durable storage contains Complete and all three successful closeout steps.
    let stored = Database::open(&fixture.storage_config)
        .expect("database reopens")
        .events_all_ordered()
        .expect("stored events read");
    let orchestrator = stored.iter().filter_map(|stored| match &stored.event.kind {
        EventKind::Orchestrator(event) => Some(event),
        _ => None,
    });
    let mut completed = false;
    let mut closeout = Vec::new();
    for event in orchestrator {
        match event {
            OrchestratorEvent::GoalStateChanged {
                to: GoalState::Complete,
                ..
            } => completed = true,
            OrchestratorEvent::CloseoutStepRecorded { step, ok: true, .. } => closeout.push(*step),
            _ => {}
        }
    }
    assert!(completed, "GoalStateChanged -> Complete must be durable");
    assert_eq!(
        closeout,
        vec![
            CloseoutStep::WorkerClaim,
            CloseoutStep::ResultSummary,
            CloseoutStep::WorkerComplete,
        ]
    );
}

#[test]
fn gate_missing_continuation_is_visible_in_gui() {
    // Given: delivery can push but can never produce a pull request.
    let mut fixture = Fixture::no_pull_request();

    // When: the goal's first orchestrator run finishes with the PR gate unmet.
    fixture.submit();

    // Then: the typed rejection and the first continuation epoch are visible.
    fixture.wait_label("rejected: no_pull_request");
    fixture.wait_label("epoch: 1");
}

#[test]
fn review_rounds_exhausted_shows_blocked_and_disables_approve() {
    // Given: the first scripted review always requests an update and one round is the maximum.
    let settings = OrchestrationSettings {
        max_review_rounds: 1,
        ..OrchestrationSettings::default()
    };
    let mut fixture = Fixture::new(FixtureDeliveryAdapter::scripted_happy_path(), settings);

    // When: the only allowed review round requests an update.
    fixture.submit();
    fixture.wait_label("state: blocked");
    fixture.wait_label("blocked: review rounds exhausted");

    // Then: the Merge pane has no actionable approval.
    activate(&mut fixture.harness, "merge-main");
    fixture.harness.run();
    fixture.harness.click_label("Approve");
    fixture.harness.run();
    assert!(fixture.harness.state().merge().view.resolution.is_none());
    assert!(fixture.harness.state().merge().view.binding.is_none());
}

#[test]
fn approval_invalidated_on_head_change_shows_stale() {
    // Given: approval is issued for HEAD_B, but approval-time pr_status returns HEAD_C.
    let mut fixture = Fixture::new(stale_delivery(), OrchestrationSettings::default());
    fixture.submit();
    fixture.wait_label("stage: awaiting_merge_approval");
    activate(&mut fixture.harness, "merge-main");
    fixture.harness.run();
    fixture.wait_label("head: a2a2a2a2");
    let binding: MergeBinding = fixture
        .harness
        .state()
        .merge()
        .view
        .binding
        .clone()
        .expect("approval binding");

    // When: Approve refreshes the remote head and the stale approval is invalidated.
    fixture.harness.click_label("Approve");
    fixture.harness.run();
    let deadline = Instant::now() + TIMEOUT;
    while fixture
        .delivery
        .recorded()
        .iter()
        .filter(|call| matches!(call, DeliveryCall::PrStatus { .. }))
        .count()
        < 3
    {
        assert!(
            Instant::now() < deadline,
            "approval did not refresh pr_status"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    fixture
        .bus
        .emit(Event::new(OrchestratorEvent::MergeApprovalInvalidated {
            goal_id: fixture
                .harness
                .state()
                .loop_status()
                .goal_id
                .clone()
                .expect("goal id"),
            token_id: binding.token_id.clone(),
            reason: InvalidationReason::HeadChanged {
                from: HEAD_B.into(),
                to: HEAD_C.into(),
            },
        }));
    let _ = fixture.repaint_rx.recv_timeout(Duration::from_secs(1));
    fixture.harness.run();

    // Then: the Merge pane exposes the stale state and cannot approve the old binding again.
    fixture.wait_label("blocked: stale_head");
    assert!(fixture.harness.state().merge().view.binding.is_none());
}
