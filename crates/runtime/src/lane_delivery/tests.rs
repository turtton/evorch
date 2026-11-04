use super::*;
use crate::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use event_bus::{EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, Message, ToolSpec};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
#[derive(Default)]
struct Model {
    finish: AtomicBool,
    requests: Mutex<Vec<Vec<Message>>>,
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
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let finish = self.finish.load(Ordering::SeqCst);
        Ok(providers::ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: if finish {
                    vec![ContentBlock::ToolUse {
                        id: format!("finish-{}", messages.len()),
                        name: "finish".into(),
                        input: serde_json::json!({"result":"complete"}),
                    }]
                } else {
                    vec![ContentBlock::Text {
                        text: "idle".into(),
                    }]
                },
            },
            finish_reason: if finish {
                providers::FinishReason::ToolUse
            } else {
                providers::FinishReason::Stop
            },
            usage: Default::default(),
            usage_reported: true,
        })
    }
}
struct Host {
    target: LaneDeliveryTarget,
    now: AtomicI64,
    restores: AtomicUsize,
    fail: AtomicBool,
    stop: AtomicBool,
    deny: Mutex<Option<LaneDeliveryNack>>,
}
impl LaneDeliveryHost for Host {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        if let Some(reason) = *self.deny.lock().unwrap() {
            return Err(reason);
        }
        if thread != "delivery-lead" {
            return Err(LaneDeliveryNack::UnknownThread);
        }
        Ok(self.target.clone())
    }
    fn restore_root(
        &self,
        runtime: &AgentRuntime,
        target: &LaneDeliveryTarget,
    ) -> Result<(), LaneDeliveryNack> {
        self.restores.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(LaneDeliveryNack::RestoreFailed);
        }
        if self.stop.swap(false, Ordering::SeqCst) {
            runtime
                .stop(target.root_run_id, crate::StopScope::SelfOnly)
                .unwrap();
            return Err(LaneDeliveryNack::Stopped);
        }
        runtime
            .wake_chat(target.root_run_id, target.authority.clone())
            .map(|_| ())
            .map_err(|error| {
                eprintln!("restore failed: {error}");
                LaneDeliveryNack::RestoreFailed
            })
    }
    fn now_ns(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}
struct Fixture {
    runtime: AgentRuntime,
    storage: storage::Storage,
    config: storage::StorageConfig,
    model: Arc<Model>,
    events: event_bus::EventReceiver,
    _dir: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("delivery.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let model = Arc::new(Model::default());
        let bus = Arc::new(EventBus::new(16384));
        let events = bus.subscribe();
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            model.clone(),
        )
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        Self {
            runtime,
            storage,
            config,
            model,
            events,
            _dir: dir,
        }
    }
    async fn phase(&mut self, id: RunId, expected: AgentRunPhase) {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) = self.events.recv().await.unwrap().kind
                && run_id == id.to_string()
            {
                assert_ne!(to, AgentRunPhase::Error, "{reason:?}");
                if to == expected {
                    return;
                }
            }
        }
    }
    async fn lead(&mut self) -> (RunId, Arc<Host>) {
        let authority = RunConfig {
            interactive: true,
            keep_alive: true,
            conversation: true,
            ..Default::default()
        };
        let lead = self
            .runtime
            .delegate_chat(
                "delivery-lead",
                Role::Orchestrator,
                "coordinate".into(),
                authority.clone(),
            )
            .unwrap();
        self.phase(lead, AgentRunPhase::Waiting).await;
        let host = Arc::new(Host {
            target: LaneDeliveryTarget {
                root_run_id: lead,
                generation: 1,
                authority,
            },
            now: AtomicI64::new(100),
            restores: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            deny: Mutex::new(None),
        });
        // Explicit drives are completion barriers; no background expiry timer is needed.
        *self.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
        (lead, host)
    }
    async fn lane(&mut self, lead: RunId, key: &str) -> crate::lanes::LaneOpened {
        let lane = self
            .runtime
            .open_lane(
                lead,
                key,
                event_bus::LaneBrief {
                    objective: key.into(),
                    inputs: vec![],
                    deliverables: vec!["artifact".into()],
                    acceptance_criteria: vec!["review".into()],
                    constraints: vec![],
                    workspace: Default::default(),
                    report_policy: event_bus::LanePolicy::Attached,
                },
            )
            .unwrap();
        self.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
        lane
    }
    async fn finish(&self, lead: RunId) {
        self.model.finish.store(true, Ordering::SeqCst);
        self.runtime
            .send_message(lead, "finish independently".into())
            .unwrap();
        self.runtime.wait(lead).await.unwrap();
        assert_eq!(
            self.runtime.inspect_agent(lead).unwrap().phase,
            AgentRunPhase::Done
        )
    }
    fn report(&self, lane: &crate::lanes::LaneOpened, rev: Option<u32>, status: LaneStatus) {
        let report = self
            .storage
            .handle()
            .append_lane_report_with_delivery(
                lane.lane_id.clone(),
                event_bus::LaneReportInput {
                    status,
                    summary: "artifact details".repeat(60),
                    artifact_refs: vec!["artifact:result".into()],
                    artifact_revision: rev
                        .map(|r| event_bus::lane::ArtifactRevision::CommitSha(format!("{r:040x}"))),
                    failure_episode: None,
                    needs_human: false,
                    question_refs: vec![],
                    reason_code: None,
                    producer: event_bus::LaneReportProducer::LaneAgent,
                },
                100,
            )
            .unwrap()
            .report;
        self.runtime.record_lane_report_progress(&report).unwrap()
    }
    fn question(&self, lane: &crate::lanes::LaneOpened, n: u32) {
        let q = event_bus::UserQuestion {
            responder: None,
            routing_revision: 0,
            management_revision: None,
            id: format!("question-{}-{n}", lane.lane_id),
            run_id: lane.root_run_id.to_string(),
            root_run_id: lane.root_run_id.to_string(),
            root_name: "lane".into(),
            recipient_run_ids: vec![],
            title: "scope?".into(),
            options: vec![],
            blocking: true,
            answer: None,
            answer_origin: None,
        };
        self.storage
            .handle()
            .create_lane_question(
                lane.lane_id.clone(),
                q,
                event_bus::lane::QuestionRequester {
                    run_id: lane.root_run_id.to_string(),
                    root_run_id: lane.root_run_id.to_string(),
                    thread_id: lane.thread_id.clone(),
                },
                LaneDeliveryBinding::Bound,
                100,
            )
            .unwrap();
    }
    fn sql(&self, sql: &str) {
        rusqlite::Connection::open(&self.config.db_path)
            .unwrap()
            .execute_batch(sql)
            .unwrap()
    }
    fn receipts(&self) -> usize {
        rusqlite::Connection::open(&self.config.db_path).unwrap().query_row("SELECT count(*) FROM host_input_receipts WHERE json_extract(provenance_json,'$.kind')='lane_notification'",[],|r|r.get(0)).unwrap()
    }
    async fn drive(&self) -> LaneDeliveryOutcome {
        self.runtime
            .drive_lane_deliveries("delivery-lead")
            .await
            .unwrap()
    }
}
#[tokio::test]
async fn ten_sources_single_flight_bounded_details_and_receipts() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let mut lanes = Vec::new();
    for n in 0..10 {
        lanes.push(f.lane(lead, &format!("lane-{n}")).await)
    }
    f.finish(lead).await;
    for lane in &lanes {
        f.report(lane, Some(1), LaneStatus::Ready)
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(11));
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let r = f.runtime.clone();
        let b = barrier.clone();
        tasks.push(tokio::spawn(async move {
            b.wait().await;
            r.drive_lane_deliveries("delivery-lead").await.unwrap()
        }))
    }
    barrier.wait().await;
    let mut received = 0;
    for task in tasks {
        if let LaneDeliveryOutcome::Delivered { notifications } = task.await.unwrap() {
            received += notifications
        }
    }
    f.runtime.wait(lead).await.unwrap();
    assert_eq!(received, 10);
    assert_eq!(host.restores.load(Ordering::SeqCst), 1);
    assert_eq!(f.receipts(), 10);
    let requests = f.model.requests.lock().unwrap();
    let notice = requests
        .last()
        .unwrap()
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::Text { text }
                if text.starts_with("[Agent-authored lane notifications") =>
            {
                Some(text)
            }
            _ => None,
        })
        .unwrap();
    assert!(notice.len() <= storage::lane_delivery::LANE_DELIVERY_PAGE_BYTES);
    for lane in &lanes {
        assert!(notice.contains(&lane.lane_id))
    }
    assert_eq!(
        f.storage
            .handle()
            .lane_inbox("delivery-lead".into(), String::new())
            .unwrap()
            .len(),
        10
    );
}
#[tokio::test]
async fn twenty_five_progress_wakes_leave_auto_wake_budget_unchanged() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "progress").await;
    f.finish(lead).await;
    f.runtime.shared.auto_wakes.lock().unwrap().insert(lead, 20);
    for n in 1..=25 {
        f.report(&lane, Some(n), LaneStatus::Ready);
        assert_eq!(
            f.drive().await,
            LaneDeliveryOutcome::Delivered { notifications: 1 }
        );
        f.runtime.wait(lead).await.unwrap();
    }
    assert_eq!(host.restores.load(Ordering::SeqCst), 25);
    assert_eq!(f.runtime.shared.auto_wakes.lock().unwrap()[&lead], 20);
    assert_eq!(f.receipts(), 25);
    assert!(
        !f.storage
            .handle()
            .lane_wake_guard(lane.lane_id)
            .unwrap()
            .suppressed
    );
}
#[tokio::test]
async fn guard_is_per_source_durable_and_finish_independent() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "loop").await;
    let other = f.lane(lead, "healthy").await;
    f.finish(lead).await;
    for n in 0..20 {
        f.question(&lane, n);
        assert_eq!(
            f.drive().await,
            LaneDeliveryOutcome::Delivered { notifications: 1 }
        );
        f.runtime.wait(lead).await.unwrap();
    }
    f.question(&lane, 20);
    assert_eq!(f.drive().await, LaneDeliveryOutcome::Empty);
    let guard = f
        .storage
        .handle()
        .lane_wake_guard(lane.lane_id.clone())
        .unwrap();
    assert!(guard.suppressed);
    assert_eq!(guard.no_progress_batches, 20);
    f.report(&other, Some(1), LaneStatus::Ready);
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.runtime.wait(lead).await.unwrap();
    // A fresh runtime and writer (no in-memory flight/guard state) still defer it.
    let reopened = storage::Storage::open(f.config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let restarted = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&f.config, reopened.handle()).unwrap());
    let host = f
        .runtime
        .shared
        .lanes
        .delivery
        .host
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    *restarted.shared.lanes.delivery.host.lock().unwrap() = Some(host);
    assert_eq!(
        restarted
            .drive_lane_deliveries("delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    f.report(&lane, None, LaneStatus::Working);
    assert!(
        f.storage
            .handle()
            .lane_wake_guard(lane.lane_id.clone())
            .unwrap()
            .suppressed
    );
    f.report(&lane, Some(7), LaneStatus::Ready);
    assert!(
        !f.storage
            .handle()
            .lane_wake_guard(lane.lane_id.clone())
            .unwrap()
            .suppressed
    );
    assert!(matches!(
        f.drive().await,
        LaneDeliveryOutcome::Delivered { .. }
    ));
    f.runtime.wait(lead).await.unwrap();
    assert_eq!(
        f.runtime.inspect_agent(lead).unwrap().phase,
        AgentRunPhase::Done
    );
}
#[tokio::test]
async fn stop_at_restore_boundary_requires_explicit_resume() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "stop").await;
    f.finish(lead).await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    host.stop.store(true, Ordering::SeqCst);
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::Stopped)
    );
    assert_eq!(f.receipts(), 0);
    assert!(
        f.storage
            .handle()
            .lane_delivery_destination("delivery-lead".into())
            .unwrap()
            .unwrap()
            .stopped
    );
    f.report(&lane, Some(2), LaneStatus::Ready);
    assert!(matches!(
        f.drive().await,
        LaneDeliveryOutcome::Stopped | LaneDeliveryOutcome::Nacked(LaneDeliveryNack::Stopped)
    ));
    f.runtime
        .resume_chat(lead, host.target.authority.clone())
        .unwrap();
    f.runtime.wait(lead).await.unwrap();
    assert_eq!(f.receipts(), 2);
    assert!(
        !f.storage
            .handle()
            .lane_delivery_destination("delivery-lead".into())
            .unwrap()
            .unwrap()
            .stopped
    );
}
#[tokio::test]
async fn snapshot_failure_releases_lease_without_provider_send() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "snapshot").await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    let before = f.model.requests.lock().unwrap().len();
    f.sql("CREATE TRIGGER fail_lane_receipt BEFORE INSERT ON host_input_receipts WHEN json_extract(NEW.provenance_json,'$.kind')='lane_notification' BEGIN SELECT RAISE(ABORT,'injected'); END;");
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::StorageFailed)
    );
    assert_eq!(f.receipts(), 0);
    assert_eq!(f.model.requests.lock().unwrap().len(), before);
    f.sql("DROP TRIGGER fail_lane_receipt");
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
}
#[tokio::test]
async fn receipt_before_ack_crash_repairs_without_reappend_or_wake() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "ack").await;
    f.finish(lead).await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    f.sql("CREATE TRIGGER fail_lane_ack BEFORE UPDATE OF state ON lane_notifications WHEN NEW.state='acked' BEGIN SELECT RAISE(ABORT,'injected'); END;");
    assert!(
        f.runtime
            .drive_lane_deliveries("delivery-lead")
            .await
            .is_err()
    );
    f.runtime.wait(lead).await.unwrap();
    assert_eq!(f.receipts(), 1);
    let requests = f.model.requests.lock().unwrap().len();
    f.sql("DROP TRIGGER fail_lane_ack");
    host.now.fetch_add(LEASE_NS, Ordering::SeqCst);
    assert_eq!(f.drive().await, LaneDeliveryOutcome::Empty);
    assert_eq!(f.model.requests.lock().unwrap().len(), requests);
    assert_eq!(host.restores.load(Ordering::SeqCst), 1);
    assert!(
        f.storage
            .handle()
            .pending_lane_delivery_destinations(String::new())
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn working_deferred_and_intervention_never_copies_input() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "milestone").await;
    f.finish(lead).await;
    f.report(&lane, None, LaneStatus::Working);
    assert_eq!(f.drive().await, LaneDeliveryOutcome::Empty);
    assert_eq!(host.restores.load(Ordering::SeqCst), 0);
    f.model.finish.store(false, Ordering::SeqCst);
    f.runtime
        .continue_goal(lead, "next user turn".into(), host.target.authority.clone())
        .unwrap();
    f.phase(lead, AgentRunPhase::Waiting).await;
    f.runtime
        .send_message(lane.root_run_id, "private lane input never copied".into())
        .unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let notices = f
        .storage
        .handle()
        .lane_inbox("delivery-lead".into(), String::new())
        .unwrap();
    assert!(
        notices
            .iter()
            .any(|n| matches!(n.kind, LaneNotificationKind::Intervention { .. }))
    );
    assert!(
        !serde_json::to_string(&notices)
            .unwrap()
            .contains("private lane input")
    );
}
#[tokio::test]
async fn nacks_hold_notifications_until_an_explicit_redrive() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "retry").await;
    f.finish(lead).await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    for reason in [
        LaneDeliveryNack::UnknownThread,
        LaneDeliveryNack::Readonly,
        LaneDeliveryNack::OwnershipLost,
    ] {
        *host.deny.lock().unwrap() = Some(reason);
        assert_eq!(f.drive().await, LaneDeliveryOutcome::Nacked(reason))
    }
    *host.deny.lock().unwrap() = None;
    host.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::RestoreFailed)
    );
    assert_eq!(host.restores.load(Ordering::SeqCst), 1);
    assert_eq!(f.receipts(), 0);
    host.fail.store(false, Ordering::SeqCst);
    f.runtime.drive_pending_lane_deliveries().await.unwrap();
    f.runtime.wait(lead).await.unwrap();
    assert_eq!(f.receipts(), 1);
}

#[tokio::test]
async fn a_crashed_claim_is_retried_at_the_exact_injected_lease_boundary() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "leased").await;
    f.finish(lead).await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    let owner = host.target.owner();
    let h = f.storage.handle();
    let epoch = h
        .bind_lane_delivery_destination(
            "delivery-lead".into(),
            h.lane_delivery_destination("delivery-lead".into())
                .unwrap()
                .map_or(0, |d| d.epoch),
            owner.clone(),
            LaneDestinationAction::Bind,
        )
        .unwrap();
    let old = h
        .claim_lane_deliveries(LaneClaimRequest {
            destination_thread_id: "delivery-lead".into(),
            expected_destination_epoch: epoch,
            owner,
            token: "crashed-owner".into(),
            now_ns: 100,
            lease_until_ns: 200,
            allow_non_waking: false,
        })
        .unwrap();
    let LaneClaimResult::Claimed(old) = old else {
        panic!("claim missing")
    };
    assert_eq!(f.drive().await, LaneDeliveryOutcome::Busy);
    assert_eq!(host.restores.load(Ordering::SeqCst), 0);
    host.now.store(200, Ordering::SeqCst);
    assert!(h.ack_lane_delivery(old.clone(), 200).is_err());
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.runtime.wait(lead).await.unwrap();
    assert_eq!(f.receipts(), 1);
    // Once every row is already ACKed, even a late ACK is an idempotent no-op.
    h.ack_lane_delivery(old, 200).unwrap();
}

#[tokio::test]
async fn persisted_stop_survives_a_fresh_runtime_and_startup_scan() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "stopped-on-restart").await;
    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
    f.report(&lane, Some(1), LaneStatus::Ready);
    let storage = storage::Storage::open(f.config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let restarted = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&f.config, storage.handle()).unwrap());
    *restarted.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
    restarted.drive_pending_lane_deliveries().await.unwrap();
    assert_eq!(host.restores.load(Ordering::SeqCst), 0);
    assert_eq!(f.receipts(), 0);
    assert!(
        storage
            .handle()
            .lane_delivery_destination("delivery-lead".into())
            .unwrap()
            .unwrap()
            .stopped
    );
}

#[tokio::test]
async fn an_inactive_checkpoint_with_unknown_stop_state_never_auto_wakes() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "unknown-stop").await;
    f.finish(lead).await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    f.sql(&format!(
        "UPDATE run_contexts SET terminal_phase='Checkpoint' WHERE run_id='{lead}'"
    ));
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::Stopped)
    );
    assert_eq!(host.restores.load(Ordering::SeqCst), 0);
    assert_eq!(f.receipts(), 0);
}
#[test]
fn binding_without_an_executor_remains_unbound() {
    let f = Fixture::new();
    let host = Arc::new(Host {
        target: LaneDeliveryTarget {
            root_run_id: RunId::new(1),
            generation: 1,
            authority: RunConfig::default(),
        },
        now: AtomicI64::new(100),
        restores: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        deny: Mutex::new(None),
    });
    assert!(f.runtime.bind_lane_delivery_host(host).is_none());
    assert_eq!(
        f.runtime.lane_delivery_binding(),
        LaneDeliveryBinding::Unbound
    );
}
