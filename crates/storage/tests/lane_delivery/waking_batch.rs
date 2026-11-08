use super::*;

#[test]
fn important_notice_admits_bounded_deferred_updates_but_working_alone_never_wakes() {
    let f = Fixture::new();
    for n in 1..=25 {
        f.lane(n, LanePolicy::Attached);
        f.h()
            .append_lane_report_with_delivery(
                format!("lane-{n}"),
                1,
                report(LaneStatus::Working),
                i64::from(n),
            )
            .unwrap();
    }
    let mut req = request(100);
    req.allow_non_waking = false;
    assert_eq!(
        f.h().claim_lane_deliveries(req.clone()).unwrap(),
        LaneClaimResult::Empty
    );
    f.lane(26, LanePolicy::Attached);
    f.h()
        .append_lane_report_with_delivery("lane-26".into(), 1, report(LaneStatus::Ready), 101)
        .unwrap();
    let LaneClaimResult::Claimed(batch) = f.h().claim_lane_deliveries(req).unwrap() else {
        panic!("important notice must admit its deferred batch")
    };
    assert_eq!(batch.notifications.len(), LANE_DELIVERY_PAGE_MAX);
    assert!(matches!(
        batch.notifications[0].kind,
        event_bus::lane::LaneNotificationKind::Ready { .. }
    ));
    assert!(serde_json::to_string(&batch.notifications).unwrap().len() < LANE_DELIVERY_PAGE_BYTES);
}

#[test]
fn suppressed_important_notice_cannot_wake_deferred_updates() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.lane(2, LanePolicy::Attached);
    f.h()
        .append_lane_report_with_delivery("lane-1".into(), 1, report(LaneStatus::Working), 1)
        .unwrap();
    f.h()
        .append_lane_report_with_delivery("lane-2".into(), 1, report(LaneStatus::Ready), 2)
        .unwrap();
    let mut guard = f.h().lane_wake_guard("lane-2".into()).unwrap();
    for n in 0..20 {
        guard = f
            .h()
            .update_lane_wake_guard(
                "lane-2".into(),
                guard.revision,
                LaneGuardSignal::WakeBatch {
                    batch_id: format!("no-progress-{n}"),
                },
            )
            .unwrap();
    }
    let mut req = request(100);
    req.allow_non_waking = false;
    assert_eq!(
        f.h().claim_lane_deliveries(req).unwrap(),
        LaneClaimResult::Empty
    );
}
