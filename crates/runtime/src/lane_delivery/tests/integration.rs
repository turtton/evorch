//! Cross-lane scale and lifecycle regression using the real execution/storage paths.
use super::*;
#[path = "accepted_answer_retention.rs"]
mod accepted_answer_retention;
use crate::runtime::questions::routing::LaneAnswerRequest;
use event_bus::lane::{LaneAttentionFilter, LaneAttentionItem, LaneAttentionKind};

tokio::task_local! { pub(crate) static MANUAL_SOURCES: (); }
struct Host {
    targets: Mutex<HashMap<String, LaneDeliveryTarget>>,
    deny: Mutex<Option<LaneDeliveryNack>>,
    fail: AtomicBool,
    restores: AtomicUsize,
    now: AtomicI64,
}
impl LaneDeliveryHost for Host {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        // Bindings are controlled explicitly; test source commits do not start
        // an unobserved competing driver. Runtime input acceptance still resolves.
        if let Some(reason) = *self.deny.lock().unwrap() {
            return Err(reason);
        }
        self.targets
            .lock()
            .unwrap()
            .get(thread)
            .cloned()
            .ok_or(LaneDeliveryNack::UnknownThread)
    }
    fn restore_root(
        &self,
        runtime: &AgentRuntime,
        target: &LaneDeliveryTarget,
    ) -> Result<(), LaneDeliveryNack> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(LaneDeliveryNack::RestoreFailed);
        }
        self.restores.fetch_add(1, Ordering::SeqCst);
        runtime
            .wake_chat(target.root_run_id, target.authority.clone())
            .map(|_| ())
            .map_err(|_| LaneDeliveryNack::RestoreFailed)
    }
    fn now_ns(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}
fn attention(f: &Fixture) -> Vec<LaneAttentionItem> {
    let db = storage::Database::open_read_only(&f.config.db_path).unwrap();
    let mut items = Vec::new();
    let mut cursor = None;
    loop {
        let page = db
            .lane_attention(
                LaneAttentionFilter {
                    lead_thread_id: "delivery-lead".into(),
                    project_id: None,
                },
                cursor,
                20,
            )
            .unwrap();
        assert!(page.items.len() <= 20);
        items.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return items;
        }
    }
}
async fn drive(f: &Fixture, thread: &str) -> LaneDeliveryOutcome {
    f.runtime.drive_lane_deliveries(thread).await.unwrap()
}
#[tokio::test]
async fn twelve_lanes_forty_child_questions_and_twenty_five_progress_wakes_remain_independent() {
    MANUAL_SOURCES.scope((), run_scale()).await;
}
async fn run_scale() {
    let mut f = Fixture::new();
    let (lead, old) = f.lead().await;
    let mut lanes = Vec::new();
    for n in 0..12 {
        lanes.push(f.lane(lead, &format!("cross-{n}")).await);
    }
    let mut targets = HashMap::from([("delivery-lead".into(), old.target.clone())]);
    for lane in &lanes {
        targets.insert(
            lane.thread_id.clone(),
            LaneDeliveryTarget {
                root_run_id: lane.root_run_id,
                ..old.target.clone()
            },
        );
        assert!(
            f.runtime
                .shared
                .runs
                .lock()
                .unwrap()
                .get(&lane.root_run_id)
                .unwrap()
                .parent
                .is_none()
        );
    }
    let host = Arc::new(Host {
        targets: Mutex::new(targets),
        deny: Mutex::new(None),
        fail: AtomicBool::new(false),
        restores: AtomicUsize::new(0),
        now: AtomicI64::new(crate::run_store::review_time_ns()),
    });
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
    assert!(attention(&f).is_empty());
    let mut children = Vec::new();
    let mut questions = Vec::new();
    for lane in lanes.iter().take(10) {
        let child = f
            .runtime
            .spawn_reserved_child(
                lane.root_run_id,
                f.runtime.reserve_run_id(),
                Role::Worker,
                "question requester",
                RunConfig::default(),
            )
            .unwrap();
        f.runtime.wait(child).await.unwrap();
        f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
        children.push(child);
        for n in 0..4 {
            let q = f
                .runtime
                .request_user_question(child, format!("scope {n}?"), vec![], false)
                .unwrap();
            f.runtime
                .forward_lane_question(lane.root_run_id, &q.id, &format!("forward-{n}"))
                .unwrap();
            let routed = f.runtime.user_question(&q.id).unwrap().unwrap();
            assert_eq!(routed.run_id, q.run_id);
            assert_eq!(routed.root_run_id, q.root_run_id);
            assert_eq!(routed.recipient_run_ids, q.recipient_run_ids);
            assert_eq!(
                f.runtime
                    .answer_user_question(&q.id, "no bypass")
                    .unwrap_err(),
                "not_responder"
            );
            questions.push(routed);
        }
    }
    assert_eq!(questions.len(), 40);
    assert_eq!(
        attention(&f)
            .iter()
            .filter(|i| matches!(i.kind, LaneAttentionKind::QuestionPending { .. }))
            .count(),
        40
    );
    assert!(
        f.runtime.user_answers(lead).unwrap().is_empty(),
        "40 routed questions consume no lead question links"
    );
    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
    assert!(
        lanes.iter().all(
            |l| f.runtime.inspect_agent(l.root_run_id).unwrap().phase == AgentRunPhase::Waiting
        )
    );
    assert_eq!(
        drive(&f, "delivery-lead").await,
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::Stopped)
    );
    assert!(
        attention(&f)
            .iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::StopPending { .. }))
    );
    assert!(questions.iter().all(|q| {
        f.runtime
            .user_question(&q.id)
            .unwrap()
            .unwrap()
            .answer
            .is_none()
    }));
    f.runtime
        .resume_chat(lead, old.target.authority.clone())
        .unwrap();
    f.phase(lead, AgentRunPhase::Waiting).await;
    *host.deny.lock().unwrap() = Some(LaneDeliveryNack::OwnershipLost);
    assert_eq!(
        drive(&f, "delivery-lead").await,
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::OwnershipLost)
    );
    assert_eq!(
        attention(&f)
            .iter()
            .filter(|i| matches!(i.kind, LaneAttentionKind::QuestionPending { .. }))
            .count(),
        40
    );
    *host.deny.lock().unwrap() = None;
    let mut delivered = 0;
    while delivered < 40 {
        let outcome = drive(&f, "delivery-lead").await;
        let LaneDeliveryOutcome::Delivered { notifications } = outcome else {
            panic!("pending questions were not delivered: {outcome:?}")
        };
        assert!(notifications > 0);
        delivered += notifications;
        f.phase(lead, AgentRunPhase::Waiting).await;
    }
    assert!(
        !attention(&f)
            .iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::StopPending { .. }))
    );
    for q in &questions {
        f.runtime
            .resolve_lane_question(
                lead,
                LaneAnswerRequest {
                    question_id: q.id.clone(),
                    management_revision: q.management_revision.unwrap(),
                    routing_revision: q.routing_revision,
                    operation_id: format!("answer-{}", q.id),
                    answer: Some("agent task-scope decision".into()),
                },
            )
            .unwrap();
    }
    assert!(
        attention(&f).is_empty(),
        "answers resolve attention, not reading or notification ACKs"
    );
    for (lane, child) in lanes.iter().zip(children) {
        assert_eq!(
            drive(&f, &lane.thread_id).await,
            LaneDeliveryOutcome::Delivered { notifications: 4 }
        );
        f.runtime.wait(child).await.unwrap();
        assert!(
            f.runtime
                .user_answers(child)
                .unwrap()
                .iter()
                .all(|q| q.answer.as_deref() == Some("agent task-scope decision"))
        );
    }
    // Reuse the real monitor, ownership permit and driver after all forwarded
    // questions resolve. Only the injected host clock advances: no wall-time wait.
    let lane = &lanes[0];
    let mut phase = f.runtime.lane_delivery_phase(lane.root_run_id).unwrap();
    phase
        .wait_for(|p| *p == AgentRunPhase::Waiting)
        .await
        .unwrap();
    let owner = crate::ownership::OwnerHost::open(
        f._dir.sockets(),
        Default::default(),
        f.runtime.shared.bus.clone(),
    )
    .unwrap();
    let permit = owner.start(&lane.thread_id).unwrap();
    host.targets
        .lock()
        .unwrap()
        .get_mut(&lane.thread_id)
        .unwrap()
        .authority
        .ownership = Some(permit);
    f.runtime.start_lane_monitor();
    f.runtime.monitor_lanes().unwrap();
    host.now.fetch_add(60 * 60_000_000_000, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert!(
        attention(&f)
            .iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::Stall { .. }))
    );
    phase.borrow_and_update();
    let legacy_before = f
        .runtime
        .shared
        .auto_wakes
        .lock()
        .unwrap()
        .get(&lane.root_run_id)
        .copied();
    assert_eq!(
        drive(&f, &lane.thread_id).await,
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    assert_eq!(
        f.runtime
            .shared
            .auto_wakes
            .lock()
            .unwrap()
            .get(&lane.root_run_id)
            .copied(),
        legacy_before
    );
    phase.changed().await.unwrap();
    phase
        .wait_for(|p| *p == AgentRunPhase::Waiting)
        .await
        .unwrap();
    // Finish is independent of the other twelve live roots and their ledger.
    f.finish(lead).await;
    f.runtime.shared.auto_wakes.lock().unwrap().insert(lead, 20);
    for n in 0..25 {
        let lane = &lanes[n % lanes.len()];
        f.report(lane, Some(n as u32 + 1), LaneStatus::Ready);
        if n == 3 {
            host.fail.store(true, Ordering::SeqCst);
            assert_eq!(
                drive(&f, "delivery-lead").await,
                LaneDeliveryOutcome::Nacked(LaneDeliveryNack::RestoreFailed)
            );
            assert!(
                attention(&f)
                    .iter()
                    .any(|i| matches!(i.kind, LaneAttentionKind::RestoreFailed { .. }))
            );
            host.fail.store(false, Ordering::SeqCst);
        }
        assert_eq!(
            drive(&f, "delivery-lead").await,
            LaneDeliveryOutcome::Delivered { notifications: 1 }
        );
        f.runtime.wait(lead).await.unwrap();
    }
    assert!(
        !attention(&f)
            .iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::Stall { .. }))
    );
    assert_eq!(host.restores.load(Ordering::SeqCst), 25);
    assert_eq!(f.runtime.shared.auto_wakes.lock().unwrap()[&lead], 20);
    assert_eq!(
        attention(&f)
            .iter()
            .filter(|i| matches!(i.kind, LaneAttentionKind::ReadyUnclosed { .. }))
            .count(),
        12
    );
    assert!(
        !attention(&f)
            .iter()
            .any(|i| matches!(i.kind, LaneAttentionKind::RestoreFailed { .. }))
    );
    f.model.finish.store(false, Ordering::SeqCst);
    let lane = &lanes[11];
    let record = f
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(lane.root_run_id)
        .unwrap()
        .unwrap();
    let messages: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
    let fork = f
        .runtime
        .delegate_chat_seeded(
            "cross-fork",
            Role::Orchestrator,
            "fork".into(),
            old.target.authority.clone(),
            Some(crate::ChatForkSeed {
                source_run_id: lane.root_run_id.to_string(),
                context_len: crate::restore::conversation_len(&messages) as u64,
            }),
        )
        .unwrap();
    f.phase(fork, AgentRunPhase::Waiting).await;
    assert_eq!(
        f.runtime
            .shared
            .runs
            .lock()
            .unwrap()
            .get(&fork)
            .unwrap()
            .config
            .lane_scope,
        Some(event_bus::LaneScope::Lane)
    );
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    let rewind = f
        .runtime
        .delegate_chat(
            &lane.thread_id,
            Role::Orchestrator,
            "rewind".into(),
            old.target.authority.clone(),
        )
        .unwrap();
    f.phase(rewind, AgentRunPhase::Waiting).await;
    assert_eq!(
        f.runtime
            .shared
            .runs
            .lock()
            .unwrap()
            .get(&rewind)
            .unwrap()
            .config
            .lane_scope,
        Some(event_bus::LaneScope::Lane)
    );
    assert_eq!(
        f.storage
            .handle()
            .lane(&lane.lane_id)
            .unwrap()
            .unwrap()
            .root_run_id,
        Some(rewind.to_string())
    );
    let plain = f
        .runtime
        .delegate_chat(
            "ordinary",
            Role::Worker,
            "ordinary chat".into(),
            old.target.authority.clone(),
        )
        .unwrap();
    f.phase(plain, AgentRunPhase::Waiting).await;
    assert!(
        f.runtime
            .shared
            .runs
            .lock()
            .unwrap()
            .get(&plain)
            .unwrap()
            .config
            .lane_scope
            .is_none()
    );
    let q = f
        .runtime
        .request_user_question(plain, "ordinary user question".into(), vec![], false)
        .unwrap();
    f.runtime
        .answer_user_question(&q.id, "human scope")
        .unwrap();
    assert_eq!(
        attention(&f)
            .iter()
            .filter(|i| matches!(i.kind, LaneAttentionKind::ReadyUnclosed { .. }))
            .count(),
        12
    );
    for run in lanes
        .iter()
        .take(11)
        .map(|l| l.root_run_id)
        .chain([fork, rewind, plain])
    {
        f.runtime.cancel(run).unwrap();
        f.runtime.wait(run).await.unwrap();
    }
}
