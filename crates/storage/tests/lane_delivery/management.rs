//! Public single-writer integration at claim/receipt/management boundaries.
use super::*;
use storage::lane_management::*;
fn manage(f: &Fixture, revision: u64, action: LaneManagementAction) -> LaneManagementResult {
    f.h()
        .apply_lane_management_transition(
            LaneManagementRequest {
                operation_id: format!("manage-{revision}"),
                lane_id: "lane-1".into(),
                expected_management_revision: revision,
                action,
            },
            LaneManagementActor::User,
            None,
            40,
        )
        .unwrap()
}
fn lane_claim(f: &Fixture, owner: &LaneDeliveryOwner, now: i64) -> LaneDeliveryClaim {
    let mut r = request(now);
    r.destination_thread_id = "thread-1".into();
    r.expected_destination_epoch = 1;
    r.owner = owner.clone();
    match f.h().claim_lane_deliveries(r).unwrap() {
        LaneClaimResult::Claimed(c) => c,
        other => panic!("{other:?}"),
    }
}
fn lane_snapshot() -> SnapshotWrite {
    let mut s = snapshot();
    s.record.run_id = "run-1".into();
    s.record.name = "chat:Orchestrator:thread-1".into();
    s
}
#[test]
fn policy_routes_only_new_questions_to_lead_and_rejects_delayed_lead_answers() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    let LaneQuestionCreated::Created(original) = f.create(1, 0, LaneDeliveryBinding::Bound) else {
        panic!("question capacity is available");
    };
    manage(
        &f,
        1,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
    );
    let mut late = mutation(
        0,
        LaneQuestionAction::Answer {
            text: "too late".into(),
            origin: AnswerOrigin::LaneLead {
                thread_id: "lead".into(),
                run_id: "lead-root".into(),
            },
        },
    );
    late.expected_management_revision = 2;
    late.expected_routing_revision = 2;
    assert!(f.h().mutate_lane_question(late).is_err());
    manage(
        &f,
        2,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Attached,
        },
    );
    let old = f.db().user_question(&original.id).unwrap().unwrap();
    assert_eq!(old.responder, Some(QuestionResponder::User));
    assert_eq!(old.run_id, original.run_id);
    assert_eq!(old.root_run_id, original.root_run_id);
    let LaneQuestionCreated::Created(new) = f.create(1, 1, LaneDeliveryBinding::Bound) else {
        panic!("question capacity is available");
    };
    assert_eq!(
        new.responder,
        Some(QuestionResponder::Lead {
            thread_id: "lead".into()
        })
    );
    assert_eq!(new.management_revision, Some(3));
    manage(&f, 3, LaneManagementAction::Detach);
    let detached = f.db().user_question(&new.id).unwrap().unwrap();
    assert_eq!(detached.responder, Some(QuestionResponder::User));
    assert_eq!(detached.routing_revision, new.routing_revision + 1);
    assert_eq!(detached.run_id, new.run_id);
    f.h()
        .answer_user_question(
            &new.id,
            "user decides",
            &AnswerOrigin::Host {
                channel: Default::default(),
            },
        )
        .unwrap();
    assert_eq!(
        f.db().user_question(&new.id).unwrap().unwrap().answer,
        Some("user decides".into())
    );
}
#[test]
fn accepted_answer_survives_detach_before_delivery_and_after_receipt_without_duplicate_input() {
    for receive_first in [false, true] {
        let f = Fixture::new();
        let owner = started(&f);
        f.create(1, 0, LaneDeliveryBinding::Bound);
        let answered = f
            .h()
            .mutate_lane_question(mutation(
                0,
                LaneQuestionAction::Answer {
                    text: "accepted".into(),
                    origin: AnswerOrigin::LaneLead {
                        thread_id: "lead".into(),
                        run_id: "lead-root".into(),
                    },
                },
            ))
            .unwrap();
        let old = lane_claim(&f, &owner, 10);
        let id = old.notifications[0].notification_id.clone();
        if receive_first {
            f.h()
                .accept_lane_delivery(old.clone(), lane_snapshot(), 11)
                .unwrap();
        }
        let result = manage(&f, 1, LaneManagementAction::Detach);
        assert_eq!(result.lane.management_revision, 2);
        assert!(
            f.h()
                .accept_lane_delivery(old.clone(), lane_snapshot(), 12)
                .is_err()
        );
        if receive_first {
            let mut request = request(13);
            request.destination_thread_id = "thread-1".into();
            request.expected_destination_epoch = 1;
            request.owner = owner.clone();
            assert_eq!(
                f.h().claim_lane_deliveries(request).unwrap(),
                LaneClaimResult::Empty
            );
        } else {
            let fresh = lane_claim(&f, &owner, 13);
            assert_eq!(fresh.notifications[0].notification_id, id);
            assert_eq!(fresh.notifications[0].management_revision, 2);
            assert!(
                f.h()
                    .accept_lane_delivery(fresh.clone(), lane_snapshot(), 14)
                    .unwrap()
                    .is_some()
            );
            f.h().ack_lane_delivery(fresh, 15).unwrap();
        }
        assert_eq!(f.count("host_input_receipts"), 1);
        assert_eq!(
            f.db().user_question(&answered.id).unwrap().unwrap().answer,
            Some("accepted".into())
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &f.db().run_context("run-1").unwrap().unwrap().messages_json
            )
            .unwrap(),
            serde_json::from_str::<serde_json::Value>(&lane_snapshot().record.messages_json)
                .unwrap()
        );
    }
}
#[test]
fn changing_one_member_releases_entire_claim_and_preserves_other_lane_receipts() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.lane(2, LanePolicy::Attached);
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), 1, report(LaneStatus::Ready), 2)
        .unwrap();
    f.h()
        .append_lane_report_with_delivery("lane-2".into(), 1, report(LaneStatus::Ready), 2)
        .unwrap();
    let old = claim(&f.h(), 10);
    assert_eq!(old.notifications.len(), 2);
    f.h()
        .accept_lane_delivery(old.clone(), snapshot(), 11)
        .unwrap();
    let saved = f
        .db()
        .run_context("lead-root")
        .unwrap()
        .unwrap()
        .messages_json;
    manage(&f, 1, LaneManagementAction::Detach);
    assert!(f.h().ack_lane_delivery(old, 12).is_err());
    // The surviving member's durable receipt repairs its ACK without another wake.
    assert_eq!(
        f.h().claim_lane_deliveries(request(13)).unwrap(),
        LaneClaimResult::Empty
    );
    let c = rusqlite::Connection::open(&f.config.db_path).unwrap();
    assert_eq!(
        c.query_row(
            "SELECT state FROM lane_notifications WHERE lane_id='lane-2'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "acked"
    );
    assert_eq!(
        f.db()
            .run_context("lead-root")
            .unwrap()
            .unwrap()
            .messages_json,
        saved
    );
    assert_eq!(f.count("host_input_receipts"), 2);
}
#[test]
fn policy_change_preserves_nudge_slot_guard_and_stop_but_invalidates_old_claim() {
    let f = Fixture::new();
    let owner = started(&f);
    let obs = observation(&f, &owner);
    let ep = f.h().observe_lane_stall(obs.clone()).unwrap();
    let ep = f
        .h()
        .request_lane_nudge(obs.clone(), ep.episode_id, ep.revision)
        .unwrap();
    let old = lane_claim(&f, &owner, 10);
    let guard = f.db().lane_wake_guard("lane-1").unwrap();
    manage(
        &f,
        1,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
    );
    assert!(
        f.h()
            .accept_lane_delivery(old, lane_snapshot(), 11)
            .is_err()
    );
    let mut new_obs = obs;
    new_obs.management_revision = 2;
    let current = f.h().observe_lane_stall(new_obs.clone()).unwrap();
    assert_eq!(current.episode_id, ep.episode_id);
    assert_eq!(current.notification_id, ep.notification_id);
    assert_eq!(current.nudge_state, LaneNudgeState::Pending);
    assert_eq!(f.db().lane_wake_guard("lane-1").unwrap(), guard);
    let claim = lane_claim(&f, &owner, 12);
    f.h()
        .accept_lane_delivery(claim.clone(), lane_snapshot(), 13)
        .unwrap();
    f.h().ack_lane_delivery(claim, 14).unwrap();
    assert_eq!(
        f.h()
            .request_lane_nudge(new_obs, current.episode_id, current.revision)
            .unwrap()
            .nudge_state,
        LaneNudgeState::Accepted
    );
    f.h()
        .bind_lane_delivery_destination("thread-1".into(), 1, owner, LaneDestinationAction::Stop)
        .unwrap();
    manage(
        &f,
        2,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Attached,
        },
    );
    assert!(
        f.h()
            .lane_delivery_destination("thread-1".into())
            .unwrap()
            .unwrap()
            .stopped
    );
}
#[test]
fn policy_change_requires_new_ready_and_verification_and_detached_reads_are_empty() {
    let f = Fixture::new();
    started(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let old = verified(&f, 8);
    f.h().close_lane(close_request(&old)).unwrap();
    assert!(f.h().current_lane_close("lane-1".into()).unwrap().is_some());
    manage(
        &f,
        1,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
    );
    assert_eq!(f.h().current_lane_close("lane-1".into()).unwrap(), None);
    rejected(
        f.h().lane_verification_target("lane-1".into()),
        LaneOperationError::StaleReady,
    );
    rejected(
        f.h().close_lane(close_request(&old)),
        LaneOperationError::StaleManagement,
    );
    f.h()
        .append_lane_report("lane-1", 2, report(LaneStatus::Ready), 41)
        .unwrap();
    let fresh = verified(&f, 9);
    f.h().close_lane(close_request(&fresh)).unwrap();
    assert!(f.h().current_lane_close("lane-1".into()).unwrap().is_some());
    manage(&f, 2, LaneManagementAction::Detach);
    assert_eq!(f.h().current_lane_close("lane-1".into()).unwrap(), None);
    assert!(
        f.h()
            .lane_attention(filter(), None, 20)
            .unwrap()
            .items
            .is_empty()
    );
}
#[test]
fn monitor_failure_cas_never_overwrites_ready_progress_or_new_report() {
    for change in ["ready", "input", "working", "not_failed", "none"] {
        let f = Fixture::new();
        let owner = started(&f);
        let obs = observation(&f, &owner);
        let ep = f.h().observe_lane_stall(obs.clone()).unwrap();
        let ep = if change == "not_failed" {
            ep
        } else {
            let ep = f
                .h()
                .request_lane_nudge(obs.clone(), ep.episode_id, ep.revision)
                .unwrap();
            f.h()
                .record_lane_nudge_outcome(
                    obs.clone(),
                    ep.episode_id,
                    ep.revision,
                    LaneNudgeState::Failed,
                )
                .unwrap()
        };
        let mut failure = report(LaneStatus::Failed);
        failure.producer = LaneReportProducer::Harness;
        failure.failure_episode = Some(ep.episode_id.clone());
        let req = LaneMonitorFailure {
            observation: obs,
            episode_id: ep.episode_id,
            expected_episode_revision: ep.revision,
            expected_report_seq: 0,
            report: failure,
        };
        match change {
            "ready" => {
                f.h()
                    .append_lane_report_with_delivery(
                        "lane-1".into(),
                        1,
                        report(LaneStatus::Ready),
                        3,
                    )
                    .unwrap();
            }
            "working" => {
                f.h()
                    .append_lane_report("lane-1", 1, report(LaneStatus::Working), 3)
                    .unwrap();
            }
            "input" => {
                f.h()
                    .accept_lane_user_input("lane-1".into(), user_receipt())
                    .unwrap();
            }
            _ => {}
        }
        let result = f.h().append_lane_monitor_failure(req).unwrap();
        assert_eq!(result.is_some(), change == "none");
        let reports = f.h().lane_reports("lane-1", 0, 20).unwrap();
        if change == "ready" {
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].status, LaneStatus::Ready);
        }
    }
}

#[test]
fn commit_without_projection_save_is_repaired_from_ledger_after_restart() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    let req = LaneManagementRequest {
        operation_id: "crash-after-commit".into(),
        lane_id: "lane-1".into(),
        expected_management_revision: 1,
        action: LaneManagementAction::Detach,
    };
    let first = f
        .h()
        .apply_lane_management_transition(req.clone(), LaneManagementActor::User, None, 40)
        .unwrap();
    let f = f.reopen();
    let mut replay = f
        .h()
        .apply_lane_management_transition(req, LaneManagementActor::User, None, 41)
        .unwrap();
    assert!(replay.replayed);
    replay.replayed = false;
    assert_eq!(replay, first);
    let pending = f.h().pending_lane_management_projections(20).unwrap();
    assert_eq!(pending, vec![first.lane]);
    f.h()
        .acknowledge_lane_management_projection("lane-1".into(), 2)
        .unwrap();
    assert!(
        f.h()
            .pending_lane_management_projections(20)
            .unwrap()
            .is_empty()
    );
}
