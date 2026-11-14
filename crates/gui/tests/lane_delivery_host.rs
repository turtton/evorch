//! ADR 0044 Phase 1c: the GUI's lane delivery host against the real runtime, storage and
//! `RuntimeCommandSink`. The runtime owns delivery; the host only says who a thread is.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use event_bus::lane::{LaneDeliveryBinding, QuestionResponder};
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent};
use gui::{
    app::WorkbenchState,
    fixture::DemoSource,
    model::commands::{ChatContinuation, ChatSubmission, CommandSink, LoopEvent, WorkbenchCommand},
    model::composer::ComposerRole,
    runtime_sink::RuntimeCommandSink,
};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::lane_delivery::LaneDeliveryOutcome;
use runtime::ownership::OwnerHost;
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RunStore,
    RuntimeError,
};
use storage::lane_delivery::LaneDeliveryNack;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

#[path = "support/short_private_directory.rs"]
mod short_private_directory;
use short_private_directory::ShortPrivateDirectory;

const LEAD: &str = "lead";
const TITLE: &str = "Which task scope?";

/// A root listed in `finishing` ends its turn with `finish`; one with a scripted tool call
/// makes it once; every other turn only says it is idle and waits.
#[derive(Default)]
struct Model {
    finishing: Mutex<HashSet<String>>,
    scripted: Mutex<HashMap<String, ContentBlock>>,
    requests: Mutex<HashMap<String, usize>>,
    /// Every message each root was sent, serialized, in request order.
    texts: Mutex<HashMap<String, Vec<String>>>,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        *self
            .requests
            .lock()
            .unwrap()
            .entry(invocation.run_id.clone())
            .or_default() += 1;
        self.texts
            .lock()
            .unwrap()
            .entry(invocation.run_id.clone())
            .or_default()
            .push(serde_json::to_string(messages).unwrap());
        let scripted = self.scripted.lock().unwrap().remove(&invocation.run_id);
        let finish = self.finishing.lock().unwrap().contains(&invocation.run_id);
        let tool = scripted.or_else(|| {
            finish.then(|| ContentBlock::ToolUse {
                id: format!("finish-{}", messages.len()),
                name: "finish".into(),
                input: serde_json::json!({"result":"complete"}),
            })
        });
        let tool_use = tool.is_some();
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![tool.unwrap_or(ContentBlock::Text {
                    text: "idle".into(),
                })],
            },
            usage: Default::default(),
            finish_reason: if tool_use {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "lane-delivery".into()
    }
}

fn brief() -> event_bus::LaneBrief {
    event_bus::LaneBrief {
        verification_requirements: Default::default(),
        objective: "an independent deliverable".into(),
        inputs: vec![],
        deliverables: vec!["artifact".into()],
        acceptance_criteria: vec!["review".into()],
        constraints: vec![],
        workspace: Default::default(),
        report_policy: event_bus::LanePolicy::Attached,
    }
}

struct World {
    runtime: AgentRuntime,
    storage: storage::Storage,
    model: Arc<Model>,
    events: EventReceiver,
    bus: Arc<EventBus>,
    /// Questions the workbench would have been told about, as the bus announced them.
    announced: Vec<event_bus::UserQuestion>,
    /// Every event received and not yet handed to a workbench.
    seen: Vec<event_bus::Event>,
    config: storage::StorageConfig,
    _dir: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        let dir = test_socket_dir::tempdir();
        let config = storage::StorageConfig {
            db_path: dir.path().join("delivery.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(16384));
        let events = bus.subscribe();
        let model = Arc::new(Model::default());
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            model.clone(),
        )
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        Self {
            runtime,
            storage,
            model,
            events,
            bus,
            announced: Vec::new(),
            seen: Vec::new(),
            config,
            _dir: dir,
        }
    }

    fn sink(&self, owner: Option<&Arc<OwnerHost>>, bound: bool) -> RuntimeCommandSink {
        let mut sink =
            RuntimeCommandSink::new(self.runtime.clone(), tokio::runtime::Handle::current());
        if let Some(owner) = owner {
            sink = sink.with_ownership(owner.clone());
        }
        if bound {
            sink = sink.with_lane_delivery();
        }
        sink
    }

    fn owner(&self, root: &std::path::Path) -> Arc<OwnerHost> {
        Arc::new(OwnerHost::open(root, Default::default(), self.bus.clone()).unwrap())
    }

    async fn phase(&mut self, run: RunId, wanted: AgentRunPhase) {
        loop {
            let event = self.next_event().await;
            if matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
                if *run_id == run.to_string() && *to == wanted)
            {
                return;
            }
        }
    }

    async fn waiting_after_request(&mut self, run: RunId, before: usize) {
        loop {
            // Another run's wait can already have consumed this run's Waiting
            // event. Check its state, but require a new request so an earlier
            // idle turn cannot satisfy the wait before delivery starts.
            let requested = self.model.requests.lock().unwrap()[&run.to_string()] > before;
            if requested && self.runtime.inspect_agent(run).unwrap().phase == AgentRunPhase::Waiting
            {
                return;
            }
            self.next_event().await;
        }
    }

    /// Waits until every run in `runs` has reached `wanted`, in whatever order they do.
    async fn all_phase(&mut self, runs: &[RunId], wanted: AgentRunPhase) {
        let mut pending: HashSet<String> = runs.iter().map(ToString::to_string).collect();
        while !pending.is_empty() {
            let event = self.next_event().await;
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
                &event.kind
                && *to == wanted
            {
                pending.remove(run_id);
            }
        }
    }

    async fn next_event(&mut self) -> event_bus::Event {
        let event = self.events.recv().await.unwrap();
        if let EventKind::Tool(event_bus::ToolEvent::UserQuestionUpdated { question }) = &event.kind
        {
            self.announced.push(question.clone());
        }
        self.seen.push(event.clone());
        event
    }

    /// Receives events until the runtime announces a change of a lane delivery state.
    async fn until_state_changed(&mut self, code: &str, thread: &str) {
        loop {
            let event = self.next_event().await;
            if matches!(&event.kind, EventKind::Diagnostic(d)
                if d.source == "lane_delivery" && d.code == code && d.thread_id.as_deref() == Some(thread))
            {
                return;
            }
        }
    }

    /// A workbench that shows this world's lanes from the real ledger, with the lead's thread.
    fn workbench(&self) -> WorkbenchState<DemoSource> {
        let mut sidebar = SidebarState::default();
        let project = ProjectId::new("project");
        sidebar
            .add_project(project.clone(), "project", self._dir.path())
            .unwrap();
        sidebar.select_project(&project).unwrap();
        sidebar
            .create_thread(ThreadId::new(LEAD), project, "Lead")
            .unwrap();
        sidebar.switch_thread(&ThreadId::new(LEAD)).unwrap();
        let inline: gui::app::lanes::Spawner = Arc::new(|job| job());
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar)
            .with_provider_status(gui::model::composer::ProviderStatus::Configured)
            .with_lane_ledger(Arc::new(gui::app::lanes::StorageLaneReader::new(
                self.config.db_path.clone(),
            )))
            .with_lane_spawner(inline)
    }

    /// Hands the events received so far to `state` and lets its ledger reads finish.
    fn show(&mut self, state: &mut WorkbenchState<DemoSource>) {
        state.apply_events(std::mem::take(&mut self.seen));
        for _ in 0..3 {
            state.pump_lanes();
        }
    }

    /// A runtime started over the same storage, as after a restart: it knows no goal binding.
    fn restarted_runtime(&self) -> AgentRuntime {
        AgentRuntime::new(
            self.bus.clone(),
            Arc::new(tools::ToolExecutor::new(self.bus.clone())),
            Arc::new(Model::default()),
        )
        .with_run_store(RunStore::open(&self.config, self.storage.handle()).unwrap())
    }

    /// Ends `run` through its own `finish` tool call.
    async fn finish(&self, run: RunId) {
        self.model.finishing.lock().unwrap().insert(run.to_string());
        self.runtime
            .send_message(run, "finish independently".into())
            .unwrap();
        self.runtime.wait(run).await.unwrap();
        self.model
            .finishing
            .lock()
            .unwrap()
            .remove(&run.to_string());
        assert_eq!(
            self.runtime.inspect_agent(run).unwrap().phase,
            AgentRunPhase::Done
        );
    }

    /// The lane's root asks `TITLE` with `ask_user` and goes back to waiting.
    async fn lane_asks(&mut self, lane: &runtime::lanes::LaneOpened) {
        self.model.scripted.lock().unwrap().insert(
            lane.root_run_id.to_string(),
            ContentBlock::ToolUse {
                id: "ask".into(),
                name: "ask_user".into(),
                input: serde_json::json!({"title": TITLE, "blocking": false}),
            },
        );
        self.runtime
            .send_message(lane.root_run_id, "ask the managing lead".into())
            .unwrap();
        self.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    }

    fn question(&self, thread: &str) -> event_bus::UserQuestion {
        let page = self
            .storage
            .handle()
            .lane_questions(thread.into(), None)
            .unwrap();
        page.questions.into_iter().next().expect("a lane question")
    }

    /// A drive that another flight (the runtime's own trigger) may be holding is repeated
    /// until it is ours: every flight ends, so this is bounded by the runtime, not a clock.
    async fn drive(&self, thread: &str) -> LaneDeliveryOutcome {
        loop {
            match self.runtime.drive_lane_deliveries(thread).await.unwrap() {
                LaneDeliveryOutcome::Busy => tokio::task::yield_now().await,
                outcome => return outcome,
            }
        }
    }
}

fn open(world: &World, lead: RunId, key: &str) -> runtime::lanes::LaneOpened {
    world.runtime.open_lane(lead, key, brief()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lead_that_became_waiting_before_the_lane_is_not_waited_for_twice() {
    fn ready(future: impl std::future::Future<Output = ()>) -> bool {
        let mut future = std::pin::pin!(future);
        future
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_ready()
    }

    let mut world = World::new();
    let mut sink = world.sink(None, true);
    let lead = start_lead(&mut sink);
    // Force the lead's Waiting event to be consumed before the lane's event,
    // as can happen while lane_asks waits for its own run on the shared bus.
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = open(&world, lead, "lead-first");
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;

    let completed_requests = world.model.requests.lock().unwrap()[&lead.to_string()];
    assert!(completed_requests > 0);
    assert!(
        ready(world.waiting_after_request(lead, 0)),
        "the lead is already waiting; consuming its event while waiting for a lane must not lose completion"
    );
    assert!(
        !ready(world.waiting_after_request(lead, completed_requests)),
        "the previous Waiting state must not stand in for a new provider request"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bound_host_restores_a_finished_lead_for_an_attached_lane_question() {
    let mut world = World::new();
    let owners = ShortPrivateDirectory::new();
    let owner = world.owner(owners.path());
    let mut sink = world.sink(Some(&owner), true);
    assert_eq!(
        world.runtime.lane_delivery_binding(),
        LaneDeliveryBinding::Bound
    );
    assert!(sink.lane_delivery_bound());

    owner.start(LEAD).unwrap();
    let accepted = sink.submit(WorkbenchCommand::SendChat(ChatSubmission {
        submission_id: String::new(),
        project_root: None,
        composer_role: ComposerRole::Orchestrator,
        images: Vec::new(),
        thread_id: LEAD.into(),
        text: "coordinate".into(),
        skills: Vec::new(),
        model_preference: None,
        fork_seed: None,
    }));
    let Some(LoopEvent::ChatAccepted { run_id, .. }) = accepted.first() else {
        panic!("the lead was not accepted: {accepted:?}");
    };
    let lead: RunId = run_id.parse().unwrap();
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = open(&world, lead, "bound-lane");
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    world.finish(lead).await;
    let before = world.model.requests.lock().unwrap()[&lead.to_string()];

    world.lane_asks(&lane).await;
    world.waiting_after_request(lead, before).await;

    let question = world.question(LEAD);
    assert!(matches!(
        question.responder,
        Some(QuestionResponder::Lead { .. })
    ));
    assert!(
        !question.is_user_visible(),
        "a lead-routed question is not in the user's list"
    );
    assert!(
        world.model.requests.lock().unwrap()[&lead.to_string()] > before,
        "the finished lead ran again to receive the notification"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_owned_by_another_window_is_nacked_and_its_notification_stays() {
    let mut world = World::new();
    let owners = ShortPrivateDirectory::new();
    let ours = world.owner(owners.path());
    let theirs = world.owner(owners.path());
    let _sink = world.sink(Some(&ours), true);

    // The other window started and owns the lead; this window never bound it.
    let lead = world
        .runtime
        .delegate_chat(
            LEAD,
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                ownership: Some(theirs.start(LEAD).unwrap()),
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = open(&world, lead, "readonly-lane");
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    world.finish(lead).await;
    world.lane_asks(&lane).await;

    assert_eq!(
        world.drive(LEAD).await,
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::Readonly)
    );
    assert_eq!(
        world.runtime.inspect_agent(lead).unwrap().phase,
        AgentRunPhase::Done,
        "a thread this window cannot act for is not restored"
    );
    let pending = world
        .storage
        .handle()
        .pending_lane_delivery_destinations(String::new())
        .unwrap();
    assert_eq!(pending, [LEAD], "the notification stays durable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unbound_thread_resolves_through_the_runtimes_saved_root_not_a_name() {
    let mut world = World::new();
    let _sink = world.sink(None, true);

    // Started outside this window's sink, as after a restart: only the runtime knows the root.
    let lead = world
        .runtime
        .delegate_chat(
            LEAD,
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = open(&world, lead, "saved-root-lane");
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let before = world.model.requests.lock().unwrap()[&lead.to_string()];
    world.lane_asks(&lane).await;
    world.waiting_after_request(lead, before).await;

    assert!(
        world.model.requests.lock().unwrap()[&lead.to_string()] > before,
        "the lead the runtime saved for this thread received the notification"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_without_any_saved_root_is_unknown_and_nothing_is_guessed() {
    let world = World::new();
    let _sink = world.sink(None, true);
    assert_eq!(
        world.drive("ghost").await,
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::UnknownThread)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_bound_host_an_attached_lane_question_comes_to_the_user() {
    let mut world = World::new();
    let sink = world.sink(None, false);
    assert!(!sink.lane_delivery_bound());
    assert_eq!(
        world.runtime.lane_delivery_binding(),
        LaneDeliveryBinding::Unbound
    );
    let lead = world
        .runtime
        .delegate_chat(
            LEAD,
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = open(&world, lead, "unbound-lane");
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    world.lane_asks(&lane).await;

    let questions: Vec<_> = world
        .announced
        .iter()
        .filter(|question| question.title == TITLE)
        .collect();
    let [question] = questions.as_slice() else {
        panic!("one question expected: {questions:?}");
    };
    assert!(question.is_user_visible());
    assert!(!matches!(
        question.responder,
        Some(QuestionResponder::Lead { .. })
    ));
}

fn start_lead(sink: &mut RuntimeCommandSink) -> RunId {
    let accepted = sink.submit(WorkbenchCommand::SendChat(ChatSubmission {
        submission_id: String::new(),
        project_root: None,
        composer_role: ComposerRole::Orchestrator,
        images: Vec::new(),
        thread_id: LEAD.into(),
        text: "coordinate".into(),
        skills: Vec::new(),
        model_preference: None,
        fork_seed: None,
    }));
    let Some(LoopEvent::ChatAccepted { run_id, .. }) = accepted.first() else {
        panic!("the lead was not accepted: {accepted:?}");
    };
    run_id.parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lead_answer_restores_the_finished_lane_through_the_gui_host_and_the_lane_reads_it() {
    let mut world = World::new();
    let owners = ShortPrivateDirectory::new();
    let owner = world.owner(owners.path());
    let mut sink = world.sink(Some(&owner), true);
    owner.start(LEAD).unwrap();
    let lead = start_lead(&mut sink);
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = open(&world, lead, "answer-lane");
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    world.finish(lead).await;

    // The lane asks and then ends: both it and its lead are finished when the answer comes.
    world
        .model
        .finishing
        .lock()
        .unwrap()
        .insert(lane.root_run_id.to_string());
    world.model.scripted.lock().unwrap().insert(
        lane.root_run_id.to_string(),
        ContentBlock::ToolUse {
            id: "ask".into(),
            name: "ask_user".into(),
            input: serde_json::json!({"title": TITLE, "blocking": false}),
        },
    );
    world
        .runtime
        .send_message(lane.root_run_id, "ask the managing lead".into())
        .unwrap();
    world.runtime.wait(lane.root_run_id).await.unwrap();
    world
        .model
        .finishing
        .lock()
        .unwrap()
        .remove(&lane.root_run_id.to_string());
    assert_eq!(
        world.runtime.inspect_agent(lane.root_run_id).unwrap().phase,
        AgentRunPhase::Done
    );
    world.phase(lead, AgentRunPhase::Waiting).await;

    let question = world.question(LEAD);
    world.model.scripted.lock().unwrap().insert(
        lead.to_string(),
        ContentBlock::ToolUse {
            id: "answer".into(),
            name: "lane_answer".into(),
            input: serde_json::json!({
                "question_id": question.id,
                "management_revision": question.management_revision.unwrap(),
                "routing_revision": question.routing_revision,
                "operation_id": "gui-host-answer",
                "action": {"kind": "answer", "text": "agent task-scope decision"}
            }),
        },
    );
    world.model.scripted.lock().unwrap().insert(
        lane.root_run_id.to_string(),
        ContentBlock::ToolUse {
            id: "read-answer".into(),
            name: "user_answers".into(),
            input: serde_json::json!({}),
        },
    );
    world
        .runtime
        .send_message(lead, "answer the pending lane question".into())
        .unwrap();
    world
        .all_phase(&[lead, lane.root_run_id], AgentRunPhase::Waiting)
        .await;
    // Both deliveries finished their runs; their drivers are the barrier before the ACKs.
    let lead_delivery = world.drive(LEAD).await;
    let lane_delivery = world.drive(&lane.thread_id).await;

    let stored = world.runtime.user_question(&question.id).unwrap().unwrap();
    assert!(matches!(
        stored.answer_origin,
        Some(event_bus::AnswerOrigin::LaneLead { .. })
    ));
    assert!(
        world.model.texts.lock().unwrap()[&lane.root_run_id.to_string()]
            .iter()
            .any(|request| request.contains("agent task-scope decision")),
        "the restored lane could read the stored answer"
    );
    // The recovery scan also includes ACKed but not yet presented snapshots.
    // It is not an ACK barrier: check the two question-specific outbox rows.
    let connection = rusqlite::Connection::open(&world.config.db_path).unwrap();
    let mut notifications = connection
        .prepare("SELECT payload, state FROM lane_notifications")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .map(|row| {
            let (payload, state) = row.unwrap();
            let notification: event_bus::lane::LaneNotification =
                serde_json::from_str(&payload).unwrap();
            (notification, state)
        })
        .filter_map(|(notification, state)| match &notification.kind {
            event_bus::lane::LaneNotificationKind::Question { question_id, .. }
            | event_bus::lane::LaneNotificationKind::Answer { question_id, .. }
                if question_id == &question.id =>
            {
                Some((notification.destination_thread_id, state))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    notifications.sort();
    let mut expected = vec![
        (LEAD.to_owned(), "acked".to_owned()),
        (lane.thread_id.clone(), "acked".to_owned()),
    ];
    expected.sort();
    assert_eq!(
        notifications, expected,
        "both notifications were acknowledged: lead={lead_delivery:?}, lane={lane_delivery:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_capacity_block_reaches_the_lane_card_the_moment_it_happens() {
    let mut world = World::new();
    let _sink = world.sink(None, false);
    let lead = world
        .runtime
        .delegate_chat(
            LEAD,
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = open(&world, lead, "capacity-lane");
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let mut state = world.workbench();
    world.show(&mut state);
    let thread = ThreadId::new(lane.thread_id.clone());
    assert!(
        state
            .lane_detail(&thread)
            .expect("the lane is shown")
            .delivery
            .capacity_blocked
            .is_none()
    );

    for _ in 0..32 {
        world
            .runtime
            .request_user_question(lane.root_run_id, TITLE.into(), Vec::new(), false)
            .unwrap();
    }
    let refused = world
        .runtime
        .request_user_question(lane.root_run_id, TITLE.into(), Vec::new(), false)
        .unwrap_err();
    assert!(refused.contains("question_capacity_blocked"), "{refused}");
    world
        .until_state_changed("question_capacity_blocked", &lane.thread_id)
        .await;
    world.show(&mut state);

    let line = state
        .lane_detail(&thread)
        .unwrap()
        .delivery
        .capacity_line()
        .expect("the diagnostic is shown without leaving the lane");
    assert!(line.contains("32 unanswered"), "{line}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_users_stop_and_resume_reach_the_lane_views_when_they_are_confirmed() {
    let mut world = World::new();
    let mut sink = world.sink(None, false);
    let lead = start_lead(&mut sink);
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = open(&world, lead, "stop-lane");
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let mut state = world.workbench();
    world.show(&mut state);
    let thread = ThreadId::new(lane.thread_id.clone());
    assert!(!state.lane_detail(&thread).unwrap().delivery.lead_stopped);

    sink.submit(WorkbenchCommand::StopChat {
        thread_id: LEAD.into(),
    });
    world
        .until_state_changed("lane_delivery_stopped", LEAD)
        .await;
    world.show(&mut state);
    assert!(
        state.lane_detail(&thread).unwrap().delivery.lead_stopped,
        "the held-notification notice appears at the Stop"
    );

    let refused = sink.submit(WorkbenchCommand::ResumeLaneDeliveries {
        thread_id: LEAD.into(),
    });
    assert!(
        matches!(refused.as_slice(), [LoopEvent::ChatRejected { reason, .. }] if reason.contains("continue it")),
        "a stopped lead is not resumed by the notification button: {refused:?}"
    );
    world.show(&mut state);
    assert!(
        state.lane_detail(&thread).unwrap().delivery.lead_stopped,
        "and the Stop notice stays"
    );

    sink.submit(WorkbenchCommand::ContinueChat(ChatContinuation {
        project_root: None,
        thread_id: LEAD.into(),
        composer_role: ComposerRole::Orchestrator,
        model_preference: None,
        automatic: false,
    }));
    world
        .until_state_changed("lane_delivery_resumed", LEAD)
        .await;
    world.show(&mut state);
    assert!(
        !state.lane_detail(&thread).unwrap().delivery.lead_stopped,
        "and goes away when the user resumes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resuming_after_a_restart_works_for_a_chat_without_a_goal_and_never_fails_silently() {
    use storage::lane_delivery::{LANE_NO_PROGRESS_LIMIT, LaneGuardSignal};

    let mut world = World::new();
    let lead = world
        .runtime
        .delegate_chat(
            LEAD,
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = open(&world, lead, "restart-lane");
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    world.finish(lead).await;
    let handle = world.storage.handle();
    for batch in 0..LANE_NO_PROGRESS_LIMIT {
        let guard = handle.lane_wake_guard(lane.lane_id.clone()).unwrap();
        handle
            .update_lane_wake_guard(
                lane.lane_id.clone(),
                guard.revision,
                LaneGuardSignal::WakeBatch {
                    batch_id: format!("batch-{batch}"),
                },
            )
            .unwrap();
    }
    assert!(
        handle
            .lane_wake_guard(lane.lane_id.clone())
            .unwrap()
            .suppressed
    );

    // A new process: no goal binding, the lead's root is only saved history.
    let mut sink =
        RuntimeCommandSink::new(world.restarted_runtime(), tokio::runtime::Handle::current());
    let events = sink.submit(WorkbenchCommand::ResumeLaneDeliveries {
        thread_id: LEAD.into(),
    });

    assert!(events.is_empty(), "{events:?}");
    assert!(
        !handle.lane_wake_guard(lane.lane_id).unwrap().suppressed,
        "the explicit resume reset the guard of the lane the lead manages"
    );
    let unknown = sink.submit(WorkbenchCommand::ResumeLaneDeliveries {
        thread_id: "ghost".into(),
    });
    assert!(
        matches!(unknown.as_slice(), [LoopEvent::ChatNotice { .. }]),
        "a resume that cannot be done says so: {unknown:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_destination_the_window_does_not_know_is_nacked_and_its_notification_stays() {
    use storage::lanes::NewLane;

    let world = World::new();
    let _sink = world.sink(None, true);
    let handle = world.storage.handle();
    let created = handle
        .create_lane(NewLane {
            lane_id: "ghost-lane".into(),
            thread_id: "ghost-lane-thread".into(),
            origin_thread_id: "ghost-lead".into(),
            origin_run_id: "ghost-run".into(),
            creation_key: "ghost-key".into(),
            brief: brief(),
            start_seed: "seed".into(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns: 1,
        })
        .unwrap();
    assert!(created.created);
    handle
        .append_lane_report_with_delivery(
            "ghost-lane".into(),
            1,
            event_bus::LaneReportInput {
                status: event_bus::LaneStatus::Ready,
                summary: "done".into(),
                artifact_refs: vec!["artifact:1".into()],
                artifact_revision: Some(event_bus::lane::ArtifactRevision::CommitSha(format!(
                    "{:040x}",
                    1
                ))),
                failure_episode: None,
                needs_human: false,
                question_refs: vec![],
                reason_code: None,
                producer: event_bus::LaneReportProducer::LaneAgent,
            },
            2,
        )
        .unwrap();

    assert_eq!(
        world.drive("ghost-lead").await,
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::UnknownThread)
    );
    assert_eq!(
        handle
            .pending_lane_delivery_destinations(String::new())
            .unwrap(),
        ["ghost-lead"],
        "nothing was guessed from a name and the notification is kept"
    );
}

#[tokio::test]
async fn attention_query_and_display_leave_a_static_provider_input_and_saved_prefix_unchanged() {
    let mut world = World::new();
    let lead = world
        .runtime
        .delegate_chat(
            LEAD,
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = open(&world, lead, "static-attention");
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    world
        .storage
        .handle()
        .append_lane_report_with_delivery(
            lane.lane_id.clone(),
            1,
            event_bus::LaneReportInput {
                status: event_bus::LaneStatus::Ready,
                summary: "unverified artifact".into(),
                artifact_refs: vec![],
                artifact_revision: None,
                failure_episode: None,
                needs_human: false,
                question_refs: vec![],
                reason_code: None,
                producer: event_bus::LaneReportProducer::LaneAgent,
            },
            100,
        )
        .unwrap();
    // No host is bound and both roots are waiting: this is a static fixture,
    // not an assertion about how quickly a scheduled delivery would happen.
    let texts = world.model.texts.lock().unwrap().clone();
    let calls = world.model.requests.lock().unwrap().clone();
    let db_path = world.config.db_path.clone();
    let saved = || {
        [lead, lane.root_run_id].map(|run| {
            let record = storage::Database::open_read_only(&db_path)
                .unwrap()
                .run_context(&run.to_string())
                .unwrap()
                .unwrap();
            (record.messages_json, record.config_json)
        })
    };
    let prefix = saved();
    let reader = Arc::new(gui::app::lane_attention::StorageLaneAttentionReader::new(
        world.config.db_path.clone(),
    ));
    let mut state = world.workbench().with_lane_attention(reader);
    world.show(&mut state);
    assert_eq!(state.lane_attention().items().len(), 1);
    let ctx = egui::Context::default();
    let mut drafts = Default::default();
    for _ in 0..3 {
        state.refresh_lane_attention();
        world.show(&mut state);
        let board = state.lane_attention();
        let rows = gui::model::lane_attention::rows(board, &[]);
        let mut output = ctx.run_ui(Default::default(), |ui| {
            gui::theme::install(ui.ctx());
            egui::CentralPanel::default().show(ui, |ui| {
                let output = gui::panes::lane_attention::lane_attention_section(
                    ui,
                    board,
                    &rows,
                    &mut drafts,
                    false,
                );
                assert!(output.action.is_none() && output.request.is_none());
            });
        });
        output.textures_delta.clear();
    }
    assert_eq!(*world.model.texts.lock().unwrap(), texts);
    assert_eq!(*world.model.requests.lock().unwrap(), calls);
    assert_eq!(
        saved(),
        prefix,
        "query and redraw do not replace saved model input/tool settings"
    );
    assert_eq!(
        state.lane_attention().items().len(),
        1,
        "reading did not resolve the item"
    );
    for run in [lead, lane.root_run_id] {
        world.runtime.cancel(run).unwrap();
        world.runtime.wait(run).await.unwrap();
    }
}
