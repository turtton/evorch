//! Samples of the operations the `lanes` area adds beyond those in the parent
//! module.
use super::AreaSamples;
use serde_json::{Value, json};
use workbench_protocol::{
    commands::Command,
    events::UiEvent,
    queries::{Query, QueryReply},
};

fn de<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("sample")
}

fn summary() -> Value {
    json!({
        "lane_id": "lane-1", "thread_id": "thread-2", "lead_thread_id": "thread-1",
        "policy": "attached", "management_revision": "3", "brief_revision": "2",
        "objective_preview": "build the thing", "root_run": "run-9", "generation": "1",
        "start": "started", "last_activity_at": "1700000000000",
        "latest_report_status": "ready", "revision": "11"
    })
}

fn detail() -> Value {
    json!({
        "summary": summary(),
        "latest_report": {"seq": "4", "status": "ready", "summary": "done", "needs_human": false,
            "management_revision": "3", "brief_revision": "1", "created_at": "1700000000001"},
        "ready": {"report_seq": "4", "brief_revision": "1", "brief_changed_since": true,
            "readiness": "unreviewed"},
        "delivery": {"held": true, "pending_count": 2},
        "attention": [{"tag": "stop_pending", "summary": "held", "revision": "1",
            "since": "1700000000002", "available_actions": ["resume"]}]
    })
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "lanes",
        commands: vec![de::<Command>(json!({"type": "lanes", "payload": {
            "type": "resume_lane_deliveries", "payload": {
                "lane_id": "lane-1", "expected_management_revision": "3",
                "lead_thread_id": "thread-1", "expected_lead_root_run_id": "run-1"}}}))],
        queries: vec![
            de::<Query>(
                json!({"type": "lanes", "payload": {"type": "list_lanes", "payload": {
                "lead_thread": "thread-1"}}}),
            ),
            de::<Query>(
                json!({"type": "lanes", "payload": {"type": "get_lane", "payload": {
                "lane_id": "lane-1"}}}),
            ),
        ],
        ui_events: vec![de::<UiEvent>(json!({"type": "lanes", "payload": {
            "type": "lane_changed", "payload": {"lane": summary()}}}))],
        query_replies: vec![
            de::<QueryReply>(
                json!({"type": "lanes", "payload": {"type": "lanes", "payload": {
                "items": [summary()], "snapshot_revision": "9"}}}),
            ),
            de::<QueryReply>(
                json!({"type": "lanes", "payload": {"type": "lane", "payload": detail()}}),
            ),
        ],
    }
}
