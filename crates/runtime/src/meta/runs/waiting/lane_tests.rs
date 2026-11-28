//! Real lane ledger/flush coverage, in addition to the memory predicate tests.
use super::*;
use crate::escalation_review::support::{ScriptedModel, text_response, tool_response};
use crate::lane_delivery::{LaneDeliveryHost, LaneDeliveryOutcome, LaneDeliveryTarget};
use crate::lane_management::{
    LaneHumanContext, LaneManagementAction, LaneManagementContext, LaneManagementRequest,
};
use storage::lane_delivery::LaneDeliveryNack;

const THREAD: &str = "wait-lane-destination";
const BODY: &str = "lane-report-visible-only-after-the-wait-result";

struct Host(LaneDeliveryTarget);
impl LaneDeliveryHost for Host {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        if thread == THREAD {
            Ok(self.0.clone())
        } else {
            Err(LaneDeliveryNack::UnknownThread)
        }
    }
    fn now_ns(&self) -> i64 {
        100
    }
}

struct Fixture {
    runtime: AgentRuntime,
    model: Arc<ScriptedModel>,
    storage: storage::Storage,
    bus: Arc<EventBus>,
    root: RunId,
    lane: crate::lanes::LaneOpened,
    child: Option<RunId>,
    authority: RunConfig,
    wait_request: usize,
    _dir: tempfile::TempDir,
}
impl Fixture {
    async fn new(with_child: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("lane-wait.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        // The independent lane root gets the unkeyed idle response.
        let model = Arc::new(ScriptedModel::new([Ok(text_response(
            "lane idle",
            FinishReason::Stop,
        ))]));
        model
            .add_keyed(
                "C9-LEAD",
                [Ok(text_response("lead idle", FinishReason::Stop))],
            )
            .await;
        model.gate_key("C9-BLOCKED", Arc::new(Notify::new())).await;
        let bus = Arc::new(EventBus::new(512));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus.clone())),
            model.clone(),
        )
        .with_sequential_run_ids()
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let authority = RunConfig {
            interactive: true,
            keep_alive: true,
            conversation: true,
            ..Default::default()
        };
        let root = runtime
            .delegate_chat(
                THREAD,
                Role::Orchestrator,
                "C9-LEAD".into(),
                authority.clone(),
            )
            .unwrap();
        model.wait_for_request(0).await;
        phase_waiting(&bus, &runtime, root).await;
        let lane = runtime
            .open_lane(
                root,
                "lane",
                event_bus::LaneBrief {
                    verification_requirements: Default::default(),
                    objective: "independent lane".into(),
                    inputs: vec![],
                    deliverables: vec!["artifact".into()],
                    acceptance_criteria: vec!["review".into()],
                    constraints: vec![],
                    workspace: Default::default(),
                    report_policy: event_bus::LanePolicy::Attached,
                },
            )
            .unwrap();
        model.wait_for_request(1).await;
        phase_waiting(&bus, &runtime, lane.root_run_id).await;
        let child = with_child.then(|| {
            runtime
                .delegate_background_as_child(
                    root,
                    Role::Worker,
                    "C9-BLOCKED",
                    RunConfig::default(),
                )
                .unwrap()
        });
        if child.is_some() {
            model.wait_for_request(2).await;
        }
        Self {
            runtime,
            model,
            storage,
            bus,
            root,
            lane,
            child,
            authority,
            wait_request: if with_child { 3 } else { 2 },
            _dir: dir,
        }
    }

    async fn start_wait(&self, input: Value) {
        self.model
            .add_keyed(
                "C9-LEAD",
                [
                    Ok(tool_response("lane-wait", "wait", input)),
                    Ok(text_response("after delivery", FinishReason::Stop)),
                    Ok(text_response("after control", FinishReason::Stop)),
                ],
            )
            .await;
        self.runtime
            .send_message(self.root, "start the wait".into())
            .unwrap();
        self.model.wait_for_request(self.wait_request).await;
        phase_waiting(&self.bus, &self.runtime, self.root).await;
    }
}

async fn phase_waiting(bus: &EventBus, runtime: &AgentRuntime, run: RunId) {
    let mut events = bus.subscribe();
    loop {
        let phase = runtime.inspect_agent(run).unwrap().phase;
        if phase == AgentRunPhase::Waiting {
            return;
        }
        assert!(
            !matches!(
                phase,
                AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
            ),
            "run {run} ended before the expected waiting boundary: {phase:?}"
        );
        events.recv().await.unwrap();
    }
}

fn wait_value(messages: &[Message]) -> Value {
    let text = messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == "lane-wait" => {
                assert!(!is_error);
                match &content[0] {
                    providers::ToolResultContent::Text { text } => Some(text),
                    _ => panic!("wait must return text"),
                }
            }
            _ => None,
        })
        .expect("wait result reaches the provider");
    serde_json::from_str(text).unwrap()
}

fn assert_post_tools_body_once(messages: &[Message]) {
    let mut pending = HashSet::new();
    let mut bodies = 0;
    let mut wait_returned = false;
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    assert!(pending.insert(id));
                }
                ContentBlock::ToolResult { tool_call_id, .. } => {
                    assert!(pending.remove(tool_call_id));
                    wait_returned |= tool_call_id == "lane-wait";
                }
                ContentBlock::Text { text } if text.contains(BODY) => {
                    assert_eq!(message.role, providers::Role::User);
                    assert!(wait_returned, "lane body cannot precede the wait result");
                    assert!(pending.is_empty(), "lane body cannot enter an open batch");
                    bodies += text.matches(BODY).count();
                }
                _ => {}
            }
        }
    }
    assert!(pending.is_empty());
    assert_eq!(bodies, 1);
}

#[tokio::test]
async fn real_lane_flush_follows_wait_result_once_in_every_form_including_partial_all() {
    for form in 0..3 {
        let f = Fixture::new(true).await;
        let child = f.child.unwrap().to_string();
        let input = match form {
            0 => json!({}),
            1 => json!({"run_id":child}),
            _ => json!({"run_ids":[child],"mode":"all"}),
        };
        // Install through the public host path, but stop its retry worker before
        // source commits: explicit drives are the test's completion barriers.
        let driver = f
            .runtime
            .bind_lane_delivery_host(Arc::new(Host(LaneDeliveryTarget {
                root_run_id: f.root,
                generation: 1,
                authority: f.authority.clone(),
            })))
            .unwrap();
        driver.abort();
        assert!(driver.await.unwrap_err().is_cancelled());
        f.start_wait(input).await;
        f.storage
            .handle()
            .append_lane_report_with_delivery(
                f.lane.lane_id.clone(),
                1,
                event_bus::LaneReportInput {
                    status: event_bus::LaneStatus::Ready,
                    summary: BODY.into(),
                    artifact_refs: vec![],
                    artifact_revision: None,
                    failure_episode: None,
                    needs_human: false,
                    question_refs: vec![],
                    reason_code: None,
                    producer: event_bus::LaneReportProducer::LaneAgent,
                },
                1,
            )
            .unwrap();
        assert_eq!(
            f.runtime.drive_lane_deliveries(THREAD).await.unwrap(),
            LaneDeliveryOutcome::Delivered { notifications: 1 }
        );
        let messages = f.model.wait_for_request(f.wait_request + 1).await;
        let result = wait_value(&messages);
        assert_eq!(result["status"], "ready");
        assert!(
            result["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "lane")
        );
        assert_eq!(
            result["runs"],
            json!([]),
            "unfinished child is not an output event"
        );
        assert!(!result.to_string().contains(BODY));
        assert_post_tools_body_once(&messages);
        phase_waiting(&f.bus, &f.runtime, f.root).await;
        assert_eq!(
            f.runtime.drive_lane_deliveries(THREAD).await.unwrap(),
            LaneDeliveryOutcome::Empty
        );
        f.runtime
            .send_message(f.root, "control turn".into())
            .unwrap();
        let later = f.model.wait_for_request(f.wait_request + 2).await;
        assert_post_tools_body_once(&later);
        cleanup(&f.runtime).await;
    }
}

#[tokio::test]
async fn real_close_and_detach_wake_a_wait_when_the_last_managed_lane_disappears() {
    for close in [false, true] {
        let f = Fixture::new(false).await;
        f.start_wait(json!({})).await;
        if close {
            let current = f
                .storage
                .handle()
                .lane_close_context(f.lane.lane_id.clone())
                .unwrap();
            f.runtime
                .close_lane_from_host(
                    LaneHumanContext,
                    crate::lane_close::HumanLaneCloseRequest {
                        lane_id: f.lane.lane_id.clone(),
                        ready_seq: None,
                        expected_management_revision: 1,
                        expected_request_revision: current.request_revision,
                        discard: true,
                        note: "remove the last wait source".into(),
                        surface: "test".into(),
                        client_id: None,
                    },
                )
                .unwrap();
        } else {
            f.runtime
                .detach_lane(
                    LaneManagementContext::Run(f.root),
                    LaneManagementRequest {
                        operation_id: "detach".into(),
                        lane_id: f.lane.lane_id.clone(),
                        expected_management_revision: 1,
                        action: LaneManagementAction::Detach,
                    },
                )
                .unwrap();
        }
        let messages = f.model.wait_for_request(f.wait_request + 1).await;
        assert_eq!(wait_value(&messages)["status"], "no_wait_sources");
        assert!(f.runtime.wait_lanes(f.root).is_empty());
        cleanup(&f.runtime).await;
    }
}
