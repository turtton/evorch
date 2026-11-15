//! Samples of the `improvement` area (PR 13, read side).
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

fn summary() -> Value {
    json!({
        "state": {"id": "cand-1", "status": "new", "revision": "4", "actions": [
            {"action": "mark_reviewed", "state": {"type": "allowed"}},
            {"action": "dismiss", "state": {"type": "allowed"}},
            {"action": "triage", "state": {"type": "denied", "reason": "read_only"}}]},
        "source": "diagnostic", "severity": "warning", "code": "storage.busy",
        "title": "Reduce writer contention", "occurrences": 3, "last_seen_ms": 1700000000000u64
    })
}

fn query(operation: &str, payload: Value) -> Query {
    de(json!({"type": "improvement", "payload": {"type": operation, "payload": payload}}))
}

fn reply(kind: &str, payload: Value) -> QueryReply {
    de(json!({"type": "improvement", "payload": {"type": kind, "payload": payload}}))
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "improvement",
        commands: Vec::<Command>::new(),
        queries: vec![
            query(
                "list_improvement",
                json!({"operation_id": "op-1", "project": "project-1", "status": "new"}),
            ),
            query(
                "get_improvement",
                json!({"operation_id": "op-2", "project": "project-1", "candidate": "cand-1"}),
            ),
            query("get_improvement_operation", json!({"operation_id": "op-1"})),
        ],
        ui_events: vec![de::<UiEvent>(json!({"type": "improvement", "payload": {
            "type": "improvement_operation_changed", "payload": {"view": view(json!({"type": "ready"}))}}}))],
        query_replies: vec![
            reply(
                "list",
                json!({"operation": view(json!({"type": "ready"})), "data": {
                    "enabled": true,
                    "candidates": {"items": [summary()], "snapshot_revision": "9"}}}),
            ),
            reply(
                "detail",
                json!({"operation": view(json!({"type": "ready"})), "data": {
                    "summary": summary(),
                    "evidence": {"text": "{\"busy\":true}", "total_bytes": 13, "truncated": false},
                    "draft_file": "cand-1.md", "run": "run-1", "recent_runs": ["run-1", "run-2"],
                    "created_ms": 1699999000000u64, "reopened_count": 0}}),
            ),
            reply(
                "operation",
                view(json!({"type": "failed", "code": "storage_unavailable"})),
            ),
        ],
    }
}
