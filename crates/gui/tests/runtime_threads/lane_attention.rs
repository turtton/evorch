//! ADR 0044 Phase 2D: the lane attention list against the real runtime, storage and
//! `RuntimeCommandSink`. Storage derives the list; the GUI only reads and shows it.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use event_bus::lane::QuestionResponder;
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent, ToolEvent};
use gui::{
    app::WorkbenchState,
    fixture::DemoSource,
    model::commands::{ChatSubmission, CommandSink, LoopEvent, WorkbenchCommand},
    model::composer::ComposerRole,
    model::lane_attention::{AttentionRow, lead_handles, rows},
    runtime_sink::RuntimeCommandSink,
};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::ownership::OwnerHost;
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunId, RunStore, RuntimeError,
};
use workbench_protocol::lane_attention::{LaneAttentionKind, LaneQuestionResponder};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

use crate::short_private_directory;
use short_private_directory::ShortPrivateDirectory;

const LEAD: &str = "lead";
const TITLE: &str = "Which task scope?";

/// A root with a scripted tool call makes it once; every other turn only says it is idle.
#[derive(Default)]
struct Model {
    scripted: Mutex<HashMap<String, ContentBlock>>,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let tool = self.scripted.lock().unwrap().remove(&invocation.run_id);
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
        "lane-attention".into()
    }
}

struct World {
    runtime: AgentRuntime,
    storage: storage::Storage,
    model: Arc<Model>,
    events: EventReceiver,
    bus: Arc<EventBus>,
    /// Events received and not yet handed to a workbench.
    seen: Vec<event_bus::Event>,
    config: storage::StorageConfig,
    _dir: test_socket_dir::SplitDir,
}

impl World {
    fn new() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let config = storage::StorageConfig {
            db_path: dir.path().join("attention.db"),
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
            seen: Vec::new(),
            config,
            _dir: dir,
        }
    }

    fn sink(&self, owner: &Arc<OwnerHost>) -> RuntimeCommandSink {
        RuntimeCommandSink::new(self.runtime.clone(), tokio::runtime::Handle::current())
            .with_ownership(owner.clone())
            .with_lane_delivery()
    }

    async fn next_event(&mut self) -> event_bus::Event {
        let event = self.events.recv().await.unwrap();
        self.seen.push(event.clone());
        event
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

    /// Receives events until the question `id` is announced in a state `wanted` accepts.
    async fn until_question(
        &mut self,
        id: &str,
        wanted: impl Fn(&event_bus::UserQuestion) -> bool,
    ) {
        loop {
            let event = self.next_event().await;
            if matches!(&event.kind, EventKind::Tool(ToolEvent::UserQuestionUpdated { question })
                if question.id == id && wanted(question))
            {
                return;
            }
        }
    }

    /// A workbench over this world's ledger with the lead's thread, as the app builds it.
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
        self.workbench_over(sidebar)
    }

    fn workbench_over(&self, sidebar: SidebarState) -> WorkbenchState<DemoSource> {
        let inline: gui::app::lanes::Spawner = Arc::new(|job| job());
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar)
            .with_provider_status(gui::model::composer::ProviderStatus::Configured)
            .with_lane_ledger(Arc::new(gui::app::lanes::StorageLaneReader::new(
                self.config.db_path.clone(),
            )))
            .with_lane_attention(Arc::new(
                gui::app::lane_attention::StorageLaneAttentionReader::new(
                    self.config.db_path.clone(),
                ),
            ))
            .with_lane_spawner(inline)
    }

    /// Hands the events received so far to `state` and lets its reads finish.
    fn show(&mut self, state: &mut WorkbenchState<DemoSource>) {
        state.apply_events(std::mem::take(&mut self.seen));
        settle(state);
    }

    /// The lane's root asks `TITLE` with `ask_user` and goes back to waiting.
    async fn lane_asks(&mut self, lane: &runtime::lanes::LaneOpened) -> event_bus::UserQuestion {
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
        self.question()
    }

    fn question(&self) -> event_bus::UserQuestion {
        self.storage
            .handle()
            .lane_questions(LEAD.into(), None)
            .unwrap()
            .questions
            .into_iter()
            .next()
            .expect("a lane question")
    }

    /// The lead passes the lane's question to the user with its own `lane_answer` tool.
    async fn lead_routes_to_user(&mut self, lead: RunId, question: &event_bus::UserQuestion) {
        self.model.scripted.lock().unwrap().insert(
            lead.to_string(),
            ContentBlock::ToolUse {
                id: "route".into(),
                name: "lane_answer".into(),
                input: serde_json::json!({
                    "question_id": question.id,
                    "management_revision": question.management_revision.unwrap(),
                    "routing_revision": question.routing_revision,
                    "operation_id": "route-to-user",
                    "action": {"kind": "route_to_user"}
                }),
            },
        );
        self.runtime
            .send_message(lead, "pass the lane question to the user".into())
            .unwrap();
        self.until_question(&question.id, |q| {
            matches!(q.responder, Some(QuestionResponder::User))
        })
        .await;
    }
}

fn settle(state: &mut WorkbenchState<DemoSource>) {
    for _ in 0..4 {
        state.pump_lanes();
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

/// A lead with one open lane whose question went to the lead.
struct Scenario {
    world: World,
    sink: RuntimeCommandSink,
    lead: RunId,
    lane: runtime::lanes::LaneOpened,
    question: event_bus::UserQuestion,
    _owners: ShortPrivateDirectory,
}

async fn scenario() -> Scenario {
    let mut world = World::new();
    let owners = ShortPrivateDirectory::new();
    let owner =
        Arc::new(OwnerHost::open(owners.path(), Default::default(), world.bus.clone()).unwrap());
    let mut sink = world.sink(&owner);
    owner.start(LEAD).unwrap();
    let lead = start_lead(&mut sink);
    world.phase(lead, AgentRunPhase::Waiting).await;
    let lane = world
        .runtime
        .open_lane(lead, "attention-lane", brief())
        .unwrap();
    world.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let question = world.lane_asks(&lane).await;
    Scenario {
        world,
        sink,
        lead,
        lane,
        question,
        _owners: owners,
    }
}

fn listed_rows(state: &WorkbenchState<DemoSource>) -> Vec<AttentionRow<'_>> {
    rows(state.lane_attention(), &state.sidebar().threads)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_the_lead_owns_is_listed_without_an_answer_control_and_survives_a_restart() {
    let mut s = scenario().await;
    let mut state = s.world.workbench();
    s.world.show(&mut state);

    let listed = listed_rows(&state);
    let [row] = listed.as_slice() else {
        panic!("one item expected: {:?}", state.lane_attention().items());
    };
    assert!(matches!(
        &row.item.kind,
        LaneAttentionKind::QuestionPending { responder: LaneQuestionResponder::Lead { lead_thread }, .. }
            if lead_thread.as_str() == LEAD
    ));
    assert!(lead_handles(&row.item.kind));
    assert!(
        row.answerable.is_none(),
        "no answer UI for a lead-owned question"
    );
    assert_eq!(
        row.lane_thread.as_ref().map(ToString::to_string),
        Some(s.lane.thread_id.clone())
    );

    // A restart: the saved sidebar, no event replayed, an empty notification history.
    let mut restarted = s.world.workbench_over(state.sidebar().clone());
    assert_eq!(restarted.notifications().items().count(), 0);
    settle(&mut restarted);
    assert_eq!(
        restarted.lane_attention().items().len(),
        1,
        "the ledger, not an event or a notice, restores the list"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_history_does_not_decide_what_needs_attention() {
    let mut s = scenario().await;
    let mut state = s.world.workbench();
    s.world.show(&mut state);
    assert_eq!(state.lane_attention().items().len(), 1);

    // More notices than the history keeps push every earlier notice out.
    for n in 0..80 {
        state.notifications_mut().push_history_gap(&[n]);
    }
    settle(&mut state);

    assert_eq!(state.notifications().items().count(), 64);
    assert_eq!(
        state.lane_attention().items().len(),
        1,
        "nothing was resolved by notices"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routing_to_the_user_enables_the_answer_and_answering_removes_the_item() {
    let mut s = scenario().await;
    let mut state = s.world.workbench();
    s.world.show(&mut state);
    assert!(listed_rows(&state)[0].answerable.is_none());

    s.world.lead_routes_to_user(s.lead, &s.question).await;
    s.world.show(&mut state);

    let listed = listed_rows(&state);
    let [row] = listed.as_slice() else {
        panic!("one item expected");
    };
    assert!(!lead_handles(&row.item.kind));
    assert!(
        row.answerable.is_some(),
        "the user can answer after the switch"
    );

    // The user answers through the usual command; the next read no longer lists the item.
    let events = s.sink.submit(WorkbenchCommand::AnswerUserQuestion {
        thread_id: s.lane.thread_id.clone(),
        question_id: s.question.id.clone(),
        answer: "narrow scope".into(),
    });
    assert!(
        matches!(events.as_slice(), [LoopEvent::UserAnswerSaved { .. }]),
        "{events:?}"
    );
    s.world
        .until_question(&s.question.id, |q| q.answer.is_some())
        .await;
    s.world.show(&mut state);
    assert!(
        state.lane_attention().items().is_empty(),
        "an answered question leaves the list: {:?}",
        state.lane_attention().items()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_answer_to_a_lead_owned_question_is_refused_even_from_a_stale_screen() {
    let mut s = scenario().await;
    let mut state = s.world.workbench();
    s.world.show(&mut state);

    // A screen that still offered an answer sends it anyway.
    let events = s.sink.submit(WorkbenchCommand::AnswerUserQuestion {
        thread_id: s.lane.thread_id.clone(),
        question_id: s.question.id.clone(),
        answer: "from a stale screen".into(),
    });
    assert!(
        matches!(events.as_slice(), [LoopEvent::CommandRejected { .. }]),
        "{events:?}"
    );
    let stored = s
        .world
        .runtime
        .user_question(&s.question.id)
        .unwrap()
        .unwrap();
    assert!(stored.answer.is_none());

    // The storage rule itself, below the runtime's own early check.
    let refused = s
        .world
        .storage
        .handle()
        .answer_user_question(
            &s.question.id,
            "direct",
            &event_bus::AnswerOrigin::Host {
                channel: Default::default(),
            },
        )
        .unwrap_err();
    assert!(refused.to_string().contains("not_responder"), "{refused}");
    s.world.show(&mut state);
    assert_eq!(state.lane_attention().items().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_after_lost_events_restores_what_they_would_have_shown() {
    let mut s = scenario().await;
    let mut state = s.world.workbench();
    s.world.show(&mut state);
    assert!(lead_handles(&state.lane_attention().items()[0].kind));

    // The routing change happens, but its events never reach this window.
    s.world.lead_routes_to_user(s.lead, &s.question).await;
    s.world.seen.clear();
    settle(&mut state);
    assert!(
        lead_handles(&state.lane_attention().items()[0].kind),
        "without events the window cannot know"
    );

    state.refresh_lane_attention();
    settle(&mut state);
    assert!(!lead_handles(&state.lane_attention().items()[0].kind));

    // The window's event-built question projection never heard of the switch, yet the stored
    // question the read returned lets the user answer from the list.
    assert!(
        state
            .pending_user_questions()
            .all(|q| !matches!(q.responder, Some(QuestionResponder::User))),
        "the event projection is still behind"
    );
    let listed = listed_rows(&state);
    let answerable = listed[0].answerable.expect("the answer card is restored");
    assert_eq!(answerable.id, s.question.id);
}
