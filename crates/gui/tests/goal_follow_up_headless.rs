use std::sync::{Arc, Mutex};
use std::time::Duration;

use event_bus::{
    AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent, OrchestratorEvent,
};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::composer::ProviderStatus;
use gui::runtime_sink::RuntimeCommandSink;
use providers::Message;
use runtime::{AgentRuntime, FixtureDeliveryAdapter, GoalSupervisor, OrchestrationSettings, RunId};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

#[path = "support/goal_capture_model.rs"]
mod goal_capture_model;
use goal_capture_model::CaptureModel;

struct Fixture {
    state: WorkbenchState<DemoSource>,
    sink: RuntimeCommandSink,
    runtime: AgentRuntime,
    rt: tokio::runtime::Runtime,
    events: EventReceiver,
    messages: Arc<Mutex<Vec<Vec<Message>>>>,
    created: usize,
    _storage: storage::Storage,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_success(false)
    }

    fn with_success(succeeds: bool) -> Self {
        Self::with_gate(succeeds, None)
    }

    fn with_gate(succeeds: bool, gate: Option<Arc<tokio::sync::Notify>>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("test.sqlite3"),
            ..storage::StorageConfig::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let bus = Arc::new(EventBus::new(512));
        let events = bus.subscribe();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            Arc::new(CaptureModel {
                messages: messages.clone(),
                succeeds,
                gate,
            }),
        )
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let supervisor = rt.block_on(async {
            GoalSupervisor::spawn(
                runtime.clone(),
                bus,
                Arc::new(FixtureDeliveryAdapter::default()),
                OrchestrationSettings {
                    max_continuations: 0,
                    ..OrchestrationSettings::default()
                },
            )
        });
        let sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone(), supervisor);
        let mut sidebar = SidebarState::default();
        let project = ProjectId::new("test");
        let thread = ThreadId::new("thread-1");
        sidebar
            .add_project(project.clone(), "test", directory.path())
            .unwrap();
        sidebar.select_project(&project).unwrap();
        sidebar
            .create_thread(thread.clone(), project, "test")
            .unwrap();
        sidebar.switch_thread(&thread).unwrap();
        let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar)
            .with_provider_status(ProviderStatus::Configured);
        Self {
            state,
            sink,
            runtime,
            rt,
            events,
            messages,
            created: 0,
            _storage: storage,
            _directory: directory,
        }
    }

    fn submit(&mut self, input: &str) -> Vec<LoopEvent> {
        self.state.composer_mut().input = input.into();
        self.state.submit_composer();
        self.sink
            .submit(self.state.issued().last().unwrap().clone())
    }

    fn terminal(&mut self) -> RunId {
        self.wait_phase(AgentRunPhase::Error)
    }

    fn wait_phase(&mut self, phase: AgentRunPhase) -> RunId {
        let id = self.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match self.events.recv().await.unwrap().kind {
                        EventKind::Orchestrator(OrchestratorEvent::GoalCreated { .. }) => {
                            self.created += 1
                        }
                        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                            run_id,
                            to,
                            ..
                        }) if to == phase => break run_id,
                        _ => {}
                    }
                }
            })
            .await
            .unwrap()
        });
        self.runtime
            .list_agents()
            .into_iter()
            .find(|run| run.run_id.to_string() == id)
            .unwrap()
            .run_id
    }
}

#[test]
fn follow_up_to_running_goal_is_consumed_before_root_finishes() {
    // Given: the goal's first completion is in flight.
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut fixture = Fixture::with_gate(true, Some(gate.clone()));
    fixture.submit("/goal fixture feature");
    let root = fixture.wait_phase(AgentRunPhase::Running);
    // When: ordinary chat is sent before the noninteractive root stops.
    let events = fixture.submit("follow-up while running");
    gate.notify_one();
    fixture.wait_phase(AgentRunPhase::Done);
    // Then: the goal root consumed the follow-up instead of dropping its inbox.
    assert!(
        matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == root.to_string())
    );
    assert_eq!(fixture.messages.lock().unwrap().len(), 2);
}

#[test]
fn follow_up_to_done_goal_restores_context_and_live_follow_up_reuses_root() {
    // Given: a completed goal root with assistant history.
    let mut fixture = Fixture::with_success(true);
    fixture.submit("/goal fixture feature");
    let root = fixture.wait_phase(AgentRunPhase::Done);
    // When: two plain follow-ups resume the terminal root then address the live root.
    let first = fixture.submit("continue");
    assert_eq!(fixture.wait_phase(AgentRunPhase::Waiting), root);
    let second = fixture.submit("continue again");
    assert_eq!(fixture.wait_phase(AgentRunPhase::Waiting), root);
    // Then: both are routed to the original root and all user turns remain present.
    for events in [first, second] {
        assert!(
            matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == root.to_string())
        );
    }
    assert_eq!(fixture.created, 1);
    let requests = fixture.messages.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let user_turns = requests[2]
        .iter()
        .filter(|message| message.role == providers::Role::User)
        .count();
    assert_eq!(user_turns, 3);
}

#[test]
fn follow_up_to_errored_goal_thread_resumes_goal_context() {
    // Given: an explicit goal root has failed after consuming its initial context.
    let mut fixture = Fixture::new();
    fixture.submit("/goal implement fixture feature");
    let root = fixture.terminal();
    let original = fixture.messages.lock().unwrap()[0].clone();
    // When: the user sends ordinary continuation text through the GUI composer.
    let events = fixture.submit("continue with the earlier constraints");
    fixture.terminal();
    // Then: the provider receives the goal history, not a detached fresh chat.
    let requests = fixture.messages.lock().unwrap();
    assert!(
        requests[1].starts_with(&original),
        "goal root context must be restored: {requests:#?}"
    );
    assert!(requests[1].len() > original.len());
    assert_eq!(fixture.created, 1);
    assert!(
        matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == root.to_string())
    );
}

#[test]
fn plain_chat_never_increments_accepted_goals() {
    // Given: a fresh GUI composer and real runtime sink.
    let mut fixture = Fixture::new();
    // When: ordinary chat is submitted twice.
    for text in ["implement this feature", "continue"] {
        assert!(matches!(
            fixture.submit(text).as_slice(),
            [LoopEvent::ChatAccepted { .. }]
        ));
        fixture.terminal();
    }
    // Then: no supervisor goal exists and the first explicit goal remains goal-1.
    assert_eq!(fixture.created, 0);
    assert!(
        fixture
            .state
            .issued()
            .iter()
            .all(|command| matches!(command, WorkbenchCommand::SendChat(_)))
    );
    assert!(
        matches!(fixture.submit("/goal explicit").as_slice(), [LoopEvent::GoalAccepted { goal_id, .. }] if goal_id == "goal-1")
    );
    fixture.terminal();
    assert_eq!(fixture.created, 1);
}

#[test]
fn explicit_goal_submission_increments_goal_counter() {
    // Given: a fresh GUI composer and real runtime sink.
    let mut fixture = Fixture::new();
    // When: two explicit /goal commands are submitted.
    for index in 1..=2 {
        let events = fixture.submit("/goal explicit");
        fixture.terminal();
        // Then: explicit submissions retain sequential goal-N numbering.
        assert!(
            matches!(events.as_slice(), [LoopEvent::GoalAccepted { goal_id, .. }] if *goal_id == format!("goal-{index}"))
        );
    }
    assert_eq!(fixture.created, 2);
}
