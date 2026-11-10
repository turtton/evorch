use super::*;
use event_bus::{LaneBrief, LanePolicy, LaneReportInput, LaneReportProducer, LaneStatus};
use storage::lane_management::*;

#[test]
fn uncollected_presentation_requires_current_full_terminal_report_not_a_reference() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("presentation.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config).unwrap();
    let handle = storage.handle();
    let brief = LaneBrief {
        objective: "artifact".into(),
        inputs: vec![],
        deliverables: vec!["artifact".into()],
        acceptance_criteria: vec!["review".into()],
        constraints: vec![],
        workspace: Default::default(),
        report_policy: LanePolicy::Attached,
        verification_requirements: Default::default(),
    };
    handle
        .create_lane(storage::lanes::NewLane {
            lane_id: "lane".into(),
            thread_id: "thread-lane".into(),
            origin_thread_id: "lead".into(),
            origin_run_id: "root".into(),
            creation_key: "key".into(),
            brief,
            start_seed: "seed".into(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns: 1,
        })
        .unwrap();
    let report = |status| LaneReportInput {
        status,
        summary: "artifact".into(),
        artifact_revision: None,
        failure_episode: None,
        artifact_refs: vec!["artifact:one".into()],
        needs_human: false,
        question_refs: vec![],
        reason_code: None,
        producer: LaneReportProducer::LaneAgent,
    };
    let ready = handle
        .append_lane_report("lane", 1, report(LaneStatus::Ready), 2)
        .unwrap()
        .report;
    let page = serde_json::json!({"items":[{"latest_report":ready}]}).to_string();
    let delivery = format!(
        "[Agent-authored notifications]\n{}\n",
        serde_json::json!({"report":ready})
    );
    assert!(presentation_is_current(&handle, "lead", "lane", &page).unwrap());
    assert!(presentation_is_current(&handle, "lead", "lane", &delivery).unwrap());
    assert!(!presentation_is_current(&handle, "other", "lane", &page).unwrap());
    assert!(
        !presentation_is_current(
            &handle,
            "lead",
            "lane",
            r#"{"notification":{"lane_id":"lane","report_seq":1}}"#
        )
        .unwrap()
    );
    handle
        .append_lane_report("lane", 1, report(LaneStatus::Working), 3)
        .unwrap();
    assert!(!presentation_is_current(&handle, "lead", "lane", &page).unwrap());
    handle
        .append_lane_report("lane", 1, report(LaneStatus::Ready), 4)
        .unwrap();
    handle
        .apply_lane_management_transition(
            LaneManagementRequest {
                operation_id: "policy".into(),
                lane_id: "lane".into(),
                expected_management_revision: 1,
                action: LaneManagementAction::SetPolicy {
                    policy: ManagedLanePolicy::Summary,
                },
            },
            LaneManagementActor::User,
            None,
            5,
        )
        .unwrap();
    // Restored pending input from an older management revision earns no credit.
    assert!(!presentation_is_current(&handle, "lead", "lane", &page).unwrap());
    let current = handle
        .append_lane_report("lane", 2, report(LaneStatus::Ready), 6)
        .unwrap()
        .report;
    let current = serde_json::json!({"items":[{"latest_report":current}]}).to_string();
    assert!(presentation_is_current(&handle, "lead", "lane", &current).unwrap());
    handle
        .apply_lane_management_transition(
            LaneManagementRequest {
                operation_id: "detach".into(),
                lane_id: "lane".into(),
                expected_management_revision: 2,
                action: LaneManagementAction::Detach,
            },
            LaneManagementActor::User,
            None,
            7,
        )
        .unwrap();
    assert!(!presentation_is_current(&handle, "lead", "lane", &current).unwrap());
}
