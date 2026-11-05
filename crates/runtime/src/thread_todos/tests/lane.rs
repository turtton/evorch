//! A lead and each of its lanes are independent roots with independent
//! procedures. A stopped or finished work period marks only its own root's
//! list; nothing about a procedure reaches lane reports, wakes or requests.
use super::boundary::{Seen, position, statuses, todo_seen, until};
use super::*;
use ThreadTodoStatus::{Completed, InProgress, Interrupted, Pending};
use event_bus::{LaneBrief, LanePolicy};

/// Every run answers with a finished turn; requests are recorded per run.
struct Quiet {
    requests: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl AgentModel for Quiet {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
    async fn complete(
        &self,
        ctx: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests.lock().unwrap().push(ctx.run_id.clone());
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "turn complete".into(),
                }],
            },
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        })
    }
}

struct Lanes {
    runtime: AgentRuntime,
    model: Arc<Quiet>,
    events: event_bus::EventReceiver,
    lead: RunId,
    a: RunId,
    b: RunId,
    _storage: storage::Storage,
    _dir: tempfile::TempDir,
}

fn brief() -> LaneBrief {
    LaneBrief {
        verification_requirements: Default::default(),
        objective: "an independent deliverable".into(),
        inputs: vec![],
        deliverables: vec!["artifact".into()],
        acceptance_criteria: vec!["independent review".into()],
        constraints: vec![],
        workspace: Default::default(),
        report_policy: LanePolicy::Attached,
    }
}

fn running(prefix: &str) -> Vec<ThreadTodoItem> {
    vec![
        item(&format!("{prefix} doing"), InProgress),
        item(&format!("{prefix} later"), Pending),
        item(&format!("{prefix} done"), Completed),
    ]
}

fn marked() -> Vec<ThreadTodoStatus> {
    vec![Interrupted, Pending, Completed]
}

async fn waiting(events: &mut event_bus::EventReceiver, run: RunId) {
    until(events, run, |seen| {
        *seen == Seen::Phase(AgentRunPhase::Waiting)
    })
    .await;
}

/// A lead with two lanes, each holding its own running procedure.
async fn lanes() -> Lanes {
    let dir = tempfile::tempdir().unwrap();
    let storage_config = storage::StorageConfig {
        db_path: dir.path().join("lanes.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let model = Arc::new(Quiet {
        requests: Mutex::new(vec![]),
    });
    let bus = Arc::new(event_bus::EventBus::new(4096));
    let mut events = bus.subscribe();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(::tools::ToolExecutor::new(bus)),
        model.clone(),
    )
    .with_run_store(crate::RunStore::open(&storage_config, storage.handle()).unwrap());
    let lead = runtime
        .delegate_chat(
            "lead",
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
    waiting(&mut events, lead).await;
    let a = runtime.open_lane(lead, "a", brief()).unwrap().root_run_id;
    waiting(&mut events, a).await;
    let b = runtime.open_lane(lead, "b", brief()).unwrap().root_run_id;
    waiting(&mut events, b).await;
    for (root, name) in [(lead, "lead"), (a, "lane a"), (b, "lane b")] {
        runtime
            .write_thread_todo(root, Operation::Replace(running(name)))
            .unwrap();
    }
    Lanes {
        runtime,
        model,
        events,
        lead,
        a,
        b,
        _storage: storage,
        _dir: dir,
    }
}

impl Lanes {
    fn list(&self, root: RunId) -> ThreadTodoSnapshot {
        let thread = self.runtime.goal_thread(root).unwrap();
        self.runtime.thread_todo(&thread).unwrap()
    }
    fn untouched(&self, root: RunId, name: &str) {
        let list = self.list(root);
        assert_eq!(list.revision, 1, "{name}");
        assert_eq!(list.items, running(name), "{name}");
    }
    fn requests(&self) -> usize {
        self.model.requests.lock().unwrap().len()
    }
}

#[tokio::test]
async fn the_lead_and_every_lane_keep_their_own_list() {
    let lanes = lanes().await;
    let ids: Vec<_> = [lanes.lead, lanes.a, lanes.b]
        .into_iter()
        .map(|root| lanes.list(root))
        .collect();
    assert_eq!(
        ids.iter().map(|list| list.revision).collect::<Vec<_>>(),
        [1, 1, 1]
    );
    for (index, list) in ids.iter().enumerate() {
        assert!(
            ids[index + 1..]
                .iter()
                .all(|other| other.list_id != list.list_id)
        );
        assert!(
            ids[index + 1..]
                .iter()
                .all(|other| other.thread_id != list.thread_id)
        );
    }
    // Editing one list leaves the others' revisions alone.
    lanes
        .runtime
        .write_thread_todo(
            lanes.a,
            Operation::Update {
                index: 1,
                status: InProgress,
                expected_revision: 1,
            },
        )
        .unwrap();
    assert_eq!(lanes.list(lanes.a).revision, 2);
    lanes.untouched(lanes.lead, "lead");
    lanes.untouched(lanes.b, "lane b");
}

#[tokio::test]
async fn stopping_the_lead_marks_only_the_lead_list() {
    let mut lanes = lanes().await;
    let before = lanes.requests();
    lanes
        .runtime
        .stop(lanes.lead, crate::StopScope::Subtree)
        .unwrap();
    let seen = until(&mut lanes.events, lanes.lead, |seen| {
        *seen == Seen::Phase(AgentRunPhase::Stopped)
    })
    .await;
    let lead_thread = lanes.runtime.goal_thread(lanes.lead).unwrap();
    position(&seen, &todo_seen(&lead_thread, 2, &marked()));
    assert_eq!(statuses(&lanes.list(lanes.lead).items), marked());
    lanes.untouched(lanes.a, "lane a");
    lanes.untouched(lanes.b, "lane b");
    for lane in [lanes.a, lanes.b] {
        assert_eq!(
            lanes.runtime.inspect_agent(lane).unwrap().phase,
            AgentRunPhase::Waiting
        );
    }
    assert_eq!(lanes.requests(), before, "no list change asks the model");
}

#[tokio::test]
async fn stopping_a_lane_marks_only_that_lane_list() {
    let mut lanes = lanes().await;
    let before = lanes.requests();
    lanes
        .runtime
        .stop(lanes.a, crate::StopScope::SelfOnly)
        .unwrap();
    let seen = until(&mut lanes.events, lanes.a, |seen| {
        *seen == Seen::Phase(AgentRunPhase::Stopped)
    })
    .await;
    let thread = lanes.runtime.goal_thread(lanes.a).unwrap();
    assert!(
        position(&seen, &todo_seen(&thread, 2, &marked()))
            < position(&seen, &Seen::Phase(AgentRunPhase::Stopped))
    );
    lanes.untouched(lanes.lead, "lead");
    lanes.untouched(lanes.b, "lane b");
    assert_eq!(
        lanes.runtime.inspect_agent(lanes.lead).unwrap().phase,
        AgentRunPhase::Waiting
    );
    assert_eq!(lanes.requests(), before, "the lead was not woken");
}

#[tokio::test]
async fn a_lane_that_finishes_a_turn_marks_its_list_and_wakes_nobody() {
    let mut lanes = lanes().await;
    let before_lead = lanes
        .model
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|run| **run == lanes.lead.to_string())
        .count();
    lanes
        .runtime
        .send_message(lanes.a, "Next lane input".into())
        .unwrap();
    waiting(&mut lanes.events, lanes.a).await;
    waiting_turn(&mut lanes).await;
    assert_eq!(statuses(&lanes.list(lanes.a).items), marked());
    lanes.untouched(lanes.lead, "lead");
    lanes.untouched(lanes.b, "lane b");
    let lead_requests = lanes
        .model
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|run| **run == lanes.lead.to_string())
        .count();
    assert_eq!(lead_requests, before_lead);
}

async fn waiting_turn(lanes: &mut Lanes) {
    // The turn event follows Waiting; the list was marked before both.
    until(&mut lanes.events, lanes.a, |seen| *seen == Seen::Turn).await;
}

#[tokio::test]
async fn a_lane_waiting_for_a_required_answer_keeps_its_running_items() {
    let mut lanes = lanes().await;
    lanes
        .runtime
        .request_user_question(lanes.a, "Which option?".into(), vec![], true)
        .unwrap();
    lanes
        .runtime
        .send_message(lanes.a, "Next lane input".into())
        .unwrap();
    waiting(&mut lanes.events, lanes.a).await;
    lanes.untouched(lanes.a, "lane a");
    lanes.untouched(lanes.lead, "lead");
    lanes.untouched(lanes.b, "lane b");
    // The lane stops while still waiting: now its period is over.
    lanes
        .runtime
        .stop(lanes.a, crate::StopScope::SelfOnly)
        .unwrap();
    lanes.runtime.wait(lanes.a).await.unwrap();
    assert_eq!(statuses(&lanes.list(lanes.a).items), marked());
    lanes.untouched(lanes.lead, "lead");
    lanes.untouched(lanes.b, "lane b");
}
