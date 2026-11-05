//! Contract tests use injected integer clocks and completed single-writer calls.
//! SQL connections below are fixture/fault injection only, never parallel writers.
use event_bus::{AnswerOrigin, UserQuestion};
use rusqlite::Connection;
use storage::{
    Database, RunContextRecord, Storage, StorageConfig, StorageError, StorageHandle,
    lane_delivery::*, lanes::*, thread_index::*,
};
#[path = "lane_delivery/atomic_guard.rs"]
mod atomic_guard;
#[path = "lane_delivery/control.rs"]
mod control;
#[path = "lane_delivery/recovery.rs"]
mod recovery;
#[path = "lane_delivery/rollback.rs"]
mod rollback;
#[path = "lane_delivery/waking_batch.rs"]
mod waking_batch;

struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("lane.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
        }
    }
    fn reopen(self) -> Self {
        let Self {
            _dir,
            config,
            storage,
        } = self;
        storage.close();
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir,
            config,
            storage,
        }
    }
    fn h(&self) -> StorageHandle {
        self.storage.handle()
    }
    fn db(&self) -> Database {
        Database::open(&self.config).unwrap()
    }
    fn sql(&self) -> Connection {
        let conn = Connection::open(&self.config.db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn
    }
    fn lane(&self, n: u32, policy: LanePolicy) {
        self.h()
            .create_lane(NewLane {
                lane_id: format!("lane-{n}"),
                thread_id: format!("thread-{n}"),
                origin_thread_id: "lead".into(),
                origin_run_id: "lead-root".into(),
                creation_key: format!("open-{n}"),
                brief: LaneBrief {
                    verification_requirements: Default::default(),
                    objective: "implement".into(),
                    inputs: vec![],
                    deliverables: vec![],
                    acceptance_criteria: vec!["verified".into()],
                    constraints: vec![],
                    workspace: Default::default(),
                    report_policy: policy,
                },
                start_seed: "seed".into(),
                project_id: None,
                project_root: None,
                title: None,
                at_ns: 1,
            })
            .unwrap();
    }
    fn create(&self, n: u32, q: u32, binding: LaneDeliveryBinding) -> LaneQuestionCreated {
        let question = question(n, q);
        self.h()
            .create_lane_question(
                format!("lane-{n}"),
                question.clone(),
                QuestionRequester {
                    run_id: question.run_id,
                    root_run_id: question.root_run_id,
                    thread_id: format!("thread-{n}"),
                },
                binding,
                2,
            )
            .unwrap()
    }
    fn count(&self, table: &str) -> u64 {
        self.sql()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
}
fn question(n: u32, q: u32) -> UserQuestion {
    UserQuestion {
        id: format!("q-{n:03}-{q:03}"),
        run_id: format!("run-{n}"),
        root_run_id: format!("run-{n}"),
        root_name: format!("chat:Orchestrator:thread-{n}"),
        recipient_run_ids: vec![],
        title: "choose scope".into(),
        options: vec![],
        blocking: true,
        answer: None,
        answer_origin: None,
        responder: None,
        routing_revision: 0,
        management_revision: None,
    }
}
fn report(status: LaneStatus) -> LaneReportInput {
    LaneReportInput {
        status,
        summary: "result".into(),
        artifact_refs: vec![],
        needs_human: false,
        question_refs: vec![],
        reason_code: None,
        producer: LaneReportProducer::LaneAgent,
        artifact_revision: Some(ArtifactRevision::CommitSha("a".repeat(40))),
        failure_episode: None,
    }
}
fn owner() -> LaneDeliveryOwner {
    LaneDeliveryOwner {
        root_run_id: "lead-root".into(),
        generation: 1,
        ownership_generation: 7,
    }
}
fn request(now: i64) -> LaneClaimRequest {
    LaneClaimRequest {
        destination_thread_id: "lead".into(),
        expected_destination_epoch: 0,
        owner: owner(),
        token: format!("claim-{now}"),
        now_ns: now,
        lease_until_ns: now + 100,
        allow_non_waking: true,
    }
}
fn claim(h: &StorageHandle, now: i64) -> LaneDeliveryClaim {
    match h.claim_lane_deliveries(request(now)).unwrap() {
        LaneClaimResult::Claimed(value) => value,
        other => panic!("expected claim: {other:?}"),
    }
}
fn snapshot() -> SnapshotWrite {
    SnapshotWrite {
        record: RunContextRecord {
            run_id: "lead-root".into(),
            role: "Orchestrator".into(),
            name: "chat:Orchestrator:lead".into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json:
                r#"[{"role":"user","content":[{"type":"text","text":"notification reference"}]}]"#
                    .into(),
            checkpoints_json: "[]".into(),
            terminal_phase: "Checkpoint".into(),
            restorable: true,
            updated_at_ns: 10,
            host_inputs_json: "[]".into(),
        },
        expected_snapshot_revision: None,
        binding: None,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
        map: ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: 1,
            checkpoint_count: 0,
            messages: vec![MessageProvenance {
                raw_index: 0,
                ordinal: 0,
                blocks: vec![BlockProvenance {
                    block_index: 0,
                    origin: BlockOrigin::Native,
                    excluded_reason: None,
                }],
            }],
        },
    }
}
fn user_receipt() -> storage::HostInputReceipt {
    storage::HostInputReceipt {
        prompt_id: "input-1".into(),
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        text: "private user instructions".into(),
        image_count: 0,
        status: storage::HostInputStatus::Accepted,
        reason: None,
        accepted_at_ns: 3,
        status_at_ns: 3,
        evidence_id: None,
        provenance: storage::HostInputProvenance::User,
    }
}

fn mutation(q: u32, action: LaneQuestionAction) -> LaneQuestionMutation {
    LaneQuestionMutation {
        lane_id: "lane-1".into(),
        question_id: question(1, q).id,
        lead_thread_id: "lead".into(),
        expected_management_revision: 1,
        expected_routing_revision: 1,
        operation_id: format!("op-{q}"),
        action,
        at_ns: 3,
    }
}
#[test]
fn unbound_and_legacy_questions_stay_user_routed_without_retroactive_reports() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    let old = question(1, 0);
    f.h().create_user_question(&old).unwrap();
    f.h()
        .append_lane_report("lane-1", report(LaneStatus::Ready), 1)
        .unwrap();
    assert_eq!(f.h().lane_question_routing(old.id.clone()).unwrap(), None);
    assert!(
        f.db()
            .user_question(&old.id)
            .unwrap()
            .unwrap()
            .is_user_visible()
    );
    let LaneQuestionCreated::Created(q) = f.create(1, 1, LaneDeliveryBinding::Unbound) else {
        panic!()
    };
    assert!(q.is_user_visible());
    assert_eq!(q.responder, Some(QuestionResponder::User));
    assert_eq!(f.count("lane_notifications"), 0);
}
#[test]
fn question_route_and_outbox_roll_back_together() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.sql().execute_batch("CREATE TRIGGER fail_notice BEFORE INSERT ON lane_notifications BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let q = question(1, 0);
    let error = f
        .h()
        .create_lane_question(
            "lane-1".into(),
            q.clone(),
            QuestionRequester {
                run_id: q.run_id,
                root_run_id: q.root_run_id,
                thread_id: "thread-1".into(),
            },
            LaneDeliveryBinding::Bound,
            2,
        )
        .unwrap_err();
    assert!(matches!(error, StorageError::Sqlite(_)));
    for table in [
        "user_questions",
        "lane_question_routes",
        "lane_notifications",
        "lane_delivery_keys",
    ] {
        assert_eq!(f.count(table), 0, "{table}");
    }
}
#[test]
fn report_semantics_deduplicate_summary_and_sequence_but_not_revision_or_episode() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    let first = f
        .h()
        .append_lane_report_with_delivery("lane-1".into(), report(LaneStatus::Ready), 2)
        .unwrap();
    let mut second = report(LaneStatus::Ready);
    second.summary = "different words".into();
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), second.clone(), 3)
        .unwrap();
    assert_eq!(f.count("lane_notifications"), 1);
    assert_eq!(first.report.artifact_revision, second.artifact_revision);
    second.artifact_revision = Some(ArtifactRevision::CommitSha("b".repeat(40)));
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), second, 4)
        .unwrap();
    let mut failed = report(LaneStatus::Failed);
    assert!(
        f.h()
            .append_lane_report_with_delivery("lane-1".into(), failed.clone(), 5)
            .is_err()
    );
    assert_eq!(f.count("lane_reports"), 3);
    failed.failure_episode = Some("episode-1".into());
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), failed.clone(), 6)
        .unwrap();
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), failed.clone(), 7)
        .unwrap();
    failed.failure_episode = Some("episode-2".into());
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), failed, 8)
        .unwrap();
    assert_eq!(f.count("lane_notifications"), 4);
}
#[test]
fn route_cas_invalidates_old_notice_and_republishes_projection_without_new_id() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    let claim = claim(&f.h(), 3);
    let request = mutation(0, LaneQuestionAction::RouteToUser);
    let q = f.h().mutate_lane_question(request.clone()).unwrap();
    assert_eq!(q.id, question(1, 0).id);
    assert_eq!(q.routing_revision, 2);
    assert!(q.is_user_visible());
    assert_eq!(f.h().mutate_lane_question(request).unwrap(), q);
    let late = LaneQuestionMutation {
        operation_id: "late-op".into(),
        ..mutation(
            0,
            LaneQuestionAction::Answer {
                text: "late".into(),
                origin: AnswerOrigin::LaneLead {
                    run_id: "lead-root".into(),
                    thread_id: "lead".into(),
                },
            },
        )
    };
    assert!(matches!(
        f.h().mutate_lane_question(late),
        Err(StorageError::StaleRevision {
            kind: "routing",
            ..
        })
    ));
    assert!(
        f.h()
            .accept_lane_delivery(claim.clone(), snapshot(), 4)
            .is_err()
    );
    f.h()
        .nack_lane_delivery(claim, LaneDeliveryNack::OwnershipLost, 4)
        .unwrap();
    assert_eq!(f.count("user_question_links"), 0);
}
#[test]
fn snapshot_receipt_rollback_and_crash_ack_repair_are_idempotent() {
    let mut f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    let old = claim(&f.h(), 10);
    assert!(matches!(
        f.h().ack_lane_delivery(old.clone(), 11),
        Err(StorageError::LaneDeliveryConflict("receipt_required"))
    ));
    f.sql().execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON host_input_receipts BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        f.h()
            .accept_lane_delivery(old.clone(), snapshot(), 12)
            .is_err()
    );
    assert_eq!(f.count("run_contexts"), 0);
    f.sql().execute_batch("DROP TRIGGER fail_receipt;").unwrap();
    assert!(
        f.h()
            .accept_lane_delivery(old.clone(), snapshot(), 13)
            .unwrap()
            .is_some()
    );
    assert!(
        f.h()
            .accept_lane_delivery(old.clone(), snapshot(), 14)
            .unwrap()
            .is_none()
    );
    let receipts = f
        .db()
        .host_input_receipts(&[format!(
            "lane-notification:{}",
            old.notifications[0].notification_id
        )])
        .unwrap();
    assert!(matches!(
        receipts[0].provenance,
        storage::HostInputProvenance::LaneNotification { .. }
    ));
    assert_eq!(receipts[0].evidence_id, None);
    // Restore/compaction may no longer retain a notification in host_inputs_json.
    // Its non-user receipt still prevents re-append and must not become Unknown.
    let mut compacted = snapshot().record;
    compacted.messages_json = "[]".into();
    f.h()
        .upsert_run_context_with_receipts(
            &compacted,
            &storage::HostInputWrite {
                reconcile_reason: Some("restore".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        f.h()
            .accept_lane_delivery(old.clone(), snapshot(), 15)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.db()
            .run_context("lead-root")
            .unwrap()
            .unwrap()
            .messages_json,
        "[]"
    );
    // An expired claimant is repaired from receipts without re-waking/re-appending.
    f = f.reopen();
    assert!(matches!(
        f.h().claim_lane_deliveries(request(110)).unwrap(),
        LaneClaimResult::Empty
    ));
    f.h().ack_lane_delivery(old.clone(), 112).unwrap();
    f.h().ack_lane_delivery(old, 113).unwrap();
    assert_eq!(f.count("host_input_receipts"), 1);
    assert_eq!(
        f.db()
            .run_context("lead-root")
            .unwrap()
            .unwrap()
            .messages_json,
        "[]"
    );
    assert!(
        f.h()
            .pending_lane_delivery_destinations(String::new())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn guard_survives_retries_unknown_revision_and_new_question_ids() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    let mut state = LaneWakeGuard::default();
    for n in 0..20 {
        state = f
            .h()
            .update_lane_wake_guard(
                "lane-1".into(),
                state.revision,
                LaneGuardSignal::WakeBatch {
                    batch_id: format!("wake-{n}"),
                },
            )
            .unwrap();
    }
    assert!(state.suppressed);
    assert_eq!(state.no_progress_batches, 20);
    state = f
        .h()
        .update_lane_wake_guard(
            "lane-1".into(),
            state.revision,
            LaneGuardSignal::WakeBatch {
                batch_id: "wake-19".into(),
            },
        )
        .unwrap();
    assert_eq!(state.no_progress_batches, 20);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    state = f
        .h()
        .update_lane_wake_guard(
            "lane-1".into(),
            state.revision,
            LaneGuardSignal::TerminalArtifact {
                revision: None,
                failure_episode: Some("ep".into()),
            },
        )
        .unwrap();
    assert!(state.suppressed);
    assert!(matches!(
        f.h().claim_lane_deliveries(request(5)).unwrap(),
        LaneClaimResult::Empty
    ));
    state = f
        .h()
        .update_lane_wake_guard(
            "lane-1".into(),
            state.revision,
            LaneGuardSignal::TerminalArtifact {
                revision: Some(ArtifactRevision::CommitSha("a".repeat(40))),
                failure_episode: Some("ep".into()),
            },
        )
        .unwrap();
    assert!(!state.suppressed);
    assert!(matches!(
        f.h()
            .update_lane_wake_guard("lane-1".into(), 0, LaneGuardSignal::RealUserInput),
        Err(StorageError::StaleRevision {
            kind: "wake_guard",
            ..
        })
    ));
}

#[test]
fn tombstoned_destinations_are_invisible_and_cannot_be_claimed_or_recreated() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    let old = claim(&f.h(), 10);
    f.sql().execute("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES ('lead','family',11)",[]).unwrap();
    assert!(matches!(
        f.h().claim_lane_deliveries(request(111)),
        Err(StorageError::Tombstoned { .. })
    ));
    assert!(f.h().accept_lane_delivery(old, snapshot(), 12).is_err());
    assert!(
        f.h()
            .pending_lane_delivery_destinations(String::new())
            .unwrap()
            .is_empty()
    );
    // Database-level fences also stop writes outside the facade during recovery.
    assert!(
        f.sql()
            .execute(
                "INSERT INTO lane_delivery_destinations(thread_id) VALUES ('lead')",
                []
            )
            .is_err()
    );
}
#[test]
fn management_changes_expire_question_cursor_and_delivery_authority() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    for n in 0..21 {
        f.create(1, n, LaneDeliveryBinding::Bound);
    }
    let page = f.h().lane_questions("lead".into(), None).unwrap();
    assert!(page.has_more);
    f.h()
        .change_lane_management(
            "lane-1",
            1,
            LaneManagementChange {
                lead_thread_id: Some("new-lead".into()),
                policy: LanePolicy::Attached,
            },
            5,
        )
        .unwrap();
    assert!(matches!(
        f.h().lane_questions("lead".into(), page.next_cursor),
        Err(StorageError::LaneDeliveryConflict("stale_cursor"))
    ));
    assert!(matches!(
        f.h()
            .mutate_lane_question(mutation(0, LaneQuestionAction::RouteToUser)),
        Err(StorageError::StaleRevision {
            kind: "management",
            ..
        })
    ));
    assert!(
        f.h()
            .pending_lane_delivery_destinations(String::new())
            .unwrap()
            .is_empty()
    );
}
#[test]
fn intervention_uses_actual_input_receipt_but_outbox_never_copies_its_text() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Summary);
    f.h().start_lane_run("lane-1", 0, "run-1", 2).unwrap();
    let receipt = user_receipt();
    f.h()
        .accept_lane_user_input("lane-1".into(), receipt.clone())
        .unwrap();
    f.h()
        .accept_lane_user_input("lane-1".into(), receipt.clone())
        .unwrap();
    assert_eq!(f.count("host_input_receipts"), 1);
    assert_eq!(f.count("lane_notifications"), 1);
    let notices = f.h().lane_inbox("lead".into(), String::new()).unwrap();
    assert!(
        !serde_json::to_string(&notices)
            .unwrap()
            .contains(&receipt.text)
    );
    let mut req = request(5);
    req.allow_non_waking = false;
    assert!(matches!(
        f.h().claim_lane_deliveries(req).unwrap(),
        LaneClaimResult::Empty
    ));
    f.lane(2, LanePolicy::Detached);
    f.h().start_lane_run("lane-2", 0, "run-2", 2).unwrap();
    let mut detached = receipt;
    detached.prompt_id = "input-2".into();
    detached.run_id = "run-2".into();
    detached.root_run_id = "run-2".into();
    f.h()
        .accept_lane_user_input("lane-2".into(), detached)
        .unwrap();
    assert_eq!(f.count("lane_notifications"), 1);
}
#[test]
fn v46_upgrade_does_not_route_existing_questions_or_backfill_terminal_reports() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.h().create_user_question(&question(1, 0)).unwrap();
    f.h()
        .append_lane_report("lane-1", report(LaneStatus::Ready), 2)
        .unwrap();
    let Fixture {
        _dir,
        config,
        storage,
    } = f;
    drop(storage);
    let conn = Connection::open(&config.db_path).unwrap();
    conn.execute_batch(concat!(
        include_str!("../undo/undo_v48.sql"),
        include_str!("../undo/undo_v47.sql")
    ))
    .unwrap();
    conn.pragma_update(None, "user_version", 46).unwrap();
    drop(conn);
    let storage = Storage::open(config.clone()).unwrap();
    let h = storage.handle();
    assert_eq!(h.lane_question_routing(question(1, 0).id).unwrap(), None);
    assert!(
        h.lane_inbox("lead".into(), String::new())
            .unwrap()
            .is_empty()
    );
    let database = Database::open(&config).unwrap();
    assert!(
        database
            .user_question(&question(1, 0).id)
            .unwrap()
            .unwrap()
            .is_user_visible()
    );
}

#[test]
fn owner_rebind_does_not_clear_stop_but_explicit_resume_does() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    let epoch = f
        .h()
        .bind_lane_delivery_destination("lead".into(), 0, owner(), LaneDestinationAction::Stop)
        .unwrap();
    let mut next = owner();
    next.ownership_generation += 1;
    let epoch = f
        .h()
        .bind_lane_delivery_destination(
            "lead".into(),
            epoch,
            next.clone(),
            LaneDestinationAction::Bind,
        )
        .unwrap();
    let mut req = request(10);
    req.owner = next.clone();
    req.expected_destination_epoch = epoch;
    assert!(matches!(
        f.h().claim_lane_deliveries(req.clone()).unwrap(),
        LaneClaimResult::Stopped
    ));
    let epoch = f
        .h()
        .bind_lane_delivery_destination(
            "lead".into(),
            epoch,
            next,
            LaneDestinationAction::ExplicitUserResume,
        )
        .unwrap();
    req.expected_destination_epoch = epoch;
    assert!(matches!(
        f.h().claim_lane_deliveries(req).unwrap(),
        LaneClaimResult::Claimed(_)
    ));
}
#[test]
fn milestones_coalesce_only_before_claim_and_keep_claimed_references_immutable() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    for at in 2..5 {
        f.h()
            .append_lane_report_with_delivery("lane-1".into(), report(LaneStatus::Working), at)
            .unwrap();
    }
    assert_eq!(f.count("lane_notifications"), 1);
    let first = claim(&f.h(), 10);
    assert_eq!(first.notifications[0].report_seq, Some(3));
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), report(LaneStatus::Working), 11)
        .unwrap();
    assert_eq!(f.count("lane_notifications"), 2);
    f.h()
        .accept_lane_delivery(first.clone(), snapshot(), 12)
        .unwrap();
    f.h().ack_lane_delivery(first, 13).unwrap();
    let second = claim(&f.h(), 14);
    assert_eq!(second.notifications[0].report_seq, Some(4));
}

#[test]
fn lead_answer_is_atomic_and_delivered_to_lane_by_the_same_outbox() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    let req = mutation(
        0,
        LaneQuestionAction::Answer {
            text: "proceed".into(),
            origin: AnswerOrigin::LaneLead {
                run_id: "lead-root".into(),
                thread_id: "lead".into(),
            },
        },
    );
    let q = f.h().mutate_lane_question(req.clone()).unwrap();
    assert_eq!(q.run_id, "run-1");
    assert_eq!(q.answer.as_deref(), Some("proceed"));
    assert_eq!(f.h().mutate_lane_question(req).unwrap(), q);
    assert_eq!(f.count("review_evidence"), 0);
    let notices = f.h().lane_inbox("thread-1".into(), String::new()).unwrap();
    assert_eq!(notices.len(), 1);
    assert!(matches!(
        notices[0].kind,
        LaneNotificationKind::Answer { .. }
    ));
    let mut req = request(5);
    req.destination_thread_id = "thread-1".into();
    req.owner.root_run_id = "run-1".into();
    assert!(matches!(
        f.h().claim_lane_deliveries(req).unwrap(),
        LaneClaimResult::Claimed(_)
    ));
}
#[test]
fn lead_inbox_exceeds_32_without_inheritance_and_pages_all_40() {
    let f = Fixture::new();
    for n in 1..=10 {
        f.lane(n, LanePolicy::Attached);
        for q in 0..4 {
            f.create(n, q, LaneDeliveryBinding::Bound);
        }
    }
    for n in 0..32 {
        let mut q = question(100, n);
        q.run_id = "lead-root".into();
        q.root_run_id = "lead-root".into();
        f.h().create_user_question(&q).unwrap();
    }
    let mut ids = std::collections::HashSet::new();
    let mut cursor = None;
    loop {
        let page = f.h().lane_questions("lead".into(), cursor).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= LANE_DELIVERY_PAGE_BYTES);
        for q in &page.questions {
            assert!(ids.insert(q.id.clone()));
        }
        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
    }
    assert_eq!(ids.len(), 40);
    assert_eq!(f.count("user_question_links"), 0);
}
#[test]
fn capacity_boundaries_are_typed_and_diagnostic_is_bounded() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    for q in 0..32 {
        f.create(1, q, LaneDeliveryBinding::Bound);
    }
    assert!(matches!(
        f.h().create_user_question(&question(1, 33)),
        Err(StorageError::QuestionCapacity(QuestionCapacity::Requester))
    ));
    for _ in 0..4 {
        assert!(matches!(
            f.create(1, 33, LaneDeliveryBinding::Bound),
            LaneQuestionCreated::CapacityBlocked(_)
        ));
    }
    assert_eq!(f.count("user_questions"), 32);
    assert_eq!(
        f.h()
            .lane_question_capacity_diagnostic("lane-1".into())
            .unwrap()
            .unwrap()
            .attempts,
        4
    );
    for n in 2..=32 {
        for q in 0..32 {
            f.h().create_user_question(&question(n, q)).unwrap();
        }
    }
    assert_eq!(f.count("user_questions"), 1024);
    assert!(matches!(
        f.h().create_user_question(&question(33, 0)),
        Err(StorageError::QuestionCapacity(QuestionCapacity::Global))
    ));
    f.lane(33, LanePolicy::Attached);
    assert!(matches!(
        f.create(33, 0, LaneDeliveryBinding::Bound),
        LaneQuestionCreated::CapacityBlocked(QuestionCapacityBlocked {
            capacity: QuestionCapacity::Global,
            ..
        })
    ));
    assert_eq!(f.count("user_questions"), 1024);
}
#[test]
fn lease_expiry_nack_and_owner_epochs_fence_old_claimants() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    let old = claim(&f.h(), 10);
    assert!(matches!(
        f.h().claim_lane_deliveries(request(11)).unwrap(),
        LaneClaimResult::Busy
    ));
    let new = claim(&f.h(), 110);
    assert!(new.claim_epoch > old.claim_epoch);
    assert!(
        f.h()
            .nack_lane_delivery(old, LaneDeliveryNack::Readonly, 111)
            .is_err()
    );
    f.h()
        .nack_lane_delivery(new, LaneDeliveryNack::Readonly, 111)
        .unwrap();
    let newer = claim(&f.h(), 112);
    let epoch = f
        .h()
        .bind_lane_delivery_destination("lead".into(), 0, owner(), LaneDestinationAction::Stop)
        .unwrap();
    assert!(f.h().accept_lane_delivery(newer, snapshot(), 113).is_err());
    let mut req = request(114);
    req.expected_destination_epoch = epoch;
    assert!(matches!(
        f.h().claim_lane_deliveries(req.clone()).unwrap(),
        LaneClaimResult::Stopped
    ));
    f.h()
        .update_lane_wake_guard("lane-1".into(), 0, LaneGuardSignal::RealUserInput)
        .unwrap();
    assert!(matches!(
        f.h().claim_lane_deliveries(req).unwrap(),
        LaneClaimResult::Stopped
    ));
}

#[test]
fn tombstoned_source_lane_is_hidden_from_a_live_lead_question_page() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    assert_eq!(
        f.h()
            .lane_questions("lead".into(), None)
            .unwrap()
            .questions
            .len(),
        1
    );
    f.sql().execute("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES ('thread-1','family',11)",[]).unwrap();
    let page = f.h().lane_questions("lead".into(), None).unwrap();
    assert!(page.questions.is_empty());
    assert!(!page.has_more);
    assert!(
        f.h()
            .lane_inbox("lead".into(), String::new())
            .unwrap()
            .is_empty()
    );
}
#[test]
fn an_escaped_question_over_page_budget_rolls_back_before_acceptance() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    let mut q = question(1, 0);
    q.title = "x".to_owned() + &"\u{0001}".repeat(2047);
    q.root_name = "x".to_owned() + &"\u{0001}".repeat(1023);
    q.options = vec!["x".to_owned() + &"\u{0001}".repeat(255); 3];
    let error = f
        .h()
        .create_lane_question(
            "lane-1".into(),
            q.clone(),
            QuestionRequester {
                run_id: q.run_id.clone(),
                root_run_id: q.root_run_id.clone(),
                thread_id: "thread-1".into(),
            },
            LaneDeliveryBinding::Bound,
            2,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        StorageError::LaneDeliveryConflict("question_exceeds_page_bytes")
    ));
    // Reject at the atomic source boundary rather than accept a question that
    // can strand the listing. No hidden blocking question or notice survives.
    assert!(f.db().user_question(&q.id).unwrap().is_none());
    assert!(f.h().lane_question_routing(q.id).unwrap().is_none());
    assert_eq!(f.count("lane_question_routes"), 0);
    assert_eq!(f.count("user_question_links"), 0);
    assert_eq!(f.count("lane_notifications"), 0);
    assert!(
        f.h()
            .lane_questions("lead".into(), None)
            .unwrap()
            .questions
            .is_empty()
    );
    f.create(1, 1, LaneDeliveryBinding::Bound);
    assert_eq!(
        f.h()
            .lane_questions("lead".into(), None)
            .unwrap()
            .questions
            .len(),
        1
    );
}

#[test]
fn missing_canonical_route_cannot_hide_a_legacy_root_behind_a_stale_lead_projection() {
    let f = Fixture::new();
    let mut q = question(1, 0);
    q.responder = Some(QuestionResponder::Lead {
        thread_id: "lead".into(),
    });
    q.routing_revision = 12;
    q.management_revision = Some(7);
    f.h().create_user_question(&q).unwrap();
    let restored = f.db().user_question(&q.id).unwrap().unwrap();
    assert!(restored.is_user_visible());
    assert_eq!(restored.responder, None);
    assert_eq!(restored.routing_revision, 0);
    assert_eq!(restored.management_revision, None);
}
