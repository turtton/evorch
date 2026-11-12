//! A lane the ledger shows as closed (by its lead, a human or a discard) is not an
//! uncollected result: it leaves the Phase 4B cohort, so it neither suppresses nor
//! keeps suppressing `lane_open`. Through the real lane tools, runtime and storage,
//! with event-driven model turns.
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RuntimeError,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const CODE: &str = event_bus::event::diagnostic_codes::LANE_UNCOLLECTED_CREATION_SUPPRESSED;

#[derive(Default)]
struct Model {
    results: Mutex<Vec<(String, Value)>>,
}

fn brief() -> Value {
    json!({"objective":"independent artifact","inputs":[],"deliverables":["artifact"],"acceptance_criteria":["review"],"constraints":[],"workspace":{},"report_policy":"attached"})
}

#[async_trait::async_trait]
impl AgentModel for Model {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }

    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let mut content = vec![ContentBlock::Text {
            text: "idle".into(),
        }];
        if let Some(last) = messages.last() {
            for block in &last.content {
                match block {
                    ContentBlock::Text { text } if text.starts_with("open:") => {
                        let mut brief = brief();
                        brief["objective"] = json!(format!("independent artifact {text}"));
                        content = vec![ContentBlock::ToolUse {
                            id: text.clone(),
                            name: "lane_open".into(),
                            input: json!({"brief":brief}),
                        }];
                    }
                    // `close:<lane_close arguments as JSON>`
                    ContentBlock::Text { text } if text.starts_with("close:") => {
                        content = vec![ContentBlock::ToolUse {
                            id: text.clone(),
                            name: "lane_close".into(),
                            input: serde_json::from_str(&text["close:".len()..]).unwrap(),
                        }];
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

async fn waiting(events: &mut event_bus::EventReceiver, run: RunId) {
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id, to, reason, ..
        }) = events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            assert_ne!(to, AgentRunPhase::Error, "{reason:?}");
            if to == AgentRunPhase::Waiting {
                return;
            }
        }
    }
}

fn report() -> event_bus::LaneReportInput {
    event_bus::LaneReportInput {
        status: event_bus::LaneStatus::Ready,
        summary: "artifact".into(),
        artifact_revision: None,
        failure_episode: None,
        artifact_refs: vec!["artifact:seed".into()],
        needs_human: false,
        question_refs: vec![],
        reason_code: None,
        producer: event_bus::LaneReportProducer::LaneAgent,
    }
}

/// A managed lane of "progress-lead" whose latest report is a current Ready.
fn seed_ready(store: &storage::Storage, lead: RunId, lane_id: &str) {
    store
        .handle()
        .create_lane(storage::lanes::NewLane {
            lane_id: lane_id.into(),
            thread_id: format!("{lane_id}-thread"),
            origin_thread_id: "progress-lead".into(),
            origin_run_id: lead.to_string(),
            creation_key: format!("seed-{lane_id}"),
            brief: serde_json::from_value(brief()).unwrap(),
            start_seed: "seed".into(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns: 1,
        })
        .unwrap();
    store
        .handle()
        .append_lane_report(lane_id, 1, report(), 2)
        .unwrap();
}

struct Fixture {
    store: storage::Storage,
    model: Arc<Model>,
    runtime: AgentRuntime,
    events: event_bus::EventReceiver,
    diagnostics: event_bus::EventReceiver,
}

fn stored_runtime(config: &storage::StorageConfig, limit: Option<u32>) -> Fixture {
    let store = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let events = bus.subscribe();
    let diagnostics = bus.subscribe();
    let model = Arc::new(Model::default());
    let mut runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        model.clone(),
    );
    if let Some(limit) = limit {
        runtime = runtime.with_uncollected_lane_open_limit(limit);
    }
    let runtime = runtime.with_run_store(runtime::RunStore::open(config, store.handle()).unwrap());
    Fixture {
        store,
        model,
        runtime,
        events,
        diagnostics,
    }
}

fn lead_config(seeded_count: Option<u32>) -> RunConfig {
    RunConfig {
        interactive: true,
        keep_alive: true,
        conversation: true,
        spawn_credits: seeded_count.map_or_else(Default::default, |count| {
            serde_json::from_value(json!({
                "spawned": [],
                "observed": [],
                "unobserved": 20,
                "uncollected_lane_open_count": count,
                "uncollected_lane_ids": ["uncollected"]
            }))
            .unwrap()
        }),
        ..Default::default()
    }
}

async fn command(f: &mut Fixture, lead: RunId, text: &str) -> Value {
    f.runtime.send_message(lead, text.into()).unwrap();
    waiting(&mut f.events, lead).await;
    f.model
        .results
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(id, _)| id == text)
        .unwrap()
        .1
        .clone()
}

async fn open_created(f: &mut Fixture, lead: RunId, name: &str) -> RunId {
    let opened = command(f, lead, &format!("open:{name}")).await;
    assert_eq!(opened["created"], true, "{name}: {opened}");
    opened["root_run_id"].as_str().unwrap().parse().unwrap()
}

fn suppression_diagnostics(f: &mut Fixture) -> Vec<event_bus::DiagnosticEvent> {
    f.diagnostics
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Diagnostic(d) if d.code == CODE => Some(d),
            _ => None,
        })
        .collect()
}

async fn lead_closes(f: &mut Fixture, lead: RunId, lane_id: &str) {
    let now = f.store.handle().lane_close_context(lane_id.into()).unwrap();
    let arguments = json!({
        "lane_id": lane_id,
        "ready_seq": now.latest_ready_seq,
        "expected_management_revision": now.management_revision,
        "expected_request_revision": now.request_revision,
        "note": "collected and accepted"
    });
    let closed = command(f, lead, &format!("close:{arguments}")).await;
    assert_eq!(closed["outcome"], "closed", "{closed}");
    assert_eq!(closed["kind"], "lead_closed");
}

fn human_closes(f: &Fixture, lane_id: &str, discard: bool) {
    let now = f.store.handle().lane_close_context(lane_id.into()).unwrap();
    f.runtime
        .close_lane_from_host(
            runtime::lane_management::LaneHumanContext,
            runtime::lane_close::HumanLaneCloseRequest {
                lane_id: lane_id.into(),
                ready_seq: (!discard).then_some(now.latest_ready_seq).flatten(),
                expected_management_revision: now.management_revision,
                expected_request_revision: now.request_revision,
                discard,
                note: "ended by a person".into(),
                surface: "web".into(),
                client_id: Some("client-1".into()),
            },
            &[],
        )
        .unwrap();
}

async fn finish(f: &Fixture, roots: Vec<RunId>, lead: RunId) {
    for run in roots.iter().copied().chain([lead]) {
        f.runtime.cancel(run).unwrap();
    }
    for run in roots.into_iter().chain([lead]) {
        f.runtime.wait(run).await.unwrap();
    }
}

/// The threshold is injected (2) only to keep the scenario short; the marker of the
/// cohort must follow the ledger through a close, and a later cohort is reported anew.
#[tokio::test]
async fn a_lead_close_lifts_an_active_suppression_and_a_later_cohort_is_reported_anew() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("close.db"),
        ..Default::default()
    };
    let mut f = stored_runtime(&config, Some(2));
    let lead = f
        .runtime
        .delegate_chat(
            "progress-lead",
            Role::Orchestrator,
            "coordinate".into(),
            lead_config(None),
        )
        .unwrap();
    waiting(&mut f.events, lead).await;
    seed_ready(&f.store, lead, "first");
    let mut roots = Vec::new();
    for n in 0..2 {
        roots.push(open_created(&mut f, lead, &format!("a{n}")).await);
    }
    let blocked = command(&mut f, lead, "open:blocked").await;
    assert_eq!(blocked["code"], "lane_creation_suppressed", "{blocked}");
    let first = suppression_diagnostics(&mut f);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(first[0].detail.contains("lane_ids=first\n"));

    lead_closes(&mut f, lead, "first").await;
    // Not suppressed, and still not after more opens than the threshold.
    for n in 0..4 {
        roots.push(open_created(&mut f, lead, &format!("b{n}")).await);
    }
    assert!(suppression_diagnostics(&mut f).is_empty());

    // A new uncollected Ready makes a new cohort: its own count and its own report.
    seed_ready(&f.store, lead, "second");
    for n in 0..2 {
        roots.push(open_created(&mut f, lead, &format!("c{n}")).await);
    }
    let blocked = command(&mut f, lead, "open:blocked-again").await;
    assert_eq!(blocked["code"], "lane_creation_suppressed", "{blocked}");
    let second = suppression_diagnostics(&mut f);
    assert_eq!(second.len(), 1, "{second:?}");
    assert!(second[0].detail.contains("lane_ids=second\n"));
    let identity = |d: &event_bus::DiagnosticEvent| {
        d.detail
            .lines()
            .find(|line| line.starts_with("identity="))
            .unwrap()
            .to_owned()
    };
    assert_ne!(identity(&first[0]), identity(&second[0]));
    finish(&f, roots, lead).await;
}

/// The default threshold (100): one count short of it, a closed lane never lets the
/// next opens reach the suppression. Without the close, the second open here is refused.
#[tokio::test]
async fn closing_the_uncollected_ready_keeps_creation_from_reaching_the_suppression() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("live.db"),
        ..Default::default()
    };
    let mut f = stored_runtime(&config, None);
    let lead = f
        .runtime
        .delegate_chat(
            "progress-lead",
            Role::Orchestrator,
            "coordinate".into(),
            lead_config(Some(99)),
        )
        .unwrap();
    waiting(&mut f.events, lead).await;
    seed_ready(&f.store, lead, "uncollected");
    lead_closes(&mut f, lead, "uncollected").await;
    let mut roots = Vec::new();
    for n in 0..3 {
        roots.push(open_created(&mut f, lead, &format!("open{n}")).await);
    }
    assert!(suppression_diagnostics(&mut f).is_empty());
    finish(&f, roots, lead).await;
}

/// A human's close or discard ends the ledger entry too, and the ledger, not the lead's
/// saved credits, decides: the lead's snapshot still lists the lane, yet a restored run
/// in a new runtime is not suppressed.
async fn human_end_survives_a_restore(discard: bool) {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("restore.db"),
        ..Default::default()
    };
    let mut f = stored_runtime(&config, None);
    let lead = f
        .runtime
        .delegate_chat(
            "progress-lead",
            Role::Orchestrator,
            "coordinate".into(),
            lead_config(Some(99)),
        )
        .unwrap();
    waiting(&mut f.events, lead).await;
    seed_ready(&f.store, lead, "uncollected");
    human_closes(&f, "uncollected", discard);
    f.runtime.stop(lead, runtime::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
    drop(f);

    let mut f = stored_runtime(&config, None);
    f.runtime.resume_chat(lead, lead_config(None)).unwrap();
    waiting(&mut f.events, lead).await;
    let mut roots = Vec::new();
    for n in 0..3 {
        roots.push(open_created(&mut f, lead, &format!("restored{n}")).await);
    }
    assert!(suppression_diagnostics(&mut f).is_empty());
    finish(&f, roots, lead).await;
}

#[tokio::test]
async fn a_human_close_leaves_the_cohort_across_a_restore() {
    human_end_survives_a_restore(false).await;
}

#[tokio::test]
async fn a_discard_leaves_the_cohort_across_a_restore() {
    human_end_survives_a_restore(true).await;
}
