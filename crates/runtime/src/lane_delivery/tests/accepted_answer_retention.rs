//! Accepted lead replies outlive the lead; delivery and child continuation use
//! the real driver, receipts and runtime restore path, not copied SQL predicates.
use super::*;
use storage::lane_management::{LaneManagementAction, LaneManagementActor, LaneManagementRequest};

#[tokio::test]
async fn detached_stopped_lane_delivers_same_answer_after_old_lead_reclaim_and_resumes_child() {
    MANUAL_SOURCES.scope((), scenario()).await;
}
async fn scenario() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "retained-answer").await;
    let authority = f.runtime.shared.runs.lock().unwrap()[&lane.root_run_id]
        .config
        .clone();
    let host = Arc::new(Host {
        targets: Mutex::new(HashMap::from([(
            lane.thread_id.clone(),
            LaneDeliveryTarget {
                root_run_id: lane.root_run_id,
                generation: 1,
                authority: authority.clone(),
            },
        )])),
        deny: Mutex::new(None),
        fail: AtomicBool::new(false),
        restores: AtomicUsize::new(0),
        now: AtomicI64::new(crate::run_store::review_time_ns()),
    });
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host);
    let child = f
        .runtime
        .spawn_reserved_child(
            lane.root_run_id,
            f.runtime.reserve_run_id(),
            Role::Worker,
            "request a scoped decision",
            RunConfig::default(),
        )
        .unwrap();
    f.runtime.wait(child).await.unwrap();
    let q = f
        .runtime
        .request_user_question(child, "scope?".into(), vec![], false)
        .unwrap();
    f.runtime
        .forward_lane_question(lane.root_run_id, &q.id, "forward")
        .unwrap();
    f.runtime
        .resolve_lane_question(
            lead,
            LaneAnswerRequest {
                question_id: q.id.clone(),
                management_revision: 1,
                routing_revision: 1,
                operation_id: "accept".into(),
                answer: Some("retained-scope-answer".into()),
            },
        )
        .unwrap();
    let answered = f.runtime.user_question(&q.id).unwrap().unwrap();
    let h = f.storage.handle();
    let route = h.lane_question_routing(q.id.clone()).unwrap().unwrap();
    let notice = h
        .lane_inbox(lane.thread_id.clone(), String::new())
        .unwrap()
        .remove(0);
    let db = storage::Database::open_read_only(&f.config.db_path).unwrap();
    let before_lane: Vec<Message> = serde_json::from_str(
        &db.run_context(&lane.root_run_id.to_string())
            .unwrap()
            .unwrap()
            .messages_json,
    )
    .unwrap();
    let before_child: Vec<Message> = serde_json::from_str(
        &db.run_context(&child.to_string())
            .unwrap()
            .unwrap()
            .messages_json,
    )
    .unwrap();
    let sent = f.model.requests.lock().unwrap().len();
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    h.apply_lane_management_transition(
        LaneManagementRequest {
            operation_id: "detach".into(),
            lane_id: lane.lane_id.clone(),
            expected_management_revision: 1,
            action: LaneManagementAction::Detach,
        },
        LaneManagementActor::User,
        None,
        50,
    )
    .unwrap();
    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
    // Seed the exact confirmed old-lead family, then exercise real bounded reclaim.
    // The lane/child are deliberately not tombstoned: detach made them independent.
    let c = rusqlite::Connection::open(&f.config.db_path).unwrap();
    c.execute(
        "INSERT INTO thread_tombstones VALUES ('delivery-lead','old-lead',50)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO run_tombstones VALUES (?1,'delivery-lead','old-lead')",
        [lead.to_string()],
    )
    .unwrap();
    c.execute("INSERT INTO reclaim_jobs(family_key,state,created_at_ns,updated_at_ns) VALUES ('old-lead','db_pending',50,50)", []).unwrap();
    loop {
        let p = h
            .reclaim_step(storage::thread_retention::ReclaimBudget::default())
            .unwrap();
        assert_eq!(p.last_error, None);
        if p.jobs_db_pending == 0 {
            break;
        }
    }
    assert!(db.run_context(&lead.to_string()).unwrap().is_none());
    assert_eq!(
        h.lane_question_routing(q.id.clone()).unwrap().unwrap(),
        route
    );
    let retained = h.lane_inbox(lane.thread_id.clone(), String::new()).unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].notification_id, notice.notification_id);
    assert_eq!(retained[0].source_thread_id, "delivery-lead");
    assert_eq!(
        f.runtime
            .user_question(&q.id)
            .unwrap()
            .unwrap()
            .answer_origin,
        answered.answer_origin
    );
    assert_eq!(
        drive(&f, &lane.thread_id).await,
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::Stopped)
    );
    f.runtime.resume_chat(lane.root_run_id, authority).unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    assert_eq!(
        drive(&f, &lane.thread_id).await,
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.runtime.wait(child).await.unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let after_child: Vec<Message> = serde_json::from_str(
        &db.run_context(&child.to_string())
            .unwrap()
            .unwrap()
            .messages_json,
    )
    .unwrap();
    assert!(
        after_child.starts_with(&before_child),
        "already-sent child conversation was rewritten"
    );
    assert!(
        serde_json::to_string(&after_child[before_child.len()..])
            .unwrap()
            .contains("retained-scope-answer")
    );
    {
        let requests = f.model.requests.lock().unwrap();
        assert!(
            requests[sent..]
                .iter()
                .any(|messages| messages.starts_with(&before_lane)),
            "next lane wire request must preserve the sent prefix"
        );
    }
    let receipts = h
        .host_input_receipts(&[format!("lane-notification:{}", notice.notification_id)])
        .unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].status, storage::HostInputStatus::Delivered);
    assert_eq!(
        c.query_row(
            "SELECT state FROM lane_notifications WHERE notification_id=?1",
            [&notice.notification_id],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "acked"
    );
    assert_eq!(drive(&f, &lane.thread_id).await, LaneDeliveryOutcome::Empty);
}
