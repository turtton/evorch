//! Diagnostics of a suppressed lane creation, through the real lane_open tool, with
//! a test-injected threshold and event-driven model turns.
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RuntimeError,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Model {
    results: Mutex<Vec<(String, Value)>>,
    reject_lane_admission: std::sync::atomic::AtomicBool,
}

fn brief() -> Value {
    json!({"objective":"independent artifact","inputs":[],"deliverables":["artifact"],"acceptance_criteria":["review"],"constraints":[],"workspace":{},"report_policy":"attached"})
}

#[async_trait::async_trait]
impl AgentModel for Model {
    fn requires_admission(&self) -> bool {
        self.reject_lane_admission
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        Err(RuntimeError::Model {
            reason: "lane route unavailable".into(),
        })
    }

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
                    ContentBlock::Text { text } if text == "collect" => {
                        content = vec![ContentBlock::ToolUse {
                            id: "collect".into(),
                            name: "lane_list".into(),
                            input: json!({}),
                        }];
                    }
                    ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        ..
                    } => {
                        for part in content {
                            if let providers::ToolResultContent::Text { text } = part {
                                self.results.lock().unwrap().push((
                                    tool_call_id.clone(),
                                    serde_json::from_str(text).unwrap(),
                                ));
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

fn report(status: event_bus::LaneStatus) -> event_bus::LaneReportInput {
    event_bus::LaneReportInput {
        status,
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

fn seed_uncollected(
    store: &storage::Storage,
    lead: RunId,
    terminal: bool,
) -> storage::lanes::LaneRecord {
    let lane = store
        .handle()
        .create_lane(storage::lanes::NewLane {
            lane_id: "uncollected".into(),
            thread_id: "uncollected-thread".into(),
            origin_thread_id: "progress-lead".into(),
            origin_run_id: lead.to_string(),
            creation_key: "seed".into(),
            brief: serde_json::from_value(brief()).unwrap(),
            start_seed: "seed".into(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns: 1,
        })
        .unwrap()
        .lane;
    store
        .handle()
        .append_lane_report(
            &lane.lane_id,
            1,
            report(if terminal {
                event_bus::LaneStatus::Ready
            } else {
                event_bus::LaneStatus::Working
            }),
            2,
        )
        .unwrap();
    lane
}

fn stored_runtime(
    config: &storage::StorageConfig,
) -> (
    storage::Storage,
    Arc<Model>,
    AgentRuntime,
    event_bus::EventReceiver,
    event_bus::EventReceiver,
) {
    let store = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let events = bus.subscribe();
    let diagnostics = bus.subscribe();
    let model = Arc::new(Model::default());
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        model.clone(),
    )
    .with_uncollected_lane_open_limit(2)
    .with_run_store(runtime::RunStore::open(config, store.handle()).unwrap());
    (store, model, runtime, events, diagnostics)
}

async fn command(
    runtime: &AgentRuntime,
    model: &Model,
    events: &mut event_bus::EventReceiver,
    lead: RunId,
    text: &str,
) -> Value {
    runtime.send_message(lead, text.into()).unwrap();
    waiting(events, lead).await;
    model
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

#[tokio::test]
async fn a_cohort_is_reported_once_however_often_creation_is_refused() {
    const CODE: &str = event_bus::event::diagnostic_codes::LANE_UNCOLLECTED_CREATION_SUPPRESSED;
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("diagnostics.db"),
        ..Default::default()
    };
    let (store, model, runtime, mut events, mut diagnostics) = stored_runtime(&config);
    let lead = runtime
        .delegate_chat(
            "progress-lead",
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
    let lane = seed_uncollected(&store, lead, true);
    let mut roots = Vec::new();
    for n in 0..2 {
        let created = command(&runtime, &model, &mut events, lead, &format!("open:{n}")).await;
        assert_eq!(created["created"], true, "{created}");
        roots.push(
            created["root_run_id"]
                .as_str()
                .unwrap()
                .parse::<RunId>()
                .unwrap(),
        );
    }
    for n in 0..3 {
        let rejected = command(
            &runtime,
            &model,
            &mut events,
            lead,
            &format!("open:blocked-{n}"),
        )
        .await;
        assert_eq!(rejected["code"], "lane_creation_suppressed", "{rejected}");
        assert_eq!(rejected["threshold"], 2);
    }
    let reported: Vec<_> = diagnostics
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Diagnostic(d) if d.code == CODE => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(reported.len(), 1, "{reported:?}");
    let diagnostic = &reported[0];
    assert_eq!(diagnostic.source, "budget_tracker");
    assert_eq!(diagnostic.run_id, Some(lead.to_string()));
    assert_eq!(diagnostic.thread_id.as_deref(), Some("progress-lead"));
    assert!(
        diagnostic
            .detail
            .contains("\nuncollected_lane_open_count=2\nthreshold=2\n"),
        "{}",
        diagnostic.detail
    );
    assert!(
        diagnostic
            .detail
            .contains("\nlead_thread_id=progress-lead\n")
    );
    assert!(
        diagnostic.detail.contains("lane_ids=uncollected\n"),
        "{}",
        diagnostic.detail
    );

    // Collecting the cohort lifts the suppression; nothing more is reported for it.
    store
        .handle()
        .append_lane_report(
            &lane.lane_id,
            1,
            report(event_bus::LaneStatus::Ready),
            i64::MAX - 1,
        )
        .unwrap();
    command(&runtime, &model, &mut events, lead, "collect").await;
    let released = command(&runtime, &model, &mut events, lead, "open:released").await;
    assert_eq!(released["created"], true, "{released}");
    roots.push(released["root_run_id"].as_str().unwrap().parse().unwrap());
    assert!(
        diagnostics
            .drain_pending_snapshot()
            .into_iter()
            .all(|event| !matches!(event.kind, EventKind::Diagnostic(d) if d.code == CODE))
    );
    for run in roots.iter().copied().chain([lead]) {
        runtime.cancel(run).unwrap();
    }
    for run in roots.into_iter().chain([lead]) {
        runtime.wait(run).await.unwrap();
    }
}

/// The marker a suppression leaves in the persisted run snapshot is what the reader of
/// `evorch inspect run` decodes, and it names the cohort the diagnostic reported.
#[tokio::test]
async fn the_persisted_snapshot_carries_the_cohort_the_diagnostic_named() {
    const CODE: &str = event_bus::event::diagnostic_codes::LANE_UNCOLLECTED_CREATION_SUPPRESSED;
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("diagnostics.db"),
        ..Default::default()
    };
    let (store, model, runtime, mut events, mut diagnostics) = stored_runtime(&config);
    let lead = runtime
        .delegate_chat(
            "progress-lead",
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
    seed_uncollected(&store, lead, true);
    let mut roots = Vec::new();
    for n in 0..2 {
        let created = command(&runtime, &model, &mut events, lead, &format!("open:{n}")).await;
        roots.push(
            created["root_run_id"]
                .as_str()
                .unwrap()
                .parse::<RunId>()
                .unwrap(),
        );
    }
    let rejected = command(&runtime, &model, &mut events, lead, "open:blocked").await;
    assert_eq!(rejected["code"], "lane_creation_suppressed", "{rejected}");
    let reported = diagnostics
        .drain_pending_snapshot()
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::Diagnostic(d) if d.code == CODE => Some(d),
            _ => None,
        })
        .expect("the suppression is reported");
    let identity = reported
        .detail
        .lines()
        .find_map(|line| line.strip_prefix("identity="))
        .unwrap()
        .to_owned();

    let database = storage::Database::open_read_only(&config.db_path).unwrap();
    let inspected = database.inspect_run(&lead.to_string(), false).unwrap();
    let stored_config = &inspected["context"]["config_json"];
    assert!(
        stored_config["spawn_credits"]["uncollected_cohort_generation"]
            .as_u64()
            .is_some_and(|generation| generation >= 1),
        "runtime snapshot must persist a seeded cohort generation: {stored_config}"
    );
    let stored = runtime::budget_tracker::stored_uncollected_lanes(stored_config).unwrap();

    assert_eq!(stored.cohort_id.as_deref(), Some(identity.as_str()));
    assert!(!stored.cohort_id_derived);
    assert!(stored.suppression_reported);
    assert_eq!(stored.uncollected_lane_open_count, 2);
    assert_eq!(stored.lane_ids, ["uncollected"]);
    let mut identityless_config = stored_config.clone();
    identityless_config["spawn_credits"]
        .as_object_mut()
        .unwrap()
        .remove("uncollected_cohort_id");
    let derived = runtime::budget_tracker::stored_uncollected_lanes(&identityless_config).unwrap();
    assert!(derived.cohort_id_derived);
    assert_eq!(derived.cohort_id.as_deref(), Some(identity.as_str()));
    for run in roots.iter().copied().chain([lead]) {
        runtime.cancel(run).unwrap();
    }
    for run in roots.into_iter().chain([lead]) {
        runtime.wait(run).await.unwrap();
    }
}
