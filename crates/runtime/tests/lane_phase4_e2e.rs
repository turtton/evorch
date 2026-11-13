//! Phase 4E integration: a real `AgentRuntime` with real SQLite storage and a model the
//! test controls turn by turn. It covers
//!
//! - the uncollected-creation threshold, default 100 against 20 (ADR 0044 §13), on the
//!   same three coordination scenarios;
//! - the lead's #559/#560 tools (`lane_close`, `lane_send`, `lane_revise_brief`) while
//!   creation is suppressed;
//! - a fourteen-lane coordination through paging, instructions and closes.
//!
//! Lane reports are written through the ledger append the `lane_report` tool uses minus
//! its notification outbox, so that no wake runs and only the creation guard is measured;
//! delivery and wake are exercised by the `lane_delivery` suites. Every turn waits on the
//! run's own `Waiting` event; nothing here depends on elapsed time. The cache half of the
//! phase lives in `cache_preservation_e2e` (`lane_cache::phase4`).
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RuntimeError,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const SUPPRESSED: &str = event_bus::event::diagnostic_codes::LANE_UNCOLLECTED_CREATION_SUPPRESSED;
const LEAD_THREAD: &str = "lead-thread";

/// Answers a user message `calls:<json array of {id,name,input}>` with those tool calls
/// and anything else with an idle text. Records every tool result by call ID and counts
/// the requests each run made.
#[derive(Default)]
struct Model {
    results: Mutex<Vec<(String, Value)>>,
    requests: Mutex<HashMap<String, usize>>,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }

    async fn complete(
        &self,
        context: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        *self
            .requests
            .lock()
            .unwrap()
            .entry(context.run_id.clone())
            .or_default() += 1;
        let mut content = vec![ContentBlock::Text {
            text: "idle".into(),
        }];
        // A batch's results may arrive as several messages: read everything after the
        // model's last answer.
        let start = messages
            .iter()
            .rposition(|message| matches!(message.role, providers::Role::Assistant))
            .map_or(0, |index| index + 1);
        for block in messages[start..]
            .iter()
            .flat_map(|message| &message.content)
        {
            match block {
                ContentBlock::Text { text } if text.starts_with("calls:") => {
                    let calls: Vec<Value> = serde_json::from_str(&text["calls:".len()..]).unwrap();
                    content = calls
                        .iter()
                        .map(|call| ContentBlock::ToolUse {
                            id: call["id"].as_str().unwrap().into(),
                            name: call["name"].as_str().unwrap().into(),
                            input: call["input"].clone(),
                        })
                        .collect();
                }
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } => {
                    for part in content {
                        if let providers::ToolResultContent::Text { text } = part
                            && let Ok(value) = serde_json::from_str(text)
                        {
                            self.results
                                .lock()
                                .unwrap()
                                .push((tool_call_id.clone(), value));
                        }
                    }
                }
                _ => {}
            }
        }
        let tool = matches!(content.first(), Some(ContentBlock::ToolUse { .. }));
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content,
            },
            finish_reason: if tool {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
            usage: Default::default(),
            usage_reported: true,
        })
    }
}

fn call(id: &str, name: &str, input: Value) -> Value {
    json!({"id": id, "name": name, "input": input})
}

fn brief(objective: &str) -> Value {
    json!({"objective":objective,"inputs":[],"deliverables":["artifact"],"acceptance_criteria":["review"],"constraints":[],"workspace":{},"report_policy":"attached"})
}

fn report(status: event_bus::LaneStatus, summary: &str) -> event_bus::LaneReportInput {
    event_bus::LaneReportInput {
        status,
        summary: summary.into(),
        artifact_revision: None,
        failure_episode: None,
        artifact_refs: vec!["artifact:seed".into()],
        needs_human: status == event_bus::LaneStatus::Blocked,
        question_refs: vec![],
        reason_code: None,
        producer: event_bus::LaneReportProducer::LaneAgent,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: storage::Storage,
    model: Arc<Model>,
    runtime: AgentRuntime,
    events: event_bus::EventReceiver,
    diagnostics: event_bus::EventReceiver,
    lead: RunId,
    roots: Vec<RunId>,
    turns: usize,
    clock: i64,
    suppressions: usize,
    other_runaway_diagnostics: Vec<String>,
}

impl Fixture {
    /// `limit: None` keeps the shipped threshold.
    async fn new(limit: Option<u32>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("phase4.db"),
            ..Default::default()
        };
        let store = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(65_536));
        let events = bus.subscribe();
        let diagnostics = bus.subscribe();
        let model = Arc::new(Model::default());
        let mut runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            model.clone(),
        )
        .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
        if let Some(limit) = limit {
            runtime = runtime.with_uncollected_lane_open_limit(limit);
        }
        let lead = runtime
            .delegate_chat(
                LEAD_THREAD,
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
        let mut fixture = Self {
            _dir: dir,
            store,
            model,
            runtime,
            events,
            diagnostics,
            lead,
            roots: Vec::new(),
            turns: 0,
            clock: 0,
            suppressions: 0,
            other_runaway_diagnostics: Vec::new(),
        };
        fixture.waiting(lead).await;
        fixture
    }

    async fn waiting(&mut self, run: RunId) {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) = self.events.recv().await.unwrap().kind
                && run_id == run.to_string()
            {
                assert_ne!(to, AgentRunPhase::Error, "{reason:?}");
                assert_ne!(to, AgentRunPhase::Done, "{reason:?}");
                if to == AgentRunPhase::Waiting {
                    return;
                }
            }
        }
    }

    /// One lead turn: the calls, then the idle answer after their results.
    async fn turn(&mut self, calls: Vec<Value>) -> Vec<Value> {
        let ids: Vec<String> = calls
            .iter()
            .map(|c| c["id"].as_str().unwrap().into())
            .collect();
        self.runtime
            .send_message(self.lead, format!("calls:{}", Value::Array(calls)))
            .unwrap();
        self.waiting(self.lead).await;
        self.turns += 1;
        for event in self.diagnostics.drain_pending_snapshot() {
            if let EventKind::Diagnostic(d) = event.kind {
                match d.code.as_str() {
                    SUPPRESSED => self.suppressions += 1,
                    "NoProgress" | "IdenticalToolCalls" | "BudgetExhausted" => {
                        self.other_runaway_diagnostics.push(d.code)
                    }
                    _ => {}
                }
            }
        }
        let results = self.model.results.lock().unwrap();
        ids.iter()
            .map(|id| {
                results
                    .iter()
                    .rev()
                    .find(|(found, _)| found == id)
                    .unwrap_or_else(|| panic!("no result for {id}"))
                    .1
                    .clone()
            })
            .collect()
    }

    async fn one(&mut self, id: &str, name: &str, input: Value) -> Value {
        self.turn(vec![call(id, name, input)]).await.remove(0)
    }

    async fn open(&mut self, id: &str) -> Value {
        let opened = self.one(id, "lane_open", json!({"brief": brief(id)})).await;
        if let Some(root) = opened["root_run_id"].as_str() {
            let root: RunId = root.parse().unwrap();
            if !self.roots.contains(&root) {
                self.roots.push(root);
            }
        }
        opened
    }

    fn at(&mut self) -> i64 {
        // Later than any real start time, so a re-reported lane lists first.
        self.clock += 1;
        i64::MAX / 2 + self.clock
    }

    fn report(&mut self, lane_id: &str, status: event_bus::LaneStatus, summary: &str) {
        let at = self.at();
        let lane = self.store.handle().lane(lane_id).unwrap().unwrap();
        self.store
            .handle()
            .append_lane_report(
                lane_id,
                lane.management_revision,
                report(status, summary),
                at,
            )
            .unwrap();
    }

    fn ready(&mut self, lane_id: &str) {
        self.report(lane_id, event_bus::LaneStatus::Ready, "artifact ready");
    }

    /// A finished lane the lead has not yet read, without a running root.
    fn seed_ready(&mut self, lane_id: &str) {
        let at = self.at();
        self.store
            .handle()
            .create_lane(storage::lanes::NewLane {
                lane_id: lane_id.into(),
                thread_id: format!("thread-{lane_id}"),
                origin_thread_id: LEAD_THREAD.into(),
                origin_run_id: self.lead.to_string(),
                creation_key: lane_id.into(),
                brief: serde_json::from_value(brief(lane_id)).unwrap(),
                start_seed: "seed".into(),
                project_id: None,
                project_root: None,
                title: None,
                at_ns: at,
            })
            .unwrap();
        self.ready(lane_id);
    }

    fn close_call(&self, id: &str, lane_id: &str) -> Value {
        let context = self
            .store
            .handle()
            .lane_close_context(lane_id.into())
            .unwrap();
        call(
            id,
            "lane_close",
            json!({
                "lane_id": lane_id,
                "ready_seq": context.latest_ready_seq.unwrap(),
                "expected_management_revision": context.management_revision,
                "expected_request_revision": context.request_revision,
                "note": "judged done by the lead",
            }),
        )
    }

    fn send_call(&self, id: &str, lane_id: &str, message: &str) -> Value {
        let lane = self.store.handle().lane(lane_id).unwrap().unwrap();
        call(
            id,
            "lane_send",
            json!({
                "operation_id": id,
                "lane_id": lane_id,
                "expected_management_revision": lane.management_revision,
                "expected_brief_revision": lane.brief_revision,
                "message": message,
            }),
        )
    }

    fn revise_call(&self, id: &str, lane_id: &str, objective: &str) -> Value {
        let lane = self.store.handle().lane(lane_id).unwrap().unwrap();
        call(
            id,
            "lane_revise_brief",
            json!({
                "operation_id": id,
                "lane_id": lane_id,
                "expected_management_revision": lane.management_revision,
                "expected_brief_revision": lane.brief_revision,
                "brief": brief(objective),
            }),
        )
    }

    fn lead_requests(&self) -> usize {
        self.model.requests.lock().unwrap()[&self.lead.to_string()]
    }

    async fn shutdown(self) {
        for run in self.roots.iter().copied().chain([self.lead]) {
            self.runtime.cancel(run).unwrap();
        }
        for run in self.roots.into_iter().chain([self.lead]) {
            self.runtime.wait(run).await.unwrap();
        }
    }
}

fn lane_id(opened: &Value) -> String {
    opened["lane_id"].as_str().unwrap().to_owned()
}

// ---------------------------------------------------------------------------------
// 100 against 20
// ---------------------------------------------------------------------------------

/// What one scenario cost the lead under one threshold.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    scenario: &'static str,
    /// New lanes the lead got to create before the first refusal.
    created_before_suppression: Option<u32>,
    created: u32,
    refusals: u32,
    diagnostics: usize,
    /// Creation worked again after the lead read what was waiting.
    released: bool,
    lead_turns: usize,
    lead_requests: usize,
}

#[track_caller]
fn assert_refusal(refused: &Value, threshold: u32) {
    assert_eq!(refused["code"], "lane_creation_suppressed", "{refused}");
    assert_eq!(refused["threshold"], threshold, "{refused}");
}

/// A lead that keeps creating lanes while a finished one stays unread: runaway.
async fn runaway(limit: Option<u32>, threshold: u32) -> Outcome {
    let mut fx = Fixture::new(limit).await;
    fx.seed_ready("unread");
    let (mut created, mut refusals) = (0, 0);
    for n in 0..threshold + 5 {
        let opened = fx.open(&format!("run-{n}")).await;
        if opened["created"] == true {
            assert_eq!(refusals, 0, "creation resumed without a read: {opened}");
            created += 1;
        } else {
            assert_refusal(&opened, threshold);
            refusals += 1;
        }
    }
    let diagnostics = fx.suppressions;
    // Reading the finished lane is what lifts it.
    fx.one("read", "lane_list", json!({})).await;
    let released = fx.open("after-read").await["created"] == true;
    let outcome = Outcome {
        scenario: "runaway: keeps opening lanes, never reads a finished one",
        created_before_suppression: (refusals > 0).then_some(created),
        created,
        refusals,
        diagnostics,
        released,
        lead_turns: fx.turns,
        lead_requests: fx.lead_requests(),
    };
    assert_eq!(fx.suppressions, diagnostics, "reading adds no diagnostic");
    assert!(fx.other_runaway_diagnostics.is_empty());
    fx.shutdown().await;
    outcome
}

const FAN_OUT: u32 = 30;

/// A wide fan-out: one early finisher is not read until the lead hits a refusal.
async fn wide_fan_out(limit: Option<u32>) -> Outcome {
    let mut fx = Fixture::new(limit).await;
    fx.seed_ready("early");
    let (mut created, mut refusals) = (0, 0);
    let mut first_refusal = None;
    for n in 0..FAN_OUT {
        let id = format!("fan-{n}");
        let mut opened = fx.open(&id).await;
        if opened["code"] == "lane_creation_suppressed" {
            first_refusal.get_or_insert(created);
            refusals += 1;
            fx.one(&format!("read-{n}"), "lane_list", json!({})).await;
            // The same envelope: the refusal left no intent behind.
            opened = fx.open(&id).await;
        }
        assert_eq!(opened["created"], true, "{opened}");
        created += 1;
    }
    let outcome = Outcome {
        scenario: "wide fan-out: 30 lanes, one early finisher unread",
        created_before_suppression: first_refusal,
        created,
        refusals,
        diagnostics: fx.suppressions,
        released: true,
        lead_turns: fx.turns,
        lead_requests: fx.lead_requests(),
    };
    assert!(fx.other_runaway_diagnostics.is_empty());
    fx.shutdown().await;
    outcome
}

const ROUNDS: u32 = 45;

/// Normal coordination: every lane's result is read as soon as it is ready.
async fn prompt_collection(limit: Option<u32>) -> Outcome {
    let mut fx = Fixture::new(limit).await;
    let mut created = 0;
    for n in 0..ROUNDS {
        let opened = fx.open(&format!("round-{n}")).await;
        assert_eq!(opened["created"], true, "round {n}: {opened}");
        created += 1;
        fx.ready(&lane_id(&opened));
        fx.one(&format!("collect-{n}"), "lane_list", json!({}))
            .await;
    }
    let outcome = Outcome {
        scenario: "normal: 45 lanes, each read as soon as it is ready",
        created_before_suppression: None,
        created,
        refusals: 0,
        diagnostics: fx.suppressions,
        released: true,
        lead_turns: fx.turns,
        lead_requests: fx.lead_requests(),
    };
    assert!(fx.other_runaway_diagnostics.is_empty());
    fx.shutdown().await;
    outcome
}

/// Prints one row of the comparison (`--nocapture` shows it) and checks the request
/// accounting: one request for the task, then two per turn, the call and the idle answer.
fn record(threshold: u32, o: &Outcome, exact_requests: bool) {
    println!(
        "threshold={threshold} | {} | created_before_suppression={:?} created={} refusals={} diagnostics={} released={} lead_turns={} lead_requests={}",
        o.scenario,
        o.created_before_suppression,
        o.created,
        o.refusals,
        o.diagnostics,
        o.released,
        o.lead_turns,
        o.lead_requests,
    );
    let expected = 1 + 2 * o.lead_turns;
    if exact_requests {
        assert_eq!(o.lead_requests, expected, "{o:?}");
    } else {
        assert!(o.lead_requests >= expected, "{o:?}");
    }
}

fn assert_runaway(o: &Outcome, threshold: u32) {
    record(threshold, o, true);
    assert_eq!(o.created_before_suppression, Some(threshold));
    assert_eq!((o.created, o.refusals), (threshold, 5));
    assert_eq!(o.diagnostics, 1);
    assert!(o.released);
}

fn assert_prompt_collection(o: &Outcome, threshold: u32) {
    // Requests beyond two per turn are not asserted here: lane roots can wake the lead.
    record(threshold, o, false);
    assert_eq!(o.created, ROUNDS);
    assert_eq!((o.refusals, o.diagnostics), (0, 0));
}

#[tokio::test]
async fn default_threshold_100_suppresses_a_runaway_only_after_a_hundred_creations() {
    // No injected limit: the shipped default.
    assert_runaway(&runaway(None, 100).await, 100);
}

#[tokio::test]
async fn threshold_20_suppresses_a_runaway_after_twenty_creations() {
    assert_runaway(&runaway(Some(20), 20).await, 20);
}

#[tokio::test]
async fn default_threshold_100_does_not_touch_a_thirty_lane_fan_out() {
    let o = wide_fan_out(None).await;
    record(100, &o, true);
    assert_eq!(o.created_before_suppression, None);
    assert_eq!((o.created, o.refusals, o.diagnostics), (30, 0, 0));
    assert_eq!(o.lead_turns, 30);
}

#[tokio::test]
async fn threshold_20_wrongly_holds_a_thirty_lane_fan_out_once() {
    // The false suppression: one refusal at the 21st lane, then a read and a retry. The
    // cost is those two extra turns, and the cohort is reported once.
    let o = wide_fan_out(Some(20)).await;
    record(20, &o, true);
    assert_eq!(o.created_before_suppression, Some(20));
    assert_eq!((o.created, o.refusals, o.diagnostics), (30, 1, 1));
    assert_eq!(o.lead_turns, 32);
}

#[tokio::test]
async fn default_threshold_100_never_holds_prompt_collection() {
    assert_prompt_collection(&prompt_collection(None).await, 100);
}

#[tokio::test]
async fn threshold_20_never_holds_prompt_collection_either() {
    // A cohort never forms when every result is read as it is ready, whatever the limit.
    assert_prompt_collection(&prompt_collection(Some(20)).await, 20);
}

// ---------------------------------------------------------------------------------
// #559 / #560 while creation is suppressed
// ---------------------------------------------------------------------------------

#[tokio::test]
async fn supplements_revisions_and_closes_compose_with_a_suppressed_creation() {
    let mut fx = Fixture::new(Some(3)).await;
    let l0 = lane_id(&fx.open("lane-0").await);
    let l1 = lane_id(&fx.open("lane-1").await);
    fx.ready(&l0);
    fx.ready(&l1);
    let mut later = Vec::new();
    for n in 2..5 {
        let opened = fx.open(&format!("open-{n}")).await;
        assert_eq!(opened["created"], true, "{opened}");
        later.push(lane_id(&opened));
    }
    let refused = fx.open("refused-1").await;
    assert_refusal(&refused, 3);
    let mut cohort = vec![l0.clone(), l1.clone()];
    cohort.sort();
    assert_eq!(refused["lane_ids"], json!(cohort));
    assert_eq!(fx.suppressions, 1);

    // A supplement is an instruction, not a new brief: the Ready stays current and nothing
    // is read, so creation stays held. An identical retry replays the first answer.
    let send = fx.send_call("send-0", &l0, "prefer the small fix");
    let sent = fx.turn(vec![send.clone(), send]).await;
    assert_eq!(sent[0]["status"], "accepted", "{}", sent[0]);
    assert_eq!(sent[0]["operation"]["instruction_seq"], 1);
    assert_eq!(sent[0]["operation"]["brief_revision"], 1);
    assert_eq!(sent[1]["operation"]["instruction_seq"], 1);
    assert_eq!(sent[1]["operation"]["replayed"], true);
    assert_refusal(&fx.open("refused-2").await, 3);

    // A revision that changes the brief makes that lane's Ready stale. One stale Ready of
    // two does not lift the hold: the other is still unread and current.
    let revise = fx.revise_call("revise-0", &l0, "reworked objective for lane 0");
    let revised = fx.turn(vec![revise]).await.remove(0);
    assert_eq!(revised["operation"]["instruction_seq"], 2, "{revised}");
    assert_eq!(revised["operation"]["brief_revision"], 2);
    assert_refusal(&fx.open("refused-3").await, 3);

    // Both lanes are back at work, so nothing finished is waiting unread: creation works,
    // and the cohort with its count survives that. A Ready the lane gave for the old brief
    // cannot be closed (#560), and no close is recorded.
    let revise = fx.revise_call("revise-1", &l1, "reworked objective for lane 1");
    let revised = fx.turn(vec![revise]).await.remove(0);
    assert_eq!(revised["operation"]["instruction_seq"], 1);
    assert_eq!(revised["operation"]["brief_revision"], 2);
    let reopened = fx.open("open-while-reworking").await;
    assert_eq!(reopened["created"], true, "{reopened}");
    let stale = fx.close_call("close-stale", &l1);
    let stale = fx.turn(vec![stale]).await.remove(0);
    assert_eq!(stale["error"]["code"], "stale_brief", "{stale}");
    assert!(
        fx.store
            .handle()
            .current_lane_close(l1.clone())
            .unwrap()
            .is_none()
    );

    // An unrelated lane that finished while its lead was holding: a supplement leaves its
    // Ready valid, and the lead closes it on its own judgement (#559), without a review. The
    // close names the lead as the actor and keeps the Ready it was about. It joins no cohort,
    // so creation keeps working.
    let done = later[0].clone();
    fx.ready(&done);
    let send = fx.send_call("send-done", &done, "last check before closing");
    let sent = fx.turn(vec![send]).await.remove(0);
    assert_eq!(sent["operation"]["brief_revision"], 1, "{sent}");
    let close = fx.close_call("close-done", &done);
    let closed = fx.turn(vec![close]).await.remove(0);
    assert_eq!(closed["outcome"], "closed", "{closed}");
    assert_eq!(closed["kind"], "lead_closed");
    let current = fx
        .store
        .handle()
        .current_lane_close(done.clone())
        .unwrap()
        .unwrap();
    assert_eq!(current.kind, event_bus::lane::LaneCloseKind::LeadClosed);
    assert_eq!(current.report_brief_revision, Some(1));
    assert_eq!(
        current.closed_by,
        Some(event_bus::lane::LaneCloseActor::Lead {
            thread_id: LEAD_THREAD.into(),
            root_run_id: fx.lead.to_string(),
        })
    );
    assert!(current.review.is_none());
    let more = fx.open("open-after-close").await;
    assert_eq!(more["created"], true, "{more}");
    assert_eq!(fx.suppressions, 1, "one cohort, one diagnostic");

    // The instructions are stored whole and in order, once.
    let stored = fx.store.handle().lane_instructions(&l0, 0, 10).unwrap();
    assert_eq!(
        stored
            .iter()
            .map(|i| (
                i.instruction_seq,
                i.kind,
                i.brief_revision_before,
                i.brief_revision_after
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                1,
                storage::lane_instructions::LaneInstructionKind::Supplement,
                1,
                1
            ),
            (
                2,
                storage::lane_instructions::LaneInstructionKind::BriefRevision,
                1,
                2
            ),
        ]
    );
    assert!(fx.other_runaway_diagnostics.is_empty());
    fx.shutdown().await;
}

// ---------------------------------------------------------------------------------
// Fourteen lanes
// ---------------------------------------------------------------------------------

#[tokio::test]
async fn fourteen_lanes_are_coordinated_through_paging_instructions_and_closes() {
    use event_bus::LaneStatus::{Blocked, Ready, Waiting, Working};
    let mut fx = Fixture::new(None).await;
    // One batch opens every lane; the shipped threshold has nothing to say about it.
    let opens: Vec<Value> = (0..14)
        .map(|n| {
            call(
                &format!("open-{n}"),
                "lane_open",
                json!({"brief": brief(&format!("lane {n}"))}),
            )
        })
        .collect();
    let opened = fx.turn(opens).await;
    let lanes: Vec<String> = opened.iter().map(lane_id).collect();
    for (n, opened) in opened.iter().enumerate() {
        assert_eq!(opened["created"], true, "lane {n}: {opened}");
        fx.roots
            .push(opened["root_run_id"].as_str().unwrap().parse().unwrap());
    }
    let distinct: std::collections::BTreeSet<_> = lanes.iter().collect();
    assert_eq!(distinct.len(), 14);
    // Seven finished, four working, two waiting, one blocked on a person.
    let states = [
        Ready, Ready, Ready, Ready, Ready, Ready, Ready, Working, Working, Working, Working,
        Waiting, Waiting, Blocked,
    ];
    for (lane, status) in lanes.iter().zip(states) {
        fx.report(lane, status, "state of the lane");
    }

    // A full listing, five at a time, is one consistent snapshot of all fourteen.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut snapshot = None;
    for page in 0.. {
        let input = match &cursor {
            Some(cursor) => json!({"limit":5,"cursor":cursor}),
            None => json!({"limit":5}),
        };
        let listed = fx.one(&format!("page-{page}"), "lane_list", input).await;
        let revision = listed["snapshot_revision"].clone();
        assert_eq!(*snapshot.get_or_insert(revision.clone()), revision);
        for item in listed["items"].as_array().unwrap() {
            seen.push((
                item["lane_id"].as_str().unwrap().to_owned(),
                item["latest_report"]["status"].as_str().unwrap().to_owned(),
            ));
        }
        match listed["next_cursor"].as_str() {
            Some(next) if listed["has_more"] == true => cursor = Some(next.to_owned()),
            _ => break,
        }
    }
    assert_eq!(seen.len(), 14);
    for (lane, status) in lanes.iter().zip(states) {
        let (_, listed) = seen.iter().find(|(id, _)| id == lane).unwrap();
        assert_eq!(listed, &format!("{status:?}").to_lowercase());
    }

    // #560: lane 0 gets a supplement (its Ready stays valid), lane 1 a new brief (its
    // Ready stops supporting a close).
    let supplement = fx.send_call("supplement-0", &lanes[0], "mind the edge case");
    let revision = fx.revise_call("revision-1", &lanes[1], "lane 1, reworked");
    let instructed = fx.turn(vec![supplement, revision]).await;
    assert_eq!(instructed[0]["status"], "accepted", "{}", instructed[0]);
    assert_eq!(instructed[1]["operation"]["brief_revision"], 2);

    // #559: the lead closes the finished lanes on its own judgement, no review or evidence.
    let mut closes: Vec<Value> = [0, 2, 3, 4]
        .iter()
        .map(|&n| fx.close_call(&format!("close-{n}"), &lanes[n]))
        .collect();
    closes.push(fx.close_call("close-1-stale", &lanes[1]));
    let closed = fx.turn(closes).await;
    for result in &closed[..4] {
        assert_eq!(result["outcome"], "closed", "{result}");
    }
    assert_eq!(closed[4]["error"]["code"], "stale_brief", "{}", closed[4]);

    for (n, lane) in lanes.iter().enumerate() {
        let current = fx.store.handle().current_lane_close(lane.clone()).unwrap();
        assert_eq!(current.is_some(), [0, 2, 3, 4].contains(&n), "lane {n}");
    }
    // Closing ends a ledger entry, not a run: every lane root is still alive, and the two
    // finished lanes that were not closed wait for the lead's judgement without being
    // treated as stalled or failed.
    for lane in &lanes {
        let lane = fx.store.handle().lane(lane).unwrap().unwrap();
        assert!(lane.is_started());
    }
    for lane in &lanes[5..7] {
        let context = fx.store.handle().lane_close_context(lane.clone()).unwrap();
        assert!(context.latest_ready_seq.is_some());
        assert!(
            fx.store
                .handle()
                .current_lane_close(lane.clone())
                .unwrap()
                .is_none()
        );
    }
    assert!(fx.other_runaway_diagnostics.is_empty());
    assert_eq!(fx.suppressions, 0);
    fx.shutdown().await;
}
