//! Lane authority changes must not become PR policy or budget revisions.
use super::*;
use storage::{lane_management::*, lanes::*};

fn manage(f: &Fixture, revision: u64, action: LaneManagementAction) -> LaneManagementResult {
    f.h()
        .apply_lane_management_transition(
            LaneManagementRequest {
                operation_id: format!("manage-{revision}"),
                lane_id: "source-lane".into(),
                expected_management_revision: revision,
                action,
            },
            LaneManagementActor::User,
            Some(LaneAttachGuard {
                pinned: false,
                archived: false,
            }),
            40,
        )
        .unwrap()
}
fn pr_identity(f: &Fixture, id: &str) -> (String, Option<String>, u64, String) {
    f.sql().query_row("SELECT source_kind,lane_id,management_revision,payload FROM lane_notifications WHERE notification_id=?1", [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap()
}
#[test]
fn management_preserves_pr_identity_and_repairs_receipts_after_shared_lease_release() {
    for policy in [
        Some(ManagedLanePolicy::Summary),
        Some(ManagedLanePolicy::Attached),
        None,
    ] {
        for received in [false, true] {
            let f = Fixture::new();
            make_lane(&f);
            let revision = if policy == Some(ManagedLanePolicy::Attached) {
                manage(
                    &f,
                    1,
                    LaneManagementAction::SetPolicy {
                        policy: ManagedLanePolicy::Summary,
                    },
                );
                2
            } else {
                1
            };
            f.policy(PrPolicyAction::SetEnabled(true));
            let notice = f.ready(1, PrNoticeMode::Auto, 10);
            let old = f.claim(10);
            if received {
                f.accept(&old, 11);
            }
            let before = pr_identity(&f, &notice.notice_id);
            let before_notice = f.h().pr_notice(notice.notice_id.clone()).unwrap();
            let state = f.h().pr_auto_fix_state(key()).unwrap();
            let action = policy
                .map(|policy| LaneManagementAction::SetPolicy { policy })
                .unwrap_or(LaneManagementAction::Detach);
            assert!(manage(&f, revision, action).changed);
            assert_eq!(pr_identity(&f, &notice.notice_id), before);
            assert_eq!(
                f.h().pr_notice(notice.notice_id.clone()).unwrap(),
                before_notice
            );
            assert_eq!(before.0, "pull_request_checks_failed");
            assert_eq!(before.1, None);
            assert_eq!(f.h().pr_auto_fix_state(key()).unwrap(), state);
            assert!(
                f.h().ack_pr_delivery(old, 41).is_err(),
                "lane transition fences the shared lease, not PR authority"
            );
            if received {
                assert_eq!(
                    f.h().claim_pr_delivery(request(50)).unwrap(),
                    PrClaimResult::Empty
                );
            } else {
                let current = f.claim(50);
                assert_eq!(current.notifications[0].notification_id, notice.notice_id);
                f.accept(&current, 51);
                f.h().ack_pr_delivery(current, 52).unwrap();
            }
            assert_eq!(f.count("host_input_receipts"), 1);
            assert_eq!(f.h().pr_auto_fix_state(key()).unwrap().used, 1);
            let status: String = f
                .sql()
                .query_row(
                    "SELECT state FROM lane_notifications WHERE notification_id=?1",
                    [&notice.notice_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(status, "acked");
        }
    }
}
#[test]
fn reattach_does_not_apply_lane_management_revision_to_a_pr_claim() {
    let f = Fixture::new();
    make_lane(&f);
    manage(&f, 1, LaneManagementAction::Detach);
    f.sql().execute_batch("UPDATE threads SET project_id='same-project' WHERE thread_id IN ('lead','source'); UPDATE threads SET root_run_id='lead-root',chat_role='Orchestrator' WHERE thread_id='lead';").unwrap();
    f.h().upsert_run_context(&snapshot().record).unwrap();
    f.policy(PrPolicyAction::SetEnabled(true));
    let notice = f.ready(1, PrNoticeMode::Auto, 10);
    let claim = f.claim(10);
    let before = pr_identity(&f, &notice.notice_id);
    let before_notice = f.h().pr_notice(notice.notice_id.clone()).unwrap();
    let result = manage(
        &f,
        2,
        LaneManagementAction::Reattach {
            lead_thread_id: "lead".into(),
            expected_lead_root_run_id: "lead-root".into(),
        },
    );
    assert_eq!(result.lane.management_revision, 3);
    assert_eq!(result.lane.policy, LanePolicy::Summary);
    assert_eq!(pr_identity(&f, &notice.notice_id), before);
    assert_eq!(
        f.h().pr_notice(notice.notice_id.clone()).unwrap(),
        before_notice
    );
    f.accept(&claim, 41);
    f.h().ack_pr_delivery(claim, 42).unwrap();
    assert_eq!(f.h().pr_auto_fix_state(key()).unwrap().used, 1);
    assert_eq!(f.count("host_input_receipts"), 1);
}
#[test]
fn reclaiming_a_detached_lane_does_not_delete_the_pr_source_or_budget() {
    let f = Fixture::new();
    make_lane(&f);
    manage(&f, 1, LaneManagementAction::Detach);
    f.policy(PrPolicyAction::SetEnabled(true));
    let notice = f.ready(1, PrNoticeMode::Auto, 10);
    let before = pr_identity(&f, &notice.notice_id);
    let before_notice = f.h().pr_notice(notice.notice_id.clone()).unwrap();
    f.sql().execute_batch("INSERT INTO thread_tombstones VALUES ('source','lane-only',50); INSERT INTO reclaim_jobs(family_key,state,created_at_ns,updated_at_ns) VALUES ('lane-only','db_pending',50,50);").unwrap();
    loop {
        let progress = f
            .h()
            .reclaim_step(storage::thread_retention::ReclaimBudget::default())
            .unwrap();
        assert_eq!(progress.last_error, None);
        if progress.jobs_db_pending == 0 {
            break;
        }
    }
    assert_eq!(f.count("lanes"), 0);
    assert_eq!(f.count("lane_management_receipts"), 0);
    assert_eq!(pr_identity(&f, &notice.notice_id), before);
    assert_eq!(
        f.h().pr_notice(notice.notice_id.clone()).unwrap(),
        before_notice
    );
    let claim = f.claim(60);
    f.accept(&claim, 61);
    f.h().ack_pr_delivery(claim, 62).unwrap();
    assert_eq!(f.h().pr_auto_fix_state(key()).unwrap().used, 1);
}

#[test]
fn lane_management_does_not_fence_an_in_flight_pr_preparation() {
    for action in [
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
        LaneManagementAction::Detach,
    ] {
        let f = Fixture::new();
        make_lane(&f);
        f.policy(PrPolicyAction::SetEnabled(true));
        let reservation = f.reserve(1, PrNoticeMode::Auto, 10);
        let before = f.h().pr_notice(reservation.notice_id.clone()).unwrap();
        manage(&f, 1, action);
        assert_eq!(
            f.h().pr_notice(reservation.notice_id.clone()).unwrap(),
            before
        );
        let ready = f
            .h()
            .ready_pr_notice_if_current(reservation.clone(), "prepared facts".into())
            .unwrap()
            .unwrap();
        assert_eq!(ready.reservation_revision, reservation.reservation_revision);
    }
}
