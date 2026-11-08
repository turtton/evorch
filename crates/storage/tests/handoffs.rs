use event_bus::{LaneBrief, LanePolicy};
use storage::{
    Storage, StorageConfig,
    handoffs::{HandoffReceipt, HandoffState},
    lanes::NewLane,
};

fn reservation(incarnation: &str, target: &str) -> HandoffReceipt {
    HandoffReceipt {
        source_run_id: "source".into(),
        source_incarnation: incarnation.into(),
        target_run_id: target.into(),
        lane_id: format!("lane-{target}"),
        request_json: "original request".into(),
        state: HandoffState::Preparing,
        goal_json: None,
        todo_json: None,
    }
}
#[test]
fn incomplete_intents_never_start_and_committed_receipts_keep_the_same_target() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("handoff.db"),
        ..Default::default()
    };
    let db = Storage::open(config.clone()).unwrap();
    let h = db.handle();
    let first = h
        .reserve_handoff(reservation("incarnation-a", "target"))
        .unwrap();
    h.create_lane(NewLane {
        lane_id: first.lane_id.clone(),
        thread_id: "target-thread".into(),
        origin_thread_id: "source-thread".into(),
        origin_run_id: "source".into(),
        creation_key: "operation".into(),
        brief: LaneBrief {
            objective: "objective".into(),
            inputs: vec![],
            deliverables: vec![],
            acceptance_criteria: vec!["evidence".into()],
            constraints: vec![],
            workspace: Default::default(),
            verification_requirements: Default::default(),
            report_policy: LanePolicy::Detached,
        },
        start_seed: "seed".into(),
        project_id: None,
        project_root: None,
        title: None,
        at_ns: 1,
    })
    .unwrap();
    assert!(h.unstarted_lanes(100).unwrap().is_empty());
    h.advance_handoff(
        "source".into(),
        "incarnation-a".into(),
        HandoffState::Preparing,
        HandoffState::Prepared,
        Some("goal".into()),
        None,
    )
    .unwrap();
    assert!(h.unstarted_lanes(100).unwrap().is_empty());
    // A retry within the same incarnation retains the target; an unrelated
    // incarnation cannot advance this operation.
    let retry = h
        .reserve_handoff(reservation("incarnation-a", "not-a-second-target"))
        .unwrap();
    assert_eq!(retry.target_run_id, first.target_run_id);
    assert!(
        h.advance_handoff(
            "source".into(),
            "stale-incarnation".into(),
            HandoffState::Prepared,
            HandoffState::Committed,
            None,
            None
        )
        .is_err()
    );
    h.advance_handoff(
        "source".into(),
        "incarnation-a".into(),
        HandoffState::Preparing,
        HandoffState::Prepared,
        None,
        None,
    )
    .unwrap();
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_commit BEFORE UPDATE OF state ON lane_handoffs WHEN NEW.state='committed' BEGIN SELECT RAISE(ABORT,'commit injection'); END;").unwrap();
    assert!(
        h.advance_handoff(
            "source".into(),
            "incarnation-a".into(),
            HandoffState::Prepared,
            HandoffState::Committed,
            None,
            None
        )
        .is_err()
    );
    assert_eq!(
        h.handoff_for_target("target".into())
            .unwrap()
            .unwrap()
            .state,
        HandoffState::Prepared
    );
    assert!(h.unstarted_lanes(100).unwrap().is_empty());
    conn.execute_batch("DROP TRIGGER fail_commit").unwrap();
    h.advance_handoff(
        "source".into(),
        "incarnation-a".into(),
        HandoffState::Prepared,
        HandoffState::Committed,
        None,
        None,
    )
    .unwrap();
    h.note_handoff_start_failure("target".into(), "injected admission rejection".into())
        .unwrap();
    assert_eq!(
        h.handoff_for_source("source".into())
            .unwrap()
            .unwrap()
            .state,
        HandoffState::RecoverableStartFailure
    );
    assert_eq!(
        h.reserve_handoff(reservation("incarnation-a", "different-target"))
            .unwrap()
            .target_run_id,
        "target"
    );
    assert_eq!(
        h.reserve_handoff(reservation("incarnation-c", "different-target"))
            .unwrap()
            .target_run_id,
        "different-target"
    );
    assert_eq!(
        h.handoff_operation("source".into(), "incarnation-a".into())
            .unwrap()
            .unwrap()
            .target_run_id,
        "target"
    );
    let pending = h.unstarted_lanes(100).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].lane_id, first.lane_id);
    h.start_lane_run(&first.lane_id, 0, "target", 2).unwrap();
    assert_eq!(
        h.handoff_operation("source".into(), "incarnation-a".into())
            .unwrap()
            .unwrap()
            .state,
        HandoffState::Started
    );
    assert!(h.unstarted_lanes(100).unwrap().is_empty());
}

#[test]
fn transfer_receipt_outlives_either_side_and_is_collected_after_both_are_gone() {
    for source_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("receipt-retention.db"),
            ..Default::default()
        };
        let db = Storage::open(config.clone()).unwrap();
        let h = db.handle();
        let receipt = h
            .reserve_handoff(reservation("incarnation-a", "target"))
            .unwrap();
        h.create_lane(NewLane {
            lane_id: receipt.lane_id,
            thread_id: "target-thread".into(),
            origin_thread_id: "source-thread".into(),
            origin_run_id: "source".into(),
            creation_key: "retention-operation".into(),
            brief: LaneBrief {
                objective: "objective".into(),
                inputs: vec![],
                deliverables: vec![],
                acceptance_criteria: vec!["evidence".into()],
                constraints: vec![],
                workspace: Default::default(),
                verification_requirements: Default::default(),
                report_policy: LanePolicy::Detached,
            },
            start_seed: "seed".into(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns: 1,
        })
        .unwrap();
        for (before, after) in [
            (HandoffState::Preparing, HandoffState::Prepared),
            (HandoffState::Prepared, HandoffState::Committed),
        ] {
            h.advance_handoff(
                "source".into(),
                "incarnation-a".into(),
                before,
                after,
                None,
                None,
            )
            .unwrap();
        }
        let conn = rusqlite::Connection::open(&config.db_path).unwrap();
        let source_delete = "INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES('source','source-thread','source-thread')";
        let target_delete = "DELETE FROM lanes WHERE lane_id='lane-target'";
        let (first, second) = if source_first {
            (source_delete, target_delete)
        } else {
            (target_delete, source_delete)
        };
        conn.execute_batch(first).unwrap();
        assert!(
            h.handoff_operation("source".into(), "incarnation-a".into())
                .unwrap()
                .is_some()
        );
        conn.execute_batch(second).unwrap();
        assert!(
            h.handoff_operation("source".into(), "incarnation-a".into())
                .unwrap()
                .is_none()
        );
    }
}
