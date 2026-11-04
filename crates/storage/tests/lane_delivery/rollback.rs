//! Fail the last writes in each remaining atomicity boundary.
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
        .append_lane_report_with_delivery("lane-1".into(), report(LaneStatus::Ready), 2)
        .unwrap();
    let before_keys = f.count("lane_delivery_keys");
    let before_inbox = f.h().lane_inbox("lead".into(), String::new()).unwrap();
    let mut changed = report(LaneStatus::Ready);
    changed.artifact_revision = Some(ArtifactRevision::CommitSha("b".repeat(40)));
    f.sql().execute_batch("CREATE TRIGGER fail_notice BEFORE INSERT ON lane_notifications BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(
        f.h()
            .append_lane_report_with_delivery("lane-1".into(), changed.clone(), 3),
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
        .append_lane_report_with_delivery("lane-1".into(), changed.clone(), 4)
        .unwrap();
    assert_eq!(next.report.report_seq, first.report.report_seq + 1);
    assert_eq!(f.count("lane_notifications"), 2);
    let keys = f.count("lane_delivery_keys");
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), changed, 5)
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
