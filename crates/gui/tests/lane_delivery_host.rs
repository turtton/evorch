//! ADR 0044 Phase 1c: the GUI's lane delivery host against the real runtime, storage and
//! `RuntimeCommandSink`. The runtime owns delivery; the host only says who a thread is.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use event_bus::lane::{LaneDeliveryBinding, QuestionResponder};
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent};
use gui::{
    model::commands::{ChatSubmission, CommandSink, LoopEvent, WorkbenchCommand},
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
    _dir: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
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
            let event = self.events.recv().await.unwrap();
            if let EventKind::Tool(event_bus::ToolEvent::UserQuestionUpdated { question }) =
                &event.kind
            {
                self.announced.push(question.clone());
            }
            if matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
                if *run_id == run.to_string() && *to == wanted)
            {
                return;
            }
        }
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
    world.phase(lead, AgentRunPhase::Waiting).await;

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
    world.phase(lead, AgentRunPhase::Waiting).await;

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
