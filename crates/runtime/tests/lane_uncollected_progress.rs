//! Real runtime/storage boundary accounting, with event-driven model turns.
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

async fn exercise_creation_boundary(terminal: bool) {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("progress.db"),
        ..Default::default()
    };
    let (store, model, runtime, mut events) = stored_runtime(&config);
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
    let lane = seed_uncollected(&store, lead, terminal);
    let mut roots = Vec::new();
    for n in 0..100 {
        let command = format!("open:{n}");
        runtime.send_message(lead, command.clone()).unwrap();
        waiting(&mut events, lead).await;
        let results = model.results.lock().unwrap();
        let value = &results
            .iter()
            .rev()
            .find(|(id, _)| id == &command)
            .unwrap()
            .1;
        assert_eq!(value["created"], true, "new intent {n}: {value}");
        roots.push(
            value["root_run_id"]
                .as_str()
                .unwrap()
                .parse::<RunId>()
                .unwrap(),
        );
    }
    runtime.send_message(lead, "open:blocked".into()).unwrap();
    waiting(&mut events, lead).await;
    let rejected = model.results.lock().unwrap().last().unwrap().1.clone();
    if terminal {
        assert_eq!(rejected["code"], "lane_creation_suppressed");
        assert_eq!(rejected["uncollected_lane_open_count"], 100);
        // Re-numbering a report isn't collection; make the actual full report easy
        // to select without treating the internal scan as provider observation.
        store
            .handle()
            .append_lane_report(
                &lane.lane_id,
                1,
                report(event_bus::LaneStatus::Ready),
                i64::MAX - 1,
            )
            .unwrap();
        runtime.send_message(lead, "collect".into()).unwrap();
        waiting(&mut events, lead).await;
        runtime.send_message(lead, "open:released".into()).unwrap();
        waiting(&mut events, lead).await;
        let released = model.results.lock().unwrap().last().unwrap().1.clone();
        assert_eq!(released["created"], true, "{released}");
        roots.push(released["root_run_id"].as_str().unwrap().parse().unwrap());
    } else {
        assert_eq!(
            rejected["created"], true,
            "running lanes are not a quantity limit: {rejected}"
        );
        roots.push(rejected["root_run_id"].as_str().unwrap().parse().unwrap());
    }
    for run in roots.iter().copied().chain([lead]) {
        runtime.cancel(run).unwrap();
    }
    for run in roots.into_iter().chain([lead]) {
        runtime.wait(run).await.unwrap();
    }
}

#[tokio::test]
async fn hundred_successful_new_intents_suppress_only_the_next_open_until_collection() {
    exercise_creation_boundary(true).await;
}

#[tokio::test]
async fn more_than_a_hundred_nonterminal_lanes_are_not_a_quantity_limit() {
    exercise_creation_boundary(false).await;
}

fn stored_runtime(
    config: &storage::StorageConfig,
) -> (
    storage::Storage,
    Arc<Model>,
    AgentRuntime,
    event_bus::EventReceiver,
) {
    let store = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let events = bus.subscribe();
    let model = Arc::new(Model::default());
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        model.clone(),
    )
    .with_run_store(runtime::RunStore::open(config, store.handle()).unwrap());
    (store, model, runtime, events)
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

// Both paths reopen SQLite and construct a new runtime + RunStore. In the first,
// an older saved budget has no marker for an already durable intent. In the
// second, admission fails after the new intent is recorded in that budget.
async fn retry_after_runtime_restart(admission_failure: bool) {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("retry.db"),
        ..Default::default()
    };
    let (store, model, runtime, mut events) = stored_runtime(&config);
    let lead = runtime
        .delegate_chat(
            "progress-lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                spawn_credits: serde_json::from_value(json!({
                    "spawned": [],
                    "observed": [],
                    "unobserved": 20,
                    "uncollected_lane_open_count": 99,
                    "uncollected_lane_ids": ["uncollected"]
                }))
                .unwrap(),
                ..Default::default()
            },
        )
        .unwrap();
    waiting(&mut events, lead).await;
    seed_uncollected(&store, lead, true);
    let old_lane = if admission_failure {
        model
            .reject_lane_admission
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let opened = command(&runtime, &model, &mut events, lead, "open:existing").await;
        assert_eq!(opened["created"], true, "{opened}");
        let root: RunId = opened["root_run_id"].as_str().unwrap().parse().unwrap();
        assert!(runtime.wait(root).await.is_err());
        assert!(
            !store
                .handle()
                .lane(opened["lane_id"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .is_started()
        );
        opened["lane_id"].as_str().unwrap().to_owned()
    } else {
        // The external durable intent is intentionally absent from the saved
        // lead's SpawnCredits, just as with a pre-existing snapshot.
        let mut old_brief = brief();
        old_brief["objective"] = json!("independent artifact open:existing");
        let opened = runtime
            .open_lane(
                lead,
                &format!("{lead}:open:existing"),
                serde_json::from_value(old_brief).unwrap(),
            )
            .unwrap();
        waiting(&mut events, opened.root_run_id).await;
        runtime
            .stop(opened.root_run_id, runtime::StopScope::SelfOnly)
            .unwrap();
        runtime.wait(opened.root_run_id).await.unwrap();
        opened.lane_id
    };
    runtime.stop(lead, runtime::StopScope::SelfOnly).unwrap();
    runtime.wait(lead).await.unwrap();
    drop(runtime);
    drop(model);
    drop(store);

    let (_store, model, runtime, mut events) = stored_runtime(&config);
    runtime
        .resume_chat(
            lead,
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    waiting(&mut events, lead).await;
    let retried = command(&runtime, &model, &mut events, lead, "open:existing").await;
    assert_eq!(retried["created"], false, "{retried}");
    assert_eq!(retried["lane_id"], old_lane);
    let root = if admission_failure {
        retried["root_run_id"]
            .as_str()
            .unwrap()
            .parse::<RunId>()
            .unwrap()
    } else {
        let new = command(
            &runtime,
            &model,
            &mut events,
            lead,
            "open:new-after-restart",
        )
        .await;
        assert_eq!(
            new["created"], true,
            "an existing intent must not consume the hundredth slot: {new}"
        );
        new["root_run_id"]
            .as_str()
            .unwrap()
            .parse::<RunId>()
            .unwrap()
    };
    let blocked = command(
        &runtime,
        &model,
        &mut events,
        lead,
        "open:blocked-after-restart",
    )
    .await;
    assert_eq!(blocked["code"], "lane_creation_suppressed");
    assert_eq!(blocked["uncollected_lane_open_count"], 100);
    for run in [root, lead] {
        runtime.cancel(run).unwrap();
    }
    for run in [root, lead] {
        runtime.wait(run).await.unwrap();
    }
}

#[tokio::test]
async fn restored_existing_intent_without_a_credit_marker_does_not_count_as_new() {
    retry_after_runtime_restart(false).await;
}

#[tokio::test]
async fn failed_admission_intent_is_counted_once_across_a_new_runtime_and_run_store() {
    retry_after_runtime_restart(true).await;
}
