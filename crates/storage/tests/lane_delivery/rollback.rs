//! Fail the last writes in each remaining atomicity boundary.

#[test]
fn host_acceptance_evidence_receipt_and_intervention_are_one_transaction() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.h().start_lane_run("lane-1", 0, "run-1", 2).unwrap();
    let before_guard = f
        .h()
        .update_lane_wake_guard(
            "lane-1".into(),
            0,
            LaneGuardSignal::WakeBatch {
                batch_id: "before-input".into(),
            },
        )
        .unwrap();
    let mut receipt = user_receipt();
    receipt.evidence_id = Some("evidence-lane-input".into());
    let evidence = storage::ReviewEvidenceRow {
        id: receipt.evidence_id.clone().unwrap(),
        root_run_id: "run-1".into(),
        target_run_id: "run-1".into(),
        kind: "host_chat".into(),
        text: receipt.text.clone(),
        in_reply_to_question_id: None,
        in_reply_to_title: None,
        reference_kind: None,
        reference_context: None,
        attachments: Vec::new(),
        created_at_ns: 3,
    };
    f.sql().execute_batch("CREATE TRIGGER fail_host_notice BEFORE INSERT ON lane_notifications BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        f.h()
            .append_host_input_accepted(std::slice::from_ref(&evidence), &[], None, &receipt)
            .is_err()
    );
    assert_eq!(f.count("host_input_receipts"), 0);
    assert_eq!(f.count("review_evidence"), 0);
    assert_eq!(
        f.h().lane_wake_guard("lane-1".into()).unwrap(),
        before_guard
    );
    assert_eq!(f.count("lane_notifications"), 0);
    f.sql()
        .execute_batch("DROP TRIGGER fail_host_notice")
        .unwrap();
    f.h()
        .append_host_input_accepted(&[evidence], &[], None, &receipt)
        .unwrap();
    assert_eq!(f.count("host_input_receipts"), 1);
    assert_eq!(f.count("review_evidence"), 1);
    assert_eq!(
        f.h()
            .lane_wake_guard("lane-1".into())
            .unwrap()
            .no_progress_batches,
        0
    );
    let notices = f.h().lane_inbox("lead".into(), String::new()).unwrap();
    assert_eq!(notices.len(), 1);
    assert!(
        matches!(&notices[0].kind, LaneNotificationKind::Intervention { input_id } if input_id == &receipt.prompt_id)
    );
    assert!(
        !serde_json::to_string(&notices)
            .unwrap()
            .contains(&receipt.text)
    );
}
use super::*;

#[test]
fn route_projection_failure_rolls_back_routing_and_old_notice_invalidation() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    let id = question(1, 0).id;
    let before_question = f.db().user_question(&id).unwrap();
    let before_route = f.h().lane_question_routing(id.clone()).unwrap();
    let before_inbox = f.h().lane_inbox("lead".into(), String::new()).unwrap();
    // Projection is saved after routing and old-notice state have been updated.
    f.sql().execute_batch("CREATE TRIGGER fail_projection BEFORE UPDATE ON user_questions BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let request = mutation(0, LaneQuestionAction::RouteToUser);
    assert!(matches!(
        f.h().mutate_lane_question(request.clone()),
        Err(StorageError::Sqlite(_))
    ));
    assert_eq!(f.db().user_question(&id).unwrap(), before_question);
    assert_eq!(
        f.h().lane_question_routing(id.clone()).unwrap(),
        before_route
    );
    assert_eq!(
        f.h().lane_inbox("lead".into(), String::new()).unwrap(),
        before_inbox
    );
    assert_eq!(
        f.h()
            .pending_lane_delivery_destinations(String::new())
            .unwrap(),
        vec!["lead"]
    );
    f.sql()
        .execute_batch("DROP TRIGGER fail_projection")
        .unwrap();
    let routed = f.h().mutate_lane_question(request).unwrap();
    assert_eq!(routed.id, id);
    assert_eq!(routed.routing_revision, 2);
    assert!(routed.is_user_visible());
    assert!(
        f.h()
            .pending_lane_delivery_destinations(String::new())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn report_outbox_failure_rolls_back_report_sequence_and_semantic_key() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    let first = f
        .h()
        .append_lane_report_with_delivery("lane-1".into(), 1, report(LaneStatus::Ready), 2)
        .unwrap();
    let before_keys = f.count("lane_delivery_keys");
    let before_inbox = f.h().lane_inbox("lead".into(), String::new()).unwrap();
    let mut changed = report(LaneStatus::Ready);
    changed.artifact_revision = Some(ArtifactRevision::CommitSha("b".repeat(40)));
    f.sql().execute_batch("CREATE TRIGGER fail_notice BEFORE INSERT ON lane_notifications BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(
        f.h()
            .append_lane_report_with_delivery("lane-1".into(), 1, changed.clone(), 3),
        Err(StorageError::Sqlite(_))
    ));
    assert_eq!(f.count("lane_reports"), 1);
    assert_eq!(f.count("lane_delivery_keys"), before_keys);
    assert_eq!(
        f.h().lane_inbox("lead".into(), String::new()).unwrap(),
        before_inbox
    );
    f.sql().execute_batch("DROP TRIGGER fail_notice").unwrap();
    let next = f
        .h()
        .append_lane_report_with_delivery("lane-1".into(), 1, changed.clone(), 4)
        .unwrap();
    assert_eq!(next.report.report_seq, first.report.report_seq + 1);
    assert_eq!(f.count("lane_notifications"), 2);
    let keys = f.count("lane_delivery_keys");
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), 1, changed, 5)
        .unwrap();
    assert_eq!(f.count("lane_notifications"), 2);
    assert_eq!(f.count("lane_delivery_keys"), keys);
}

#[test]
fn intervention_outbox_failure_rolls_back_actual_input_receipt_and_semantic_key() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Summary);
    f.h().start_lane_run("lane-1", 0, "run-1", 2).unwrap();
    let before_keys = f.count("lane_delivery_keys");
    let receipt = user_receipt();
    f.sql().execute_batch("CREATE TRIGGER fail_notice BEFORE INSERT ON lane_notifications BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(
        f.h()
            .accept_lane_user_input("lane-1".into(), receipt.clone()),
        Err(StorageError::Sqlite(_))
    ));
    assert!(
        f.db()
            .host_input_receipts(std::slice::from_ref(&receipt.prompt_id))
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.count("lane_notifications"), 0);
    assert_eq!(f.count("lane_delivery_keys"), before_keys);
    f.sql().execute_batch("DROP TRIGGER fail_notice").unwrap();
    f.h()
        .accept_lane_user_input("lane-1".into(), receipt.clone())
        .unwrap();
    f.h()
        .accept_lane_user_input("lane-1".into(), receipt.clone())
        .unwrap();
    assert_eq!(
        f.db().host_input_receipts(&[receipt.prompt_id]).unwrap()[0].text,
        receipt.text
    );
    assert_eq!(f.count("host_input_receipts"), 1);
    assert_eq!(f.count("lane_notifications"), 1);
}
