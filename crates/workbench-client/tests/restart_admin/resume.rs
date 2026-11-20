use crate::support::*;
use workbench_client::restart::resume::*;
use workbench_protocol::{
    events::{SubscriptionScope, SubscriptionStarted},
    ids::SnapshotId,
    wire::InstanceEpoch,
};

#[test]
fn the_same_epoch_replays_after_the_held_cursor() {
    let held = cursor(7, 42);
    let plan = plan_resume(Some(held), InstanceEpoch(7));
    assert_eq!(plan, ResumePlan::ReplayAfter(held));
    assert_eq!(
        subscribe_request(SubscriptionScope::All, &plan).after,
        Some(held)
    );
}

#[test]
fn a_new_epoch_drops_the_old_cursor_and_asks_for_no_replay() {
    let plan = plan_resume(Some(cursor(7, 42)), InstanceEpoch(8));
    assert_eq!(plan, ResumePlan::SnapshotFirst);
    assert_eq!(subscribe_request(SubscriptionScope::All, &plan).after, None);
    assert_eq!(
        plan_resume(None, InstanceEpoch(8)),
        ResumePlan::SnapshotFirst
    );
}

fn snapshot_started(epoch: u64) -> SubscriptionStarted {
    SubscriptionStarted {
        snapshot_id: Some(SnapshotId::new("snap").unwrap()),
        ..started("s1", cursor(epoch, 1))
    }
}

#[test]
fn a_snapshot_plan_accepts_only_a_snapshot_of_the_connections_epoch() {
    let plan = ResumePlan::SnapshotFirst;
    assert_eq!(
        check_started(&plan, &snapshot_started(8), InstanceEpoch(8)),
        Ok(Started::Snapshot)
    );
    assert_eq!(
        check_started(&plan, &started("s1", cursor(8, 1)), InstanceEpoch(8)),
        Err(ResumeError::ReplayInsteadOfSnapshot)
    );
    assert_eq!(
        check_started(&plan, &snapshot_started(7), InstanceEpoch(8)),
        Err(ResumeError::EpochMismatch {
            expected: 8,
            got: 7
        })
    );
}

#[test]
fn a_replay_plan_may_be_answered_by_a_snapshot_which_replaces_held_state() {
    let plan = ResumePlan::ReplayAfter(cursor(7, 5));
    assert_eq!(
        check_started(&plan, &started("s1", cursor(7, 5)), InstanceEpoch(7)),
        Ok(Started::Replaying)
    );
    assert_eq!(
        check_started(&plan, &snapshot_started(7), InstanceEpoch(7)),
        Ok(Started::Snapshot)
    );
}

#[test]
fn an_event_of_another_epoch_is_not_applied() {
    assert!(event_in_epoch(&cursor(8, 1), InstanceEpoch(8)));
    assert!(!event_in_epoch(&cursor(7, 99), InstanceEpoch(8)));
}
