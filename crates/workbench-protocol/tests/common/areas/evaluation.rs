//! Samples of the `evaluation` area (PR 13, read side).
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

fn view(state: Value) -> Value {
    json!({"operation_id": "op-1", "state": state, "revision": "2"})
}

fn artifact(kind: &str, presence: &str) -> Value {
    json!({"group": "20260101-000000", "member": "member-0", "kind": kind, "presence": presence})
}

fn query(operation: &str, payload: Value) -> Query {
    de(json!({"type": "evaluation", "payload": {"type": operation, "payload": payload}}))
}

fn reply(kind: &str, payload: Value) -> QueryReply {
    de(json!({"type": "evaluation", "payload": {"type": kind, "payload": payload}}))
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "evaluation",
        commands: Vec::<Command>::new(),
        queries: vec![
            query("list_evaluations", json!({"operation_id": "op-1"})),
            query(
                "get_evaluation",
                json!({"operation_id": "op-2", "group": "20260101-000000"}),
            ),
            query(
                "get_evaluation_report",
                json!({"operation_id": "op-3", "group": "20260101-000000", "member": "member-0"}),
            ),
            query("get_evaluation_operation", json!({"operation_id": "op-1"})),
        ],
        ui_events: vec![de::<UiEvent>(json!({"type": "evaluation", "payload": {
            "type": "evaluation_operation_changed", "payload": {"view": view(json!({"type": "ready"}))}}}))],
        query_replies: vec![
            reply(
                "list",
                json!({"operation": view(json!({"type": "ready"})), "data": {
                    "groups": {"items": [
                        {"group": "20260101-000000", "presence": "present", "phase": "finished",
                         "started_at": "2026-01-01T00:00:00Z", "members_total": 1, "revision": "5"},
                        {"group": "20251231-000000", "presence": "problem", "problem": "malformed",
                         "revision": "6"}],
                     "snapshot_revision": "9"},
                    "omitted": 0, "scan_truncated": false}}),
            ),
            reply(
                "group",
                json!({"operation": view(json!({"type": "ready"})), "data": {
                    "group": "20260101-000000", "phase": "running",
                    "started_at": "2026-01-01T00:00:00Z",
                    "members": [{
                        "member": "member-0", "index": 0, "thread_id": "thread-1",
                        "phase": "running_trials",
                        "trials": {"planned": 4, "queued": 1, "running": 1, "finished": 2,
                                   "failed": 0, "interrupted": 0, "unavailable": 0, "unverified": 0},
                        "report": artifact("report", "missing"),
                        "replay": artifact("replay", "present"),
                        "problems": 0}],
                    "members_omitted": 0, "members_total": 1, "revision": "7"}}),
            ),
            reply(
                "report",
                json!({"operation": view(json!({"type": "ready"})), "data": {
                    "artifact": artifact("report", "present"),
                    "text": {"text": "# Report", "total_bytes": 8, "truncated": false}}}),
            ),
            reply(
                "operation",
                view(json!({"type": "cancelled", "reason": "shutdown"})),
            ),
        ],
    }
}
