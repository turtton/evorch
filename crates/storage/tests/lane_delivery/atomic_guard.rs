//! Guard state cannot depend on a post-commit runtime callback surviving a crash.
use super::*;

fn fail_guard(f: &Fixture) {
    f.sql().execute_batch("CREATE TRIGGER fail_guard BEFORE UPDATE OF wake_guard_json ON lanes BEGIN SELECT RAISE(ABORT,'injected guard write failure'); END;").unwrap();
}

#[test]
fn wake_guard_and_snapshot_receipt_commit_or_rollback_together() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    let mut request = request(10);
    request.allow_non_waking = false;
    let LaneClaimResult::Claimed(claim) = f.h().claim_lane_deliveries(request).unwrap() else {
        panic!("expected completed-root wake claim");
    };
    fail_guard(&f);
    assert!(
        f.h()
            .accept_lane_delivery(claim.clone(), snapshot(), 11)
            .is_err()
    );
    assert_eq!(f.count("run_contexts"), 0);
    assert_eq!(f.count("host_input_receipts"), 0);
    assert_eq!(
        f.h().lane_wake_guard("lane-1".into()).unwrap(),
        LaneWakeGuard::default()
    );
    f.sql().execute_batch("DROP TRIGGER fail_guard").unwrap();
    assert!(
        f.h()
            .accept_lane_delivery(claim.clone(), snapshot(), 12)
            .unwrap()
            .is_some()
    );
    assert!(
        f.h()
            .accept_lane_delivery(claim.clone(), snapshot(), 13)
            .unwrap()
            .is_none()
    );
    // No runtime callback or ACK: reopen at the exact post-receipt crash boundary.
    let f = f.reopen();
    assert_eq!(
        f.h()
            .lane_wake_guard("lane-1".into())
            .unwrap()
            .no_progress_batches,
        1
    );
    let mut retry = super::request(110);
    retry.allow_non_waking = false;
    assert_eq!(
        f.h().claim_lane_deliveries(retry).unwrap(),
        LaneClaimResult::Empty
    );
    assert_eq!(f.count("host_input_receipts"), 1);
    assert_eq!(
        f.h()
            .lane_wake_guard("lane-1".into())
            .unwrap()
            .no_progress_batches,
        1
    );
}

#[test]
fn terminal_progress_reset_and_report_outbox_commit_or_rollback_together() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    let mut guard = LaneWakeGuard::default();
    for n in 0..LANE_NO_PROGRESS_LIMIT {
        guard = f
            .h()
            .update_lane_wake_guard(
                "lane-1".into(),
                guard.revision,
                LaneGuardSignal::WakeBatch {
                    batch_id: format!("old-{n}"),
                },
            )
            .unwrap();
    }
    assert!(guard.suppressed);
    let input = report(LaneStatus::Ready);
    fail_guard(&f);
    assert!(
        f.h()
            .append_lane_report_with_delivery("lane-1".into(), 1, input.clone(), 2)
            .is_err()
    );
    assert_eq!(f.count("lane_reports"), 0);
    assert_eq!(f.count("lane_notifications"), 0);
    assert_eq!(f.h().lane_wake_guard("lane-1".into()).unwrap(), guard);
    f.sql().execute_batch("DROP TRIGGER fail_guard").unwrap();
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), 1, input.clone(), 3)
        .unwrap();
    // Restart immediately after the source commit, without the runtime reset hook.
    let f = f.reopen();
    let guard = f.h().lane_wake_guard("lane-1".into()).unwrap();
    assert!(!guard.suppressed);
    assert_eq!(guard.no_progress_batches, 0);
    assert_eq!(
        guard.artifact_fingerprint,
        input
            .artifact_revision
            .as_ref()
            .and_then(ArtifactRevision::fingerprint)
    );
    let mut request = request(10);
    request.allow_non_waking = false;
    let LaneClaimResult::Claimed(claim) = f.h().claim_lane_deliveries(request).unwrap() else {
        panic!("new artifact progress must be claimable after restart");
    };
    assert_eq!(claim.notifications.len(), 1);
    assert!(matches!(
        claim.notifications[0].kind,
        LaneNotificationKind::Ready { .. }
    ));
}
