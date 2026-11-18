//! Exact wire spelling and readback of the new process retention reason.
use workbench_protocol::handshake::{ActivityCounts, CoreState, CoreStatus, RetentionReason};
#[test]
fn pending_reindex_status_matches_the_wire_golden_and_round_trips() {
    let golden: serde_json::Value = serde_json::from_str(r#"{"state":"ready","retention_reasons":["pending_reindex"],"running":{"turns":0,"tool_calls":0,"waiting_lanes":0,"shell_jobs":0,"clients":0}}"#).unwrap();
    let status = CoreStatus {
        state: CoreState::Ready,
        retention_reasons: vec![RetentionReason::PendingReindex],
        running: ActivityCounts::default(),
    };
    assert_eq!(serde_json::to_value(&status).unwrap(), golden);
    assert_eq!(
        serde_json::from_value::<CoreStatus>(golden).unwrap(),
        status
    );
}
