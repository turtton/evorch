//! Restart and receipt-only recovery; all time is caller-injected.
use super::*;

#[test]
fn lost_bind_reply_recovers_epoch_owner_and_stop_after_reopen() {
    for action in [LaneDestinationAction::Bind, LaneDestinationAction::Stop] {
        let f = Fixture::new();
        f.lane(1, LanePolicy::Attached);
        f.create(1, 0, LaneDeliveryBinding::Bound);
        assert_eq!(
            f.h().lane_delivery_destination("lead".into()).unwrap(),
            None
        );
        // Discard the committed reply: restart knows only the destination thread.
        f.h()
            .bind_lane_delivery_destination("lead".into(), 0, owner(), action)
            .unwrap();
        let f = f.reopen();
        let saved = f
            .h()
            .lane_delivery_destination("lead".into())
            .unwrap()
            .unwrap();
        assert_eq!(
            saved,
            LaneDeliveryDestination {
                thread_id: "lead".into(),
                epoch: 1,
                stopped: action == LaneDestinationAction::Stop,
                owner: Some(owner()),
            }
        );
        assert_eq!(
            f.h()
                .lane_delivery_destination("never-bound".into())
                .unwrap(),
            None
        );
        assert!(matches!(
            f.h().claim_lane_deliveries(request(10)),
            Err(StorageError::LaneDeliveryConflict(
                "stale_destination_epoch"
            ))
        ));
        let next_owner = LaneDeliveryOwner {
            generation: 2,
            ..owner()
        };
        let epoch = f
            .h()
            .bind_lane_delivery_destination(
                saved.thread_id.clone(),
                saved.epoch,
                next_owner.clone(),
                LaneDestinationAction::Bind,
            )
            .unwrap();
        let rebound = f
            .h()
            .lane_delivery_destination(saved.thread_id.clone())
            .unwrap()
            .unwrap();
        assert_eq!(rebound.epoch, epoch);
        assert_eq!(rebound.owner, Some(next_owner.clone()));
        assert_eq!(rebound.stopped, saved.stopped);
        let mut req = LaneClaimRequest {
            expected_destination_epoch: rebound.epoch,
            owner: next_owner.clone(),
            ..request(11)
        };
        if saved.stopped {
            assert_eq!(
                f.h().claim_lane_deliveries(req.clone()).unwrap(),
                LaneClaimResult::Stopped
            );
            req.expected_destination_epoch = f
                .h()
                .bind_lane_delivery_destination(
                    saved.thread_id,
                    rebound.epoch,
                    next_owner,
                    LaneDestinationAction::ExplicitUserResume,
                )
                .unwrap();
        }
        let LaneClaimResult::Claimed(claim) = f.h().claim_lane_deliveries(req).unwrap() else {
            panic!("restart lost pending delivery")
        };
        assert_eq!(claim.notifications.len(), 1);
        f.h()
            .nack_lane_delivery(claim, LaneDeliveryNack::StorageFailed, 12)
            .unwrap();
    }
}

#[test]
fn suppressed_lane_repairs_only_received_notices_after_crash_without_waking() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    let old = claim(&f.h(), 10);
    f.h()
        .accept_lane_delivery(old.clone(), snapshot(), 11)
        .unwrap();
    f.create(1, 1, LaneDeliveryBinding::Bound); // No receipt: suppression must hold.
    let mut guard = LaneWakeGuard::default();
    for n in 0..LANE_NO_PROGRESS_LIMIT {
        guard = f
            .h()
            .update_lane_wake_guard(
                "lane-1".into(),
                guard.revision,
                LaneGuardSignal::WakeBatch {
                    batch_id: format!("wake-{n}"),
                },
            )
            .unwrap();
    }
    assert!(guard.suppressed);
    let before = f
        .db()
        .run_context("lead-root")
        .unwrap()
        .unwrap()
        .messages_json;
    let f = f.reopen();
    // A live claimant is untouched. At expiry, only committed receipts repair ACK.
    assert_eq!(
        f.h().claim_lane_deliveries(request(109)).unwrap(),
        LaneClaimResult::Busy
    );
    assert!(f.h().ack_lane_delivery(old.clone(), 110).is_err());
    assert_eq!(
        f.h().claim_lane_deliveries(request(110)).unwrap(),
        LaneClaimResult::Empty
    );
    f.h().ack_lane_delivery(old, 111).unwrap();
    assert_eq!(f.h().lane_wake_guard("lane-1".into()).unwrap(), guard);
    assert_eq!(
        f.db()
            .run_context("lead-root")
            .unwrap()
            .unwrap()
            .messages_json,
        before
    );
    assert_eq!(f.count("host_input_receipts"), 1);
    assert_eq!(
        f.h().claim_lane_deliveries(request(112)).unwrap(),
        LaneClaimResult::Empty
    );
    // The unreceived notice is available only after a genuine resume signal.
    f.h()
        .update_lane_wake_guard(
            "lane-1".into(),
            guard.revision,
            LaneGuardSignal::RealUserInput,
        )
        .unwrap();
    let resumed = claim(&f.h(), 113);
    assert_eq!(resumed.notifications.len(), 1);
    assert!(
        matches!(&resumed.notifications[0].kind, LaneNotificationKind::Question { question_id, .. } if question_id == &question(1, 1).id)
    );
}

#[test]
fn host_origins_skip_lane_receipts_even_when_explicitly_requested() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Summary);
    f.h().start_lane_run("lane-1", 0, "run-1", 2).unwrap();
    let user = user_receipt();
    f.h()
        .accept_lane_user_input("lane-1".into(), user.clone())
        .unwrap();
    let delivery = claim(&f.h(), 10);
    f.h()
        .accept_lane_delivery(delivery.clone(), snapshot(), 11)
        .unwrap();
    let lane_prompt: String = f
        .sql()
        .query_row(
            "SELECT receipt_prompt_id FROM lane_notifications WHERE notification_id=?1",
            [&delivery.notifications[0].notification_id],
            |r| r.get(0),
        )
        .unwrap();
    let origins = f
        .db()
        .host_input_origins(&[
            lane_prompt.clone(),
            user.prompt_id.clone(),
            "missing".into(),
        ])
        .unwrap();
    assert_eq!(origins.len(), 1);
    assert_eq!(origins[0].prompt_id, user.prompt_id);
    assert_eq!(origins[0].text, user.text);
    // Filtering origins does not remove the durable notification receipt itself.
    assert!(matches!(
        f.db().host_input_receipts(&[lane_prompt]).unwrap()[0].provenance,
        storage::HostInputProvenance::LaneNotification { .. }
    ));
}
