//! Integer clocks and synchronous writer completion: no timing assumptions.
use super::*;
fn setup() -> Fixture {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    f.create(1, 1, LaneDeliveryBinding::Bound);
    f.h().upsert_run_context(&snapshot().record).unwrap();
    f
}
fn failure(f: &Fixture, at: i64) -> LaneRecoveryEpisode {
    let c = claim(&f.h(), at);
    let attempt = f.h().prepare_lane_recovery(c.clone(), at).unwrap();
    let e = f
        .h()
        .record_lane_recovery_failure(c.clone(), attempt.clone(), at + 1)
        .unwrap()
        .episode;
    let retry = f
        .h()
        .record_lane_recovery_failure(c.clone(), attempt, at + 1)
        .unwrap();
    assert_eq!(retry.episode, e);
    assert!(retry.report.is_none());
    f.h()
        .nack_lane_delivery(c, LaneDeliveryNack::RestoreFailed, at + 2)
        .unwrap();
    e
}
#[test]
fn three_failures_count_batches_not_notices_and_survive_restart_rebind_until_resume() {
    let mut f = setup();
    let mut id = None;
    for count in 1..=3 {
        let e = failure(&f, count as i64 * 10);
        assert_eq!(e.attempts, count);
        assert_eq!(
            e.state,
            if count == 3 {
                LaneRecoveryState::Suppressed
            } else {
                LaneRecoveryState::Retryable
            }
        );
        if let Some(id) = &id {
            assert_eq!(&e.episode_id, id);
        }
        id = Some(e.episode_id);
        f = f.reopen();
    }
    assert_eq!(f.count("lane_notifications"), 2);
    let changed_owner = LaneDeliveryOwner {
        ownership_generation: 9,
        ..owner()
    };
    f.h()
        .bind_lane_delivery_destination(
            "lead".into(),
            0,
            changed_owner.clone(),
            LaneDestinationAction::Bind,
        )
        .unwrap();
    let mut req = request(50);
    req.expected_destination_epoch = 1;
    req.owner = changed_owner.clone();
    let LaneClaimResult::Claimed(c) = f.h().claim_lane_deliveries(req.clone()).unwrap() else {
        panic!()
    };
    let suppressed = f.h().prepare_lane_recovery(c.clone(), 50).unwrap();
    assert_eq!(suppressed.episode.state, LaneRecoveryState::Suppressed);
    assert_eq!(suppressed.episode.attempts, 3);
    assert!(
        f.h()
            .record_lane_recovery_failure(c.clone(), suppressed, 51)
            .is_err()
    );
    f.h()
        .nack_lane_delivery(c, LaneDeliveryNack::RestoreFailed, 52)
        .unwrap();
    let filter = storage::lane_control::LaneAttentionFilter {
        lead_thread_id: "lead".into(),
        project_id: None,
    };
    assert!(f.db().lane_attention(filter.clone(), None, 20).unwrap().items.iter().any(|i| matches!(&i.kind, storage::lane_control::LaneAttentionKind::RestoreFailed { attempts: 3, suppressed: true, failure_ref, .. } if Some(failure_ref) == id.as_ref())));
    f.h()
        .bind_lane_delivery_destination(
            "lead".into(),
            1,
            changed_owner,
            LaneDestinationAction::ExplicitUserResume,
        )
        .unwrap();
    assert!(
        !f.db()
            .lane_attention(filter, None, 20)
            .unwrap()
            .items
            .iter()
            .any(|i| matches!(
                i.kind,
                storage::lane_control::LaneAttentionKind::RestoreFailed { .. }
            ))
    );
    req.expected_destination_epoch = 2;
    req.now_ns = 60;
    req.lease_until_ns = 160;
    req.token = "resume-claim".into();
    let LaneClaimResult::Claimed(c) = f.h().claim_lane_deliveries(req).unwrap() else {
        panic!()
    };
    let resumed = f.h().prepare_lane_recovery(c, 60).unwrap();
    assert_ne!(Some(resumed.episode.episode_id), id);
    assert_eq!(resumed.episode.attempts, 0);
}
#[test]
fn repaired_source_is_a_new_episode_but_timestamps_reports_and_question_churn_are_not() {
    let f = setup();
    let old = failure(&f, 10);
    let mut record = snapshot().record;
    record.updated_at_ns = 900;
    f.h().upsert_run_context(&record).unwrap();
    f.h()
        .append_lane_report_with_delivery(
            "lane-1".into(),
            1,
            LaneReportInput {
                artifact_revision: None,
                ..report(LaneStatus::Working)
            },
            12,
        )
        .unwrap();
    f.create(1, 2, LaneDeliveryBinding::Bound);
    let c = claim(&f.h(), 20);
    let same = f.h().prepare_lane_recovery(c.clone(), 20).unwrap();
    assert_eq!(same.episode.episode_id, old.episode_id);
    f.h()
        .nack_lane_delivery(c, LaneDeliveryNack::StorageFailed, 21)
        .unwrap();
    record.config_json = r#"{"repaired":true}"#.into();
    f.h().upsert_run_context(&record).unwrap();
    let c = claim(&f.h(), 30);
    let repaired = f.h().prepare_lane_recovery(c, 30).unwrap();
    assert_ne!(repaired.episode.episode_id, old.episode_id);
    assert_ne!(
        repaired.episode.stable_recovery_target,
        old.stable_recovery_target
    );
    assert_eq!(repaired.episode.attempts, 0);
    assert_eq!(
        f.h()
            .lane_recovery_episode(old.episode_id)
            .unwrap()
            .unwrap()
            .state,
        LaneRecoveryState::Resolved
    );
}
#[test]
fn genuine_terminal_artifact_progress_releases_recovery_without_releasing_stop() {
    let f = setup();
    let old = failure(&f, 10);
    f.h()
        .bind_lane_delivery_destination("lead".into(), 0, owner(), LaneDestinationAction::Stop)
        .unwrap();
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), 1, report(LaneStatus::Ready), 20)
        .unwrap();
    assert_eq!(
        f.h()
            .lane_recovery_episode(old.episode_id)
            .unwrap()
            .unwrap()
            .state,
        LaneRecoveryState::Resolved
    );
    assert!(
        f.h()
            .lane_delivery_destination("lead".into())
            .unwrap()
            .unwrap()
            .stopped
    );
}
#[test]
fn stale_expired_stopped_deleted_claims_and_repaired_targets_do_not_count() {
    for case in ["rebound", "expired", "stopped", "deleted", "repaired"] {
        let f = setup();
        let c = claim(&f.h(), 10);
        let a = f.h().prepare_lane_recovery(c.clone(), 10).unwrap();
        match case {
            "rebound" => {
                f.h()
                    .bind_lane_delivery_destination(
                        "lead".into(),
                        0,
                        owner(),
                        LaneDestinationAction::Bind,
                    )
                    .unwrap();
            }
            "stopped" => {
                f.h()
                    .bind_lane_delivery_destination(
                        "lead".into(),
                        0,
                        owner(),
                        LaneDestinationAction::Stop,
                    )
                    .unwrap();
            }
            "deleted" => {
                f.sql().execute("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES('lead','lead',11)", []).unwrap();
            }
            "repaired" => {
                let mut r = snapshot().record;
                r.config_json = r#"{"repaired":true}"#.into();
                f.h().upsert_run_context(&r).unwrap();
            }
            _ => (),
        }
        assert!(
            f.h()
                .record_lane_recovery_failure(
                    c,
                    a.clone(),
                    if case == "expired" { 111 } else { 12 }
                )
                .is_err(),
            "{case}"
        );
        if case == "deleted" {
            assert_eq!(f.count("lane_recovery_episodes"), 0);
        } else {
            assert_eq!(
                f.h()
                    .lane_recovery_episode(a.episode.episode_id)
                    .unwrap()
                    .unwrap()
                    .attempts,
                0,
                "{case}"
            );
        }
    }
}
fn answer_fixture() -> (Fixture, LaneClaimRequest) {
    let f = setup();
    f.h().start_lane_run("lane-1", 0, "run-1", 2).unwrap();
    let mut record = snapshot().record;
    record.run_id = "run-1".into();
    f.h().upsert_run_context(&record).unwrap();
    f.h()
        .mutate_lane_question(mutation(
            0,
            LaneQuestionAction::Answer {
                text: "continue".into(),
                origin: AnswerOrigin::LaneLead {
                    run_id: "lead-root".into(),
                    thread_id: "lead".into(),
                },
            },
        ))
        .unwrap();
    let req = LaneClaimRequest {
        destination_thread_id: "thread-1".into(),
        owner: LaneDeliveryOwner {
            root_run_id: "run-1".into(),
            ..owner()
        },
        ..request(10)
    };
    (f, req)
}
#[test]
fn lane_failure_report_outbox_and_attempt_are_atomic_and_ready_cannot_be_overwritten() {
    for ready in [false, true] {
        let (f, req) = answer_fixture();
        let LaneClaimResult::Claimed(c) = f.h().claim_lane_deliveries(req).unwrap() else {
            panic!()
        };
        let a = f.h().prepare_lane_recovery(c.clone(), 10).unwrap();
        if ready {
            f.h()
                .append_lane_report_with_delivery("lane-1".into(), 1, report(LaneStatus::Ready), 11)
                .unwrap();
            assert!(f.h().record_lane_recovery_failure(c, a, 12).is_err());
            assert_eq!(f.h().lane("lane-1").unwrap().unwrap().latest_report_seq, 1);
            assert_eq!(
                f.h().lane_reports("lane-1", 0, 20).unwrap()[0].status,
                LaneStatus::Ready
            );
        } else {
            f.sql().execute_batch("CREATE TRIGGER fail_recovery_report BEFORE INSERT ON lane_reports BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
            assert!(
                f.h()
                    .record_lane_recovery_failure(c.clone(), a.clone(), 11)
                    .is_err()
            );
            assert_eq!(
                f.h()
                    .lane_recovery_episode(a.episode.episode_id.clone())
                    .unwrap()
                    .unwrap()
                    .attempts,
                0
            );
            f.sql()
                .execute_batch("DROP TRIGGER fail_recovery_report")
                .unwrap();
            let fail = f
                .h()
                .record_lane_recovery_failure(c.clone(), a.clone(), 12)
                .unwrap();
            assert!(fail.report.is_some());
            assert!(fail.episode.failure_report_ref.is_some());
            assert_eq!(f.count("lane_reports"), 1);
            f.h()
                .nack_lane_delivery(c, LaneDeliveryNack::RestoreFailed, 13)
                .unwrap();
            let req = LaneClaimRequest {
                destination_thread_id: "thread-1".into(),
                owner: LaneDeliveryOwner {
                    root_run_id: "run-1".into(),
                    ..owner()
                },
                ..request(20)
            };
            let LaneClaimResult::Claimed(c) = f.h().claim_lane_deliveries(req).unwrap() else {
                panic!()
            };
            let a = f.h().prepare_lane_recovery(c.clone(), 20).unwrap();
            let retry = f.h().record_lane_recovery_failure(c, a, 21).unwrap();
            assert_eq!(retry.episode.attempts, 2);
            assert!(retry.report.is_none());
            assert_eq!(f.count("lane_reports"), 1);
        }
    }
}

#[test]
fn accepted_receipt_recovery_and_claimed_delivery_share_one_episode_and_root_retention_cleans_it() {
    let f = setup();
    let first = failure(&f, 10);
    for n in 2..=3 {
        let a = f
            .h()
            .prepare_lane_destination_recovery(
                "lead".into(),
                0,
                owner(),
                format!("receipt-attempt-{n}"),
            )
            .unwrap();
        assert_eq!(a.episode.episode_id, first.episode_id);
        let result = f
            .h()
            .record_lane_destination_recovery_failure(a.clone(), n * 10)
            .unwrap();
        assert_eq!(result.episode.attempts, n as u32);
        assert_eq!(
            f.h()
                .record_lane_destination_recovery_failure(a, n * 10)
                .unwrap()
                .episode,
            result.episode
        );
    }
    let f = f.reopen();
    let c = claim(&f.h(), 40);
    assert_eq!(
        f.h().prepare_lane_recovery(c, 40).unwrap().episode.state,
        LaneRecoveryState::Suppressed
    );
    // Accepted-input work may still need recovery after sender ACK repair.
    f.sql()
        .execute("UPDATE lane_notifications SET state='acked'", [])
        .unwrap();
    let page = f
        .db()
        .lane_attention(
            storage::lane_control::LaneAttentionFilter {
                lead_thread_id: "lead".into(),
                project_id: None,
            },
            None,
            20,
        )
        .unwrap();
    assert!(page.items.iter().any(|i| matches!(
        i.kind,
        storage::lane_control::LaneAttentionKind::RestoreFailed {
            attempts: 3,
            suppressed: true,
            ..
        }
    )));
    f.sql().execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES('lead-root','lead','lead')", []).unwrap();
    assert_eq!(f.count("lane_recovery_episodes"), 0);
}

#[test]
fn late_success_cannot_clear_a_new_owner_stop_or_newer_failure() {
    for case in ["rebound", "stopped", "revision"] {
        let f = setup();
        failure(&f, 10);
        let c = claim(&f.h(), 11);
        let a = f.h().prepare_lane_recovery(c.clone(), 11).unwrap();
        let expected = if case == "revision" {
            f.h()
                .record_lane_recovery_failure(c, a.clone(), 12)
                .unwrap()
                .episode
        } else {
            f.h()
                .bind_lane_delivery_destination(
                    "lead".into(),
                    0,
                    owner(),
                    if case == "stopped" {
                        LaneDestinationAction::Stop
                    } else {
                        LaneDestinationAction::Bind
                    },
                )
                .unwrap();
            a.episode.clone()
        };
        f.h().resolve_lane_recovery(a.clone()).unwrap();
        assert_eq!(
            f.h()
                .lane_recovery_episode(a.episode.episode_id)
                .unwrap()
                .unwrap(),
            expected,
            "{case}"
        );
    }
}

#[test]
fn recovery_failure_diagnostics_do_not_release_the_outgoing_wake_guard() {
    let f = setup();
    f.h().start_lane_run("lane-1", 0, "run-1", 2).unwrap();
    let mut guard = f.h().lane_wake_guard("lane-1".into()).unwrap();
    for n in 0..LANE_NO_PROGRESS_LIMIT {
        guard = f
            .h()
            .update_lane_wake_guard(
                "lane-1".into(),
                guard.revision,
                LaneGuardSignal::WakeBatch {
                    batch_id: format!("guard-{n}"),
                },
            )
            .unwrap();
    }
    assert!(guard.suppressed);
    for episode in ["original-source-failure", "repaired-source-failure"] {
        let mut input = report(LaneStatus::Failed);
        input.artifact_revision = None;
        input.failure_episode = Some(episode.into());
        input.producer = LaneReportProducer::Harness;
        input.reason_code = Some("restore_failed".into());
        f.h()
            .append_lane_report_with_delivery("lane-1".into(), 1, input, 10)
            .unwrap();
        assert_eq!(f.h().lane_wake_guard("lane-1".into()).unwrap(), guard);
    }
    let mut ordinary = report(LaneStatus::Failed);
    ordinary.failure_episode = Some("genuine-worker-failure".into());
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), 1, ordinary, 20)
        .unwrap();
    let progressed = f.h().lane_wake_guard("lane-1".into()).unwrap();
    assert!(!progressed.suppressed);
    assert_eq!(progressed.no_progress_batches, 0);
}
