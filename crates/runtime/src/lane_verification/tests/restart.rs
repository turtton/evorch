//! Closing/discarding changes durable management state, not just presentation.
use super::*;
use crate::lane_close::HumanLaneCloseRequest;
use crate::lane_management::LaneHumanContext;

#[tokio::test]
async fn closes_are_effect_free_while_preparing_or_sealed_and_require_explicit_retry() {
    for sealed in [false, true] {
        // Lead close, trusted human close, and human discard share the boundary.
        for kind in 0..3 {
            let f = Fixture::new(Mode::Approve).await;
            let v = f.verify().await;
            f.runtime
                .wait(v.reviewer_run_id.parse().unwrap())
                .await
                .unwrap();
            let lane = f.storage.handle().lane(&f.lane.lane_id).unwrap().unwrap();
            let close = || {
                if kind == 0 {
                    f.runtime
                        .close_lane(f.lead, f.close_request(Some(&v), &v.target))
                } else {
                    f.runtime.close_lane_from_host(
                        LaneHumanContext,
                        HumanLaneCloseRequest {
                            lane_id: f.lane.lane_id.clone(),
                            ready_seq: Some(v.target.ready_seq),
                            expected_management_revision: v.target.management_revision,
                            expected_request_revision: v.target.request_revision,
                            discard: kind == 2,
                            note: "explicit human decision".into(),
                            surface: "test-host".into(),
                            client_id: None,
                        },
                        &[],
                    )
                }
            };
            let attempt = f.runtime.begin_restart_quiesce().unwrap();
            if sealed {
                let fence = f.runtime.fence_dispatch_restart().await.unwrap();
                f.runtime.drain_dispatch_tails(&fence).await.unwrap();
            }
            assert_eq!(close().unwrap_err().code, "restart_busy");
            assert_eq!(
                f.storage.handle().lane(&f.lane.lane_id).unwrap().unwrap(),
                lane
            );
            assert!(
                f.storage
                    .handle()
                    .current_lane_close(f.lane.lane_id.clone())
                    .unwrap()
                    .is_none()
            );
            f.runtime.cancel_restart_quiesce(&attempt).unwrap();
            assert!(
                f.storage
                    .handle()
                    .current_lane_close(f.lane.lane_id.clone())
                    .unwrap()
                    .is_none()
            );
            assert_eq!(close().unwrap().outcome, LaneCloseOutcome::Closed);
            f.cleanup().await;
        }
    }
}

#[tokio::test]
async fn storage_health_failed_before_request_keeps_close_and_reattach_available() {
    use crate::lane_management::{
        LaneAttachGuard, LaneManagementAction, LaneManagementContext, LaneManagementRequest,
    };
    let f = Fixture::new(Mode::Approve).await;
    let v = f.verify().await;
    f.runtime
        .wait(v.reviewer_run_id.parse().unwrap())
        .await
        .unwrap();
    f.runtime.shared.quiesce.fail("previous storage failure");
    let failed = f.runtime.quiesce_state();
    assert!(f.runtime.begin_restart_quiesce().is_err());
    assert!(f.runtime.shared.quiesce.running());
    assert_eq!(f.close(&v).await.unwrap().outcome, LaneCloseOutcome::Closed);
    let request = |revision, action| LaneManagementRequest {
        operation_id: format!("health-management-{revision}"),
        lane_id: f.lane.lane_id.clone(),
        expected_management_revision: revision,
        action,
    };
    f.runtime
        .detach_lane(
            LaneManagementContext::Run(f.lead),
            request(1, LaneManagementAction::Detach),
        )
        .unwrap();
    rusqlite::Connection::open(f.dir.path().join("lanes.db"))
        .unwrap()
        .execute("UPDATE threads SET project_id='verification-project'", [])
        .unwrap();
    f.runtime
        .reattach_lane(
            LaneHumanContext,
            request(
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
    assert_eq!(
        f.storage
            .handle()
            .lane(&f.lane.lane_id)
            .unwrap()
            .unwrap()
            .management_revision,
        3
    );
    assert_eq!(f.runtime.quiesce_state(), failed);
    assert!(f.runtime.shared.quiesce.running());
    f.cleanup().await;
}
