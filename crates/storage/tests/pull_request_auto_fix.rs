//! IO contracts use caller-injected clocks and explicit Barrier synchronization.
use rusqlite::Connection;
use storage::{
    HostInputProvenance, HostInputReceipt, HostInputStatus, RunContextRecord, Storage,
    StorageConfig, StorageHandle, lane_delivery::*, pull_request_auto_fix::*, pull_requests::*,
    thread_index::*,
};
struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
}
impl Fixture {
    fn new() -> Self {
        let d = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: d.path().join("pr.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let f = Self {
            _dir: d,
            config,
            storage,
        };
        f.link("lead");
        f
    }
    fn h(&self) -> StorageHandle {
        self.storage.handle()
    }
    fn sql(&self) -> Connection {
        let c = Connection::open(&self.config.db_path).unwrap();
        c.pragma_update(None, "foreign_keys", true).unwrap();
        c
    }
    fn count(&self, table: &str) -> u64 {
        self.sql()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    fn link(&self, thread: &str) {
        self.sql().execute("INSERT INTO threads(thread_id,archived,created_at_ns,updated_at_ns,source) VALUES (?1,0,1,1,'runtime')",[thread]).unwrap();
        self.h()
            .link_pull_request(LinkPullRequest {
                thread_id: thread.into(),
                key: key(),
                expected_revision: 0,
                at_ns: 1,
            })
            .unwrap();
    }
    fn policy(&self, action: PrPolicyAction) -> PrAutoFixState {
        let s = self.h().pr_auto_fix_state(key()).unwrap();
        self.h()
            .update_pr_auto_fix_policy(PrPolicyUpdate {
                key: key(),
                expected_policy_revision: s.policy_revision,
                expected_destination_thread_id: s.destination_thread_id,
                destination_thread_id: "lead".into(),
                link_revision: 1,
                action,
            })
            .unwrap()
    }
    fn reserve(&self, n: u32, mode: PrNoticeMode, now: i64) -> PrNotice {
        self.h()
            .reserve_pr_notice(self.reservation(n, mode, now))
            .unwrap()
    }
    fn reservation(&self, n: u32, mode: PrNoticeMode, now: i64) -> PrNoticeReservation {
        PrNoticeReservation {
            key: key(),
            head_sha: format!("{n:040x}"),
            mode,
            destination_thread_id: "lead".into(),
            link_revision: 1,
            expected_policy_revision: self.h().pr_auto_fix_state(key()).unwrap().policy_revision,
            success_seq: 1,
            auto_fix_enabled: true,
            now_ns: now,
        }
    }
    fn ready(&self, n: u32, mode: PrNoticeMode, now: i64) -> PrNotice {
        let n = self.reserve(n, mode, now);
        self.h()
            .ready_pr_notice(n.notice_id, "processed failure facts".into())
            .unwrap()
    }
    fn claim(&self, now: i64) -> PrDeliveryClaim {
        let PrClaimResult::Claimed(c) = self.h().claim_pr_delivery(request(now)).unwrap() else {
            panic!("expected PR claim")
        };
        c
    }
    fn accept(&self, c: &PrDeliveryClaim, now: i64) {
        self.h()
            .accept_pr_delivery(c.clone(), snapshot(), now, true)
            .unwrap();
    }
}
fn key() -> PullRequestKey {
    PullRequestKey::github("Owner/Repo", 42).unwrap()
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
        allow_non_waking: false,
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

#[test]
fn defaults_policy_cas_and_lifecycle_are_separate() {
    let f = Fixture::new();
    let s = f.h().pr_auto_fix_state(key()).unwrap();
    assert!(!s.enabled);
    assert_eq!(s.used + s.reserved, 0);
    f.policy(PrPolicyAction::SetEnabled(true));
    f.link("other");
    let s = f.h().pr_auto_fix_state(key()).unwrap();
    let mut change = PrPolicyUpdate {
        key: key(),
        expected_policy_revision: s.policy_revision,
        expected_destination_thread_id: None,
        destination_thread_id: "other".into(),
        link_revision: 1,
        action: PrPolicyAction::SetEnabled(true),
    };
    assert!(f.h().update_pr_auto_fix_policy(change.clone()).is_err());
    change.expected_destination_thread_id = s.destination_thread_id;
    let next = f.h().update_pr_auto_fix_policy(change.clone()).unwrap();
    assert_eq!(next.destination_thread_id.as_deref(), Some("other"));
    assert!(f.h().update_pr_auto_fix_policy(change).is_err());
    let rev: u64 = f
        .sql()
        .query_row(
            "SELECT lifecycle_revision FROM thread_pull_request_state WHERE thread_id='lead'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rev, 1);
}
#[test]
fn two_connections_reserve_concurrently_with_one_sha_and_bounded_budget() {
    let f = Fixture::new();
    f.policy(PrPolicyAction::SetEnabled(true));
    let other = Storage::open(f.config.clone()).unwrap();
    let a = f.h();
    let b = other.handle();
    let barrier = std::sync::Barrier::new(2);
    let r = f.reservation(1, PrNoticeMode::Auto, 10);
    let ids = std::thread::scope(|s| {
        let one = s.spawn(|| {
            barrier.wait();
            a.reserve_pr_notice(r.clone()).unwrap().notice_id
        });
        let two = s.spawn(|| {
            barrier.wait();
            b.reserve_pr_notice(r.clone()).unwrap().notice_id
        });
        (one.join().unwrap(), two.join().unwrap())
    });
    assert_eq!(ids.0, ids.1);
    assert_eq!(f.count("pr_notices"), 1);
    let r2 = f.reservation(2, PrNoticeMode::Auto, 10);
    let r3 = f.reservation(3, PrNoticeMode::Auto, 10);
    std::thread::scope(|s| {
        let one = s.spawn(|| {
            barrier.wait();
            a.reserve_pr_notice(r2).unwrap()
        });
        let two = s.spawn(|| {
            barrier.wait();
            b.reserve_pr_notice(r3).unwrap()
        });
        one.join().unwrap();
        two.join().unwrap();
    });
    assert!(
        f.h()
            .reserve_pr_notice(f.reservation(4, PrNoticeMode::Auto, 10))
            .is_err()
    );
    assert_eq!(f.h().pr_auto_fix_state(key()).unwrap().reserved, 3);
    f.h().cancel_pr_notice(ids.0.clone()).unwrap();
    let retry = f.reserve(1, PrNoticeMode::Manual, 10);
    assert_eq!(retry.notice_id, ids.0);
    assert_eq!(f.h().pr_auto_fix_state(key()).unwrap().reserved, 2);
    other.close();
}
#[test]
fn third_adoption_is_atomic_auto_off_and_reset_preserves_sha_and_cooldown() {
    let f = Fixture::new();
    f.policy(PrPolicyAction::SetEnabled(true));
    let mut last = String::new();
    for i in 1..=3 {
        let now = i64::from(i) * (PR_AUTO_FIX_COOLDOWN_NS + 1);
        let n = f.ready(i, PrNoticeMode::Auto, now);
        let c = f.claim(now);
        if i == 3 {
            f.sql().execute_batch("CREATE TRIGGER fail_event BEFORE INSERT ON pr_auto_fix_limit_events BEGIN SELECT RAISE(ABORT,'fault'); END;").unwrap();
            assert!(
                f.h()
                    .accept_pr_delivery(c.clone(), snapshot(), now + 1, true)
                    .is_err()
            );
            let s = f.h().pr_auto_fix_state(key()).unwrap();
            assert_eq!((s.used, s.reserved, s.enabled), (2, 1, true));
            assert_eq!(f.count("host_input_receipts"), 2);
            assert_eq!(
                f.h().pr_notice(n.notice_id.clone()).unwrap().unwrap().phase,
                PrNoticePhase::Ready
            );
            f.sql().execute_batch("DROP TRIGGER fail_event").unwrap();
        }
        f.accept(&c, now + 1);
        assert!(
            f.h()
                .accept_pr_delivery(c.clone(), snapshot(), now + 2, true)
                .unwrap()
                .is_none()
        );
        f.h().ack_pr_delivery(c.clone(), now + 2).unwrap();
        f.h().ack_pr_delivery(c, now + 3).unwrap();
        last = n.notice_id;
    }
    let before = f.h().pr_auto_fix_state(key()).unwrap();
    assert_eq!(
        (before.used, before.reserved, before.enabled),
        (3, 0, false)
    );
    assert_eq!(before.stop_reason.as_deref(), Some("limit_reached"));
    assert_eq!(f.count("pr_auto_fix_limit_events"), 1);
    let events = f.h().pr_auto_fix_limit_events(key(), None).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].budget_epoch, 0);
    assert!(
        f.h()
            .pr_auto_fix_limit_events(key(), Some(0))
            .unwrap()
            .is_empty()
    );
    let reset = f.policy(PrPolicyAction::ResetBudget);
    assert_eq!(
        (reset.used, reset.budget_epoch, reset.enabled),
        (0, 1, true)
    );
    assert_eq!(reset.cooldown_until_ns, before.cooldown_until_ns);
    assert!(
        f.h()
            .reserve_pr_notice(f.reservation(3, PrNoticeMode::Manual, i64::MAX - 1000))
            .is_err()
    );
    assert!(
        f.h()
            .reserve_pr_notice(f.reservation(
                4,
                PrNoticeMode::Auto,
                before.cooldown_until_ns.unwrap() - 1
            ))
            .is_err()
    );
    assert_eq!(
        f.h().pr_notice(last).unwrap().unwrap().phase,
        PrNoticePhase::Adopted
    );
}
#[test]
fn ready_and_snapshot_failures_roll_back_with_stable_id() {
    let f = Fixture::new();
    let n = f.reserve(1, PrNoticeMode::Manual, 1);
    f.sql().execute_batch("CREATE TRIGGER fail_outbox BEFORE INSERT ON lane_notifications BEGIN SELECT RAISE(ABORT,'fault'); END;").unwrap();
    assert!(
        f.h()
            .ready_pr_notice(n.notice_id.clone(), "facts".into())
            .is_err()
    );
    assert_eq!(
        f.h().pr_notice(n.notice_id.clone()).unwrap().unwrap().phase,
        PrNoticePhase::Reserved
    );
    assert_eq!(f.count("lane_notifications"), 0);
    f.sql().execute_batch("DROP TRIGGER fail_outbox").unwrap();
    assert!(
        f.h()
            .ready_pr_notice(n.notice_id.clone(), "x".repeat(PR_NOTICE_PAYLOAD_MAX + 1))
            .is_err()
    );
    f.h()
        .ready_pr_notice(n.notice_id.clone(), "x".repeat(PR_NOTICE_PAYLOAD_MAX))
        .unwrap();
    let c = f.claim(10);
    f.sql().execute_batch("CREATE TRIGGER fail_guard BEFORE UPDATE OF wake_guard_json ON pr_auto_fix_state BEGIN SELECT RAISE(ABORT,'fault'); END;").unwrap();
    assert!(
        f.h()
            .accept_pr_delivery(c.clone(), snapshot(), 11, true)
            .is_err()
    );
    assert_eq!(f.count("run_contexts"), 0);
    assert_eq!(f.count("host_input_receipts"), 0);
    f.sql().execute_batch("DROP TRIGGER fail_guard").unwrap();
    f.accept(&c, 12);
    assert_eq!(f.h().pr_delivery_receipt_ids(c).unwrap(), vec![n.notice_id]);
    assert_eq!(f.h().pr_wake_guard(key()).unwrap().no_progress_batches, 1);
}
#[test]
fn lease_generation_stop_and_ack_recovery_are_shared() {
    let f = Fixture::new();
    f.ready(1, PrNoticeMode::Manual, 1);
    let c = f.claim(10);
    let mut forged = c.clone();
    forged.owner.generation += 1;
    assert!(
        f.h()
            .accept_pr_delivery(forged, snapshot(), 11, true)
            .is_err()
    );
    assert!(f.h().ack_pr_delivery(c.clone(), 11).is_err());
    f.h()
        .nack_pr_delivery(c.clone(), LaneDeliveryNack::Readonly, 11)
        .unwrap();
    assert!(f.h().accept_pr_delivery(c, snapshot(), 12, true).is_err());
    let c = f.claim(12);
    f.accept(&c, 13);
    // Expired lease is not usable, but the next claim repairs ACK from its receipt.
    assert!(f.h().ack_pr_delivery(c.clone(), 112).is_err());
    assert_eq!(
        f.h().claim_pr_delivery(request(112)).unwrap(),
        PrClaimResult::Empty
    );
    f.h().ack_pr_delivery(c, 113).unwrap();
    assert_eq!(f.count("host_input_receipts"), 1);
    f.ready(2, PrNoticeMode::Manual, 1);
    let epoch = f
        .h()
        .bind_lane_delivery_destination("lead".into(), 0, owner(), LaneDestinationAction::Stop)
        .unwrap();
    let mut req = request(200);
    req.expected_destination_epoch = epoch;
    assert_eq!(
        f.h().claim_pr_delivery(req.clone()).unwrap(),
        PrClaimResult::Stopped
    );
    f.policy(PrPolicyAction::ResetBudget);
    assert_eq!(
        f.h().claim_pr_delivery(req).unwrap(),
        PrClaimResult::Stopped
    );
}
#[test]
fn global_switch_is_checked_at_adoption_and_manual_is_independent() {
    let f = Fixture::new();
    f.policy(PrPolicyAction::SetEnabled(true));
    let mut req = f.reservation(1, PrNoticeMode::Auto, 1);
    req.auto_fix_enabled = false;
    assert!(f.h().reserve_pr_notice(req).is_err());
    f.ready(1, PrNoticeMode::Auto, 1);
    let c = f.claim(10);
    assert!(
        f.h()
            .accept_pr_delivery(c.clone(), snapshot(), 11, false)
            .is_err()
    );
    assert_eq!(f.count("host_input_receipts"), 0);
    f.h()
        .cancel_pr_notice(c.notifications[0].notification_id.clone())
        .unwrap();
    f.h()
        .nack_pr_delivery(c, LaneDeliveryNack::StorageFailed, 12)
        .unwrap();
    f.ready(1, PrNoticeMode::Manual, 1);
    let c = f.claim(13);
    f.h().accept_pr_delivery(c, snapshot(), 14, false).unwrap();
    assert_eq!(f.h().pr_auto_fix_state(key()).unwrap().used, 0);
}
#[test]
fn harness_receipts_never_have_human_evidence() {
    let f = Fixture::new();
    f.ready(1, PrNoticeMode::Manual, 1);
    f.accept(&f.claim(10), 11);
    let mut r = HostInputReceipt {
        provenance: HostInputProvenance::HarnessNotice {
            notification_id: "n".into(),
            destination_thread_id: "lead".into(),
        },
        prompt_id: "harness-notice:n".into(),
        run_id: "lead-root".into(),
        root_run_id: "lead-root".into(),
        text: String::new(),
        image_count: 0,
        status: HostInputStatus::Delivered,
        reason: None,
        accepted_at_ns: 1,
        status_at_ns: 1,
        evidence_id: None,
    };
    r.validate().unwrap();
    r.text = "human".into();
    assert!(r.validate().is_err());
    assert!(
        f.h()
            .append_host_input_accepted(&[], &[], None, &r)
            .is_err()
    );
    r.text.clear();
    r.image_count = 1;
    assert!(r.validate().is_err());
    assert!(
        f.h()
            .append_host_input_accepted(&[], &[], None, &r)
            .is_err()
    );
    r.image_count = 0;
    r.evidence_id = Some("evidence".into());
    assert!(r.validate().is_err());
    assert!(
        f.h()
            .append_host_input_accepted(&[], &[], None, &r)
            .is_err()
    );
    assert_eq!(f.count("review_evidence"), 0);
    let (text, images, evidence, provenance): (String, u32, Option<String>, String) = f
        .sql()
        .query_row(
            "SELECT text,image_count,evidence_id,provenance_json FROM host_input_receipts",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert!(text.is_empty());
    assert_eq!((images, evidence), (0, None));
    assert!(provenance.contains("harness_notice"));
}
#[test]
fn pr_guard_is_stable_across_heads_budget_and_destination_stop() {
    let f = Fixture::new();
    let mut guard = LaneWakeGuard::default();
    for n in 0..LANE_NO_PROGRESS_LIMIT {
        guard = f
            .h()
            .update_pr_wake_guard(
                key(),
                guard.revision,
                PrGuardSignal::WakeBatch {
                    batch_id: format!("batch-{n}"),
                },
            )
            .unwrap();
    }
    assert!(guard.suppressed);
    let same = f
        .h()
        .update_pr_wake_guard(
            key(),
            guard.revision,
            PrGuardSignal::WakeBatch {
                batch_id: format!("batch-{}", LANE_NO_PROGRESS_LIMIT - 1),
            },
        )
        .unwrap();
    assert_eq!(same, guard);
    f.policy(PrPolicyAction::ResetBudget);
    f.ready(2, PrNoticeMode::Manual, 1);
    assert_eq!(
        f.h().claim_pr_delivery(request(10)).unwrap(),
        PrClaimResult::Empty
    );
    assert_eq!(f.h().pr_wake_guard(key()).unwrap(), guard);
    let epoch = f
        .h()
        .bind_lane_delivery_destination("lead".into(), 0, owner(), LaneDestinationAction::Stop)
        .unwrap();
    let reset = f
        .h()
        .update_pr_wake_guard(key(), guard.revision, PrGuardSignal::RealUserInput)
        .unwrap();
    assert_eq!(reset.no_progress_batches, 0);
    let mut req = request(11);
    req.expected_destination_epoch = epoch;
    assert_eq!(
        f.h().claim_pr_delivery(req.clone()).unwrap(),
        PrClaimResult::Stopped
    );
    let epoch = f
        .h()
        .bind_lane_delivery_destination(
            "lead".into(),
            epoch,
            owner(),
            LaneDestinationAction::ExplicitUserResume,
        )
        .unwrap();
    req.expected_destination_epoch = epoch;
    assert!(matches!(
        f.h().claim_pr_delivery(req).unwrap(),
        PrClaimResult::Claimed(_)
    ));
    let reset = f
        .h()
        .update_pr_wake_guard(key(), reset.revision, PrGuardSignal::ExplicitUserResume)
        .unwrap();
    assert!(!reset.suppressed);
}
#[test]
fn unlink_archive_and_deletion_stop_without_fallback_and_preserve_dedup() {
    let f = Fixture::new();
    f.link("other");
    f.policy(PrPolicyAction::SetEnabled(true));
    let n = f.ready(1, PrNoticeMode::Auto, 1);
    f.accept(&f.claim(10), 11);
    f.sql().execute("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES ('lead','family',12)",[]).unwrap();
    let s = f.h().pr_auto_fix_state(key()).unwrap();
    assert!(!s.enabled);
    assert!(s.destination_thread_id.is_none());
    let record = f.h().pr_notice(n.notice_id).unwrap().unwrap();
    assert_eq!(record.phase, PrNoticePhase::Adopted);
    assert!(record.destination_thread_id.is_none());
    assert!(record.payload.is_none());
    let mut r = f.reservation(1, PrNoticeMode::Manual, 20);
    r.destination_thread_id = "other".into();
    assert!(f.h().reserve_pr_notice(r).is_err());
    let f = Fixture::new();
    f.policy(PrPolicyAction::SetEnabled(true));
    f.ready(1, PrNoticeMode::Auto, 1);
    f.h().unlink_pull_request("lead", 1, 2).unwrap();
    assert!(!f.h().pr_auto_fix_state(key()).unwrap().enabled);
    assert_eq!(
        f.h().pr_notices(key(), String::new()).unwrap()[0].phase,
        PrNoticePhase::Cancelled
    );
    let f = Fixture::new();
    f.policy(PrPolicyAction::SetEnabled(true));
    f.sql()
        .execute("UPDATE threads SET archived=1 WHERE thread_id='lead'", [])
        .unwrap();
    assert_eq!(
        f.h()
            .pr_auto_fix_state(key())
            .unwrap()
            .stop_reason
            .as_deref(),
        Some("archived")
    );
    f.sql()
        .execute("UPDATE threads SET archived=0 WHERE thread_id='lead'", [])
        .unwrap();
    assert!(!f.h().pr_auto_fix_state(key()).unwrap().enabled);
}

#[test]
fn first_thread_mirror_already_archived_stops_prepared_notice_without_fallback() {
    let f = Fixture::new();
    // PR linking can precede the thread header's first durable mirror.
    let linked = f
        .h()
        .link_pull_request(LinkPullRequest {
            thread_id: "late".into(),
            key: key(),
            expected_revision: 0,
            at_ns: 1,
        })
        .unwrap();
    let policy = f
        .h()
        .update_pr_auto_fix_policy(PrPolicyUpdate {
            key: key(),
            expected_policy_revision: 0,
            expected_destination_thread_id: None,
            destination_thread_id: "late".into(),
            link_revision: linked.link.link_revision,
            action: PrPolicyAction::SetEnabled(true),
        })
        .unwrap();
    let mut reservation = f.reservation(1, PrNoticeMode::Auto, 1);
    reservation.destination_thread_id = "late".into();
    reservation.link_revision = linked.link.link_revision;
    reservation.expected_policy_revision = policy.policy_revision;
    let notice = f.h().reserve_pr_notice(reservation).unwrap();
    f.h()
        .ready_pr_notice(notice.notice_id.clone(), "processed failure facts".into())
        .unwrap();
    f.sql().execute("INSERT INTO threads(thread_id,archived,created_at_ns,updated_at_ns,source) VALUES ('late',1,2,2,'runtime')",[]).unwrap();
    let stopped = f.h().pr_auto_fix_state(key()).unwrap();
    assert!(!stopped.enabled);
    assert_eq!(stopped.stop_reason.as_deref(), Some("archived"));
    assert!(stopped.destination_thread_id.is_none());
    assert_eq!(stopped.reserved, 0);
    let notice = f.h().pr_notice(notice.notice_id).unwrap().unwrap();
    assert_eq!(notice.phase, PrNoticePhase::Cancelled);
    assert!(notice.payload.is_none());
    let mut claim = request(3);
    claim.destination_thread_id = "late".into();
    assert_eq!(
        f.h().claim_pr_delivery(claim).unwrap(),
        PrClaimResult::Empty
    );
}

fn report(status: storage::lanes::LaneStatus) -> storage::lanes::LaneReportInput {
    storage::lanes::LaneReportInput {
        status,
        summary: "result".into(),
        artifact_refs: vec![],
        needs_human: false,
        question_refs: vec![],
        reason_code: None,
        producer: storage::lanes::LaneReportProducer::LaneAgent,
        artifact_revision: Some(ArtifactRevision::CommitSha("a".repeat(40))),
        failure_episode: None,
    }
}

#[test]
fn claims_are_source_homogeneous_with_one_shared_destination_lease() {
    let f = Fixture::new();
    make_lane(&f);
    f.ready(1, PrNoticeMode::Manual, 1);
    let lane_guard = f.h().lane_wake_guard("source-lane".into()).unwrap();
    let pr = f.claim(10);
    assert_eq!(pr.notifications.len(), 1);
    assert_eq!(
        f.h().claim_lane_deliveries(request(10)).unwrap(),
        LaneClaimResult::Busy
    );
    f.accept(&pr, 11);
    f.h().ack_pr_delivery(pr, 12).unwrap();
    assert_eq!(
        f.h().lane_wake_guard("source-lane".into()).unwrap(),
        lane_guard
    );
    let LaneClaimResult::Claimed(lane) = f.h().claim_lane_deliveries(request(13)).unwrap() else {
        panic!("Lane notice was lost")
    };
    assert_eq!(lane.notifications.len(), 1);
    assert_eq!(lane.notifications[0].lane_id, "source-lane");
    let kind: String = f
        .sql()
        .query_row(
            "SELECT source_kind FROM lane_notifications WHERE notification_id=?1",
            [&lane.notifications[0].notification_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "lane");
    assert_eq!(f.count("lanes"), 1);
}
#[test]
fn restart_preserves_sha_dedup_and_receipt_only_repair() {
    let f = Fixture::new();
    f.ready(1, PrNoticeMode::Manual, 1);
    let claim = f.claim(10);
    f.accept(&claim, 11);
    let Fixture {
        _dir,
        config,
        storage,
    } = f;
    storage.close();
    let f = Fixture {
        _dir,
        storage: Storage::open(config.clone()).unwrap(),
        config,
    };
    assert_eq!(
        f.h().claim_pr_delivery(request(110)).unwrap(),
        PrClaimResult::Empty
    );
    assert_eq!(f.count("host_input_receipts"), 1);
    assert_eq!(f.h().pr_wake_guard(key()).unwrap().no_progress_batches, 1);
    f.policy(PrPolicyAction::ResetBudget);
    assert!(
        f.h()
            .reserve_pr_notice(f.reservation(1, PrNoticeMode::Manual, 120))
            .is_err()
    );
}
#[test]
fn policy_change_fences_inflight_auto_reservation_without_resetting_budget() {
    let f = Fixture::new();
    f.policy(PrPolicyAction::SetEnabled(true));
    let n = f.ready(1, PrNoticeMode::Auto, 1);
    let c = f.claim(10);
    f.policy(PrPolicyAction::SetEnabled(false));
    assert_eq!(f.h().pr_auto_fix_state(key()).unwrap().reserved, 0);
    assert!(
        f.h()
            .accept_pr_delivery(c.clone(), snapshot(), 11, true)
            .is_err()
    );
    f.h()
        .nack_pr_delivery(c, LaneDeliveryNack::StorageFailed, 11)
        .unwrap();
    f.policy(PrPolicyAction::SetEnabled(true));
    let retry = f.reserve(1, PrNoticeMode::Auto, 12);
    assert_eq!(n.notice_id, retry.notice_id);
    assert_eq!(f.h().pr_auto_fix_state(key()).unwrap().budget_epoch, 0);
}

fn make_lane(f: &Fixture) {
    f.h()
        .create_lane(storage::lanes::NewLane {
            lane_id: "source-lane".into(),
            thread_id: "source".into(),
            origin_thread_id: "lead".into(),
            origin_run_id: "lead-root".into(),
            creation_key: "open".into(),
            brief: storage::lanes::LaneBrief {
                verification_requirements: Default::default(),
                objective: "implement".into(),
                inputs: vec![],
                deliverables: vec![],
                acceptance_criteria: vec!["verified".into()],
                constraints: vec![],
                workspace: Default::default(),
                report_policy: storage::lanes::LanePolicy::Attached,
            },
            start_seed: "seed".into(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns: 1,
        })
        .unwrap();
    f.h()
        .append_lane_report_with_delivery(
            "source-lane".into(),
            report(storage::lanes::LaneStatus::Ready),
            2,
        )
        .unwrap();
}

#[test]
fn v50_outbox_upgrade_preserves_lane_wire_ids_and_title() {
    let f = Fixture::new();
    make_lane(&f);
    let (id, payload): (String, String) = f
        .sql()
        .query_row(
            "SELECT notification_id,payload FROM lane_notifications",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let Fixture {
        _dir,
        config,
        storage,
    } = f;
    storage.close();
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let old = std::fs::read_to_string(root.join("src/migrations/v47.sql")).unwrap();
    let undo = std::fs::read_to_string(root.join("undo/undo_v51.sql")).unwrap();
    let conn = Connection::open(&config.db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn.execute_batch(&undo).unwrap();
    conn.execute_batch(
        "INSERT INTO pull_request_snapshots(provider,host,repository,number,state,
           base_branch,head_branch,head_sha,review_decision,fetched_at_ns,success_seq,fetch_seq,title)
         VALUES ('github','github.com','o/r',1,'open','main','feat','a','none',5,1,1,'kept title');",
    )
    .unwrap();
    conn.execute_batch("CREATE TEMP TABLE saved AS SELECT notification_id,lane_id,management_revision,event_kind,semantic_key,source_thread_id,destination_thread_id,payload,state,claim_epoch,receipt_prompt_id FROM lane_notifications; DROP TABLE lane_notifications;").unwrap();
    let begin = old.find("CREATE TABLE lane_notifications (").unwrap();
    let end = old
        .find("CREATE TABLE lane_delivery_destinations (")
        .unwrap();
    conn.execute_batch(&old[begin..end]).unwrap();
    conn.execute_batch("INSERT INTO lane_notifications SELECT * FROM saved; DROP TABLE saved;")
        .unwrap();
    let begin = old
        .find("CREATE TRIGGER fence_lane_notifications_insert")
        .unwrap();
    let end = old
        .find("CREATE TRIGGER fence_lane_delivery_destinations_insert")
        .unwrap();
    conn.execute_batch(&old[begin..end]).unwrap();
    conn.pragma_update(None, "user_version", 50).unwrap();
    drop(conn);
    let f = Fixture {
        _dir,
        storage: Storage::open(config.clone()).unwrap(),
        config,
    };
    let (new_id, new_payload): (String, String) = f
        .sql()
        .query_row(
            "SELECT notification_id,payload FROM lane_notifications",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((new_id, new_payload), (id.clone(), payload));
    let title: Option<String> = f
        .sql()
        .query_row(
            "SELECT title FROM pull_request_snapshots WHERE repository='o/r' AND number=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(title.as_deref(), Some("kept title"));
    let LaneClaimResult::Claimed(c) = f.h().claim_lane_deliveries(request(10)).unwrap() else {
        panic!("old pending Lane lost")
    };
    assert_eq!(c.notifications[0].notification_id, id);
    assert!(!f.h().pr_auto_fix_state(key()).unwrap().enabled);
}
#[test]
fn sql_constraints_and_tombstone_fences_cannot_be_bypassed() {
    let f = Fixture::new();
    let n = f.ready(1, PrNoticeMode::Manual, 1);
    let c = f.sql();
    assert!(
        c.execute(
            "UPDATE pr_notices SET payload=?1 WHERE notice_id=?2",
            rusqlite::params!["x".repeat(PR_NOTICE_PAYLOAD_MAX + 1), n.notice_id]
        )
        .is_err()
    );
    assert!(c.execute("INSERT INTO pr_notices SELECT 'different-id',pr_key,head_sha,mode,destination_thread_id,link_revision,policy_revision,budget_epoch,success_seq,phase,payload FROM pr_notices",[]).is_err());
    c.execute("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES ('lead','family',12)",[]).unwrap();
    assert!(
        c.execute(
            "UPDATE pr_notices SET destination_thread_id='lead' WHERE notice_id=?1",
            [&n.notice_id]
        )
        .is_err()
    );
    assert!(
        c.execute(
            "UPDATE pr_auto_fix_state SET destination_thread_id='lead'",
            []
        )
        .is_err()
    );
    assert!(
        f.h()
            .reserve_pr_notice(f.reservation(2, PrNoticeMode::Manual, 13))
            .is_err()
    );
}

#[test]
fn real_user_receipt_resets_pr_guard_but_harness_receipt_does_not() {
    let f = Fixture::new();
    let g = f
        .h()
        .update_pr_wake_guard(
            key(),
            0,
            PrGuardSignal::WakeBatch {
                batch_id: "old".into(),
            },
        )
        .unwrap();
    let mut snap = snapshot();
    snap.record.config_json = r#"{"thread_id":"lead"}"#.into();
    f.h().save_snapshot_bundle(&snap).unwrap();
    let mut receipt = HostInputReceipt {
        provenance: HostInputProvenance::HarnessNotice {
            notification_id: "internal".into(),
            destination_thread_id: "lead".into(),
        },
        prompt_id: "harness-notice:internal".into(),
        run_id: "lead-root".into(),
        root_run_id: "lead-root".into(),
        text: String::new(),
        image_count: 0,
        status: HostInputStatus::Accepted,
        reason: None,
        accepted_at_ns: 1,
        status_at_ns: 1,
        evidence_id: None,
    };
    f.h()
        .append_host_input_accepted(&[], &[], None, &receipt)
        .unwrap();
    assert_eq!(f.h().pr_wake_guard(key()).unwrap(), g);
    let db = storage::Database::open(&f.config).unwrap();
    assert!(
        db.host_input_origins(&[receipt.prompt_id.clone()])
            .unwrap()
            .is_empty()
    );
    receipt.provenance = HostInputProvenance::User;
    receipt.prompt_id = "real-input".into();
    receipt.text = "human request".into();
    f.h()
        .append_host_input_accepted(&[], &[], None, &receipt)
        .unwrap();
    assert_eq!(f.h().pr_wake_guard(key()).unwrap().no_progress_batches, 0);
}

#[test]
fn physical_reclaim_removes_delivery_and_payload_but_keeps_adopted_sha() {
    let f = Fixture::new();
    let n = f.ready(1, PrNoticeMode::Manual, 1);
    f.accept(&f.claim(10), 11);
    f.sql().execute_batch("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES ('lead','family',12); INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES ('lead-root','lead','family'); INSERT INTO reclaim_jobs(family_key,state,created_at_ns,updated_at_ns) VALUES ('family','db_pending',12,12);").unwrap();
    loop {
        let step = f
            .h()
            .reclaim_step(storage::thread_retention::ReclaimBudget {
                max_transactions: 1,
            })
            .unwrap();
        assert!(step.last_error.is_none(), "{:?}", step.last_error);
        if step.jobs_db_pending == 0 {
            break;
        }
        assert!(step.transactions > 0);
    }
    assert_eq!(f.count("lane_notifications"), 0);
    assert_eq!(f.count("host_input_receipts"), 0);
    let kept = f.h().pr_notice(n.notice_id).unwrap().unwrap();
    assert_eq!(kept.phase, PrNoticePhase::Adopted);
    assert!(kept.payload.is_none());
    assert!(kept.destination_thread_id.is_none());
    assert_eq!(f.count("pr_notices"), 1);
}

#[test]
fn pr_storage_writes_obey_capacity_suspension_while_reads_remain_available() {
    let f = Fixture::new();
    let n = f.reserve(1, PrNoticeMode::Manual, 1);
    let request = f.reservation(2, PrNoticeMode::Manual, 2);
    let Fixture {
        _dir,
        mut config,
        storage,
    } = f;
    storage.close();
    config.hard_limits.max_db_bytes = 1;
    let f = Fixture {
        _dir,
        storage: Storage::open(config.clone()).unwrap(),
        config,
    };
    assert!(f.h().reserve_pr_notice(request).is_err());
    assert!(
        f.h()
            .ready_pr_notice(n.notice_id.clone(), "facts".into())
            .is_err()
    );
    assert!(f.h().cancel_pr_notice(n.notice_id.clone()).is_err());
    assert_eq!(
        f.h().pr_notice(n.notice_id).unwrap().unwrap().phase,
        PrNoticePhase::Reserved
    );
    assert_eq!(f.count("pr_notices"), 1);
    assert_eq!(f.count("lane_notifications"), 0);
}

#[test]
fn stale_preparation_cannot_ready_or_cancel_another_destinations_replacement() {
    let f = Fixture::new();
    f.link("other");
    f.policy(PrPolicyAction::SetEnabled(true));
    let old = f.reserve(1, PrNoticeMode::Auto, 1);
    // A second Storage owns a distinct SQLite writer connection. The old preparer
    // already read its reservation; deliberately commit the adverse interleaving first.
    let other = Storage::open(f.config.clone()).unwrap();
    let h = other.handle();
    assert!(h.cancel_pr_notice_if_current(old.clone()).unwrap());
    let mut request = f.reservation(1, PrNoticeMode::Manual, 2);
    request.destination_thread_id = "other".into();
    let replacement = h.reserve_pr_notice(request).unwrap();
    assert_eq!(old.notice_id, replacement.notice_id);
    assert!(
        f.h()
            .ready_pr_notice_if_current(old.clone(), "old facts".into())
            .unwrap()
            .is_none()
    );
    assert!(!f.h().cancel_pr_notice_if_current(old.clone()).unwrap());
    assert_eq!(
        h.pr_notice(old.notice_id.clone()).unwrap().unwrap(),
        replacement
    );
    assert_eq!(f.count("lane_notifications"), 0);
    let ready = h
        .ready_pr_notice_if_current(replacement, "new facts".into())
        .unwrap()
        .unwrap();
    assert!(!f.h().cancel_pr_notice_if_current(old).unwrap());
    assert_eq!(
        h.pr_notice(ready.notice_id.clone()).unwrap().unwrap(),
        ready
    );
    assert_eq!(f.count("lane_notifications"), 1);
    assert_eq!(f.h().pr_auto_fix_state(key()).unwrap().reserved, 0);
    assert!(h.cancel_pr_notice_if_current(ready).unwrap());
    let state: String = f
        .sql()
        .query_row("SELECT state FROM lane_notifications", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "obsolete");
}

#[test]
fn identical_cancelled_reservations_have_distinct_preparation_generations() {
    let f = Fixture::new();
    let other = Storage::open(f.config.clone()).unwrap();
    let old = f.reserve(1, PrNoticeMode::Manual, 1);
    assert!(
        other
            .handle()
            .cancel_pr_notice_if_current(old.clone())
            .unwrap()
    );
    let request = f.reservation(1, PrNoticeMode::Manual, 1);
    let current = other.handle().reserve_pr_notice(request.clone()).unwrap();
    assert_eq!(current.reservation_revision, old.reservation_revision + 1);
    let mut same_contents = current.clone();
    same_contents.reservation_revision = old.reservation_revision;
    assert_eq!(
        same_contents, old,
        "the generation is the only ABA discriminator"
    );
    assert!(
        f.h()
            .ready_pr_notice_if_current(old.clone(), "stale".into())
            .unwrap()
            .is_none()
    );
    assert!(!f.h().cancel_pr_notice_if_current(old).unwrap());
    assert_eq!(
        other.handle().reserve_pr_notice(request).unwrap(),
        current,
        "an idempotent reserve does not advance the generation"
    );
    assert!(
        other
            .handle()
            .ready_pr_notice_if_current(current, "fresh".into())
            .unwrap()
            .is_some()
    );
    assert_eq!(f.count("lane_notifications"), 1);
}

#[test]
fn v51_upgrade_preserves_a_ready_notice_and_its_outbox_with_an_initial_generation() {
    let f = Fixture::new();
    let reserved = f.reserve(1, PrNoticeMode::Manual, 1);
    let ready = f
        .h()
        .ready_pr_notice(reserved.notice_id, "durable facts".into())
        .unwrap();
    f.sql()
        .execute_batch(include_str!("../undo/undo_v52.sql"))
        .unwrap();
    f.sql().pragma_update(None, "user_version", 51).unwrap();
    let upgraded = Storage::open(f.config.clone()).unwrap();
    let restored = upgraded
        .handle()
        .pr_notice(ready.notice_id.clone())
        .unwrap()
        .unwrap();
    assert_eq!(restored, ready);
    assert_eq!(restored.reservation_revision, 1);
    assert_eq!(f.count("lane_notifications"), 1);
    assert!(
        upgraded
            .handle()
            .cancel_pr_notice_if_current(restored)
            .unwrap()
    );
    assert_eq!(
        f.reserve(1, PrNoticeMode::Manual, 1).reservation_revision,
        2
    );
}
