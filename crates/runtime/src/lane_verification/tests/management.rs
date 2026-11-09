use super::*;
use crate::lane_management::*;

fn request(f: &Fixture, rev: u64, action: LaneManagementAction) -> LaneManagementRequest {
    LaneManagementRequest {
        operation_id: format!("verification-management-{rev}"),
        lane_id: f.lane.lane_id.clone(),
        expected_management_revision: rev,
        action,
    }
}
fn detach(f: &Fixture) {
    f.runtime
        .detach_lane(
            LaneManagementContext::Run(f.lead),
            request(f, 1, LaneManagementAction::Detach),
        )
        .unwrap();
}

#[tokio::test]
async fn detach_and_close_commit_orders_never_make_detach_a_successful_close() {
    for close_first in [false, true] {
        let f = Fixture::new(Mode::Approve).await;
        let v = f.verify().await;
        f.runtime
            .wait(v.reviewer_run_id.parse().unwrap())
            .await
            .unwrap();
        if close_first {
            assert_eq!(f.close(&v).await.unwrap().outcome, LaneCloseOutcome::Closed);
        }
        detach(&f);
        assert!(
            f.storage
                .handle()
                .current_lane_close(f.lane.lane_id.clone())
                .unwrap()
                .is_none()
        );
        assert!(f.close(&v).await.is_err());
        assert_eq!(
            f.runtime
                .verify_lane(f.lead, &f.lane.lane_id, v.target.ready_seq)
                .await
                .unwrap_err()
                .code,
            "not_attached"
        );
        f.cleanup().await;
    }
}

#[tokio::test]
async fn reviewer_completion_after_detach_cannot_apply_to_reattached_management() {
    let f = Fixture::new(Mode::Hold).await;
    let v = f.verify().await;
    f.model.entered.notified().await;
    detach(&f);
    *f.model.mode.lock().unwrap() = Mode::Approve;
    f.model.release.notify_one();
    f.runtime
        .wait(v.reviewer_run_id.parse().unwrap())
        .await
        .unwrap();
    rusqlite::Connection::open(f.dir.path().join("lanes.db"))
        .unwrap()
        .execute("UPDATE threads SET project_id='verification-project'", [])
        .unwrap();
    f.runtime
        .reattach_lane(
            LaneHumanContext,
            request(
                &f,
                2,
                LaneManagementAction::Reattach {
                    lead_thread_id: "lead".into(),
                    expected_lead_root_run_id: f.lead.to_string(),
                },
            ),
            LaneAttachGuard {
                pinned: false,
                archived: false,
            },
        )
        .unwrap();
    assert!(f.close(&v).await.is_err());
    assert!(
        f.runtime
            .verify_lane(f.lead, &f.lane.lane_id, v.target.ready_seq)
            .await
            .is_err()
    );
    assert!(
        f.storage
            .handle()
            .current_lane_close(f.lane.lane_id.clone())
            .unwrap()
            .is_none()
    );
    f.cleanup().await;
}

#[tokio::test]
async fn policy_change_requires_new_ready_even_when_lead_and_artifact_are_unchanged() {
    let f = Fixture::new(Mode::Approve).await;
    let v = f.verify().await;
    f.runtime
        .wait(v.reviewer_run_id.parse().unwrap())
        .await
        .unwrap();
    f.runtime
        .set_lane_policy(
            LaneManagementContext::Run(f.lead),
            request(
                &f,
                1,
                LaneManagementAction::SetPolicy {
                    policy: ManagedLanePolicy::Summary,
                },
            ),
        )
        .unwrap();
    assert!(f.close(&v).await.is_err());
    assert!(
        f.runtime
            .verify_lane(f.lead, &f.lane.lane_id, v.target.ready_seq)
            .await
            .is_err()
    );
    f.cleanup().await;
}

#[tokio::test]
async fn detach_after_close_evidence_reads_but_before_commit_rejects_old_target() {
    let f = Fixture::new(Mode::Approve).await;
    let v = f.verify().await;
    f.runtime
        .wait(v.reviewer_run_id.parse().unwrap())
        .await
        .unwrap();
    let runtime = f.runtime.clone();
    let r = request(&f, 1, LaneManagementAction::Detach);
    let lead = f.lead;
    BEFORE_CLOSE_COMMIT.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            runtime
                .detach_lane(LaneManagementContext::Run(lead), r)
                .unwrap();
        }))
    });
    assert!(f.close(&v).await.is_err());
    assert!(
        f.storage
            .handle()
            .current_lane_close(f.lane.lane_id.clone())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.storage
            .handle()
            .lane(&f.lane.lane_id)
            .unwrap()
            .unwrap()
            .policy,
        LanePolicy::Detached
    );
    f.cleanup().await;
}
