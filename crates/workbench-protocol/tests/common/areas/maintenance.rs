//! Samples of the `maintenance` area (stage 17d).
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

fn command(operation: &str, payload: Value) -> Command {
    de(json!({"type": "maintenance", "payload": {"type": operation, "payload": payload}}))
}

fn query(operation: &str, payload: Value) -> Query {
    de(json!({"type": "maintenance", "payload": {"type": operation, "payload": payload}}))
}

fn reply(kind: &str, payload: Value) -> QueryReply {
    de(json!({"type": "maintenance", "payload": {"type": kind, "payload": payload}}))
}

fn event(kind: &str, view: Value) -> UiEvent {
    de(json!({"type": "maintenance", "payload": {"type": kind, "payload": {"view": view}}}))
}

fn backfill(state: Value, batches: u32, effect: &str, extra: Value) -> Value {
    let mut view = json!({
        "operation": {"operation_id": "op-1", "state": state, "revision": "3"},
        "task": "backfill_events", "batches": batches,
        "progress": {"type": "backfill_events", "payload": {
            "converted": 3000, "reattributed": 0, "undecodable": 2,
            "remaining_unconverted": 7000, "remaining_unattributed": 0}},
        "effect": effect
    });
    for (key, value) in extra.as_object().unwrap() {
        view[key] = value.clone();
    }
    view
}

fn reindex(state: Value, batches: u32, effect: &str, progress: Value, extra: Value) -> Value {
    let mut view = json!({
        "operation": {"operation_id": "op-2", "state": state, "revision": "4"},
        "task": "reindex_threads", "batches": batches,
        "progress": {"type": "reindex_threads", "payload": progress},
        "effect": effect
    });
    for (key, value) in extra.as_object().unwrap() {
        view[key] = value.clone();
    }
    view
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "maintenance",
        commands: vec![
            command(
                "start_maintenance",
                json!({"operation_id": "op-1", "task": {"type": "backfill_events",
                       "payload": {"batch_rows": 1000, "max_rows": 10000}}}),
            ),
            command("cancel_maintenance", json!({"operation_id": "op-1"})),
        ],
        queries: vec![
            query("get_maintenance", json!({"operation_id": "op-1"})),
            query("list_maintenance", json!({"page": {"limit": 20}})),
        ],
        ui_events: vec![
            event(
                "maintenance_progress",
                backfill(json!({"type": "pending"}), 3, "partial", json!({})),
            ),
            event(
                "maintenance_settled",
                backfill(
                    json!({"type": "cancelled", "reason": "requested"}),
                    3,
                    "partial",
                    json!({"message": "cancelled after 3 committed batches"}),
                ),
            ),
        ],
        query_replies: vec![
            reply(
                "operation",
                backfill(
                    json!({"type": "ready"}),
                    4,
                    "complete",
                    json!({"outcome": "completed"}),
                ),
            ),
            reply(
                "list",
                json!({"items": [
                    reindex(
                    json!({"type": "ready"}),
                    7,
                    "complete",
                    json!({"request_id": "req-1",
                           "summary": {"target_count": 3, "succeeded": 2,
                                       "deleted_before_submission": 0,
                                       "gaps": [{"reason": {"reason": "blocked",
                                                 "detail": {"reason": "extraction",
                                                            "detail": {"reason": "provenance_unknown",
                                                                       "detail": "legacy_unproven"}}},
                                                 "count": 1}]},
                           "result": {"status": "completed_with_gaps", "request_id": "req-1",
                                      "summary": {"target_count": 3, "succeeded": 2,
                                                  "deleted_before_submission": 0,
                                                  "gaps": [{"reason": {"reason": "capacity_paused"}, "count": 1}]}}}),
                    json!({"outcome": "completed"}),
                ),
                    backfill(json!({"type": "pending"}), 0, "none",
                             json!({"progress": {"type": "backfill_events", "payload": {
                                 "converted": 0, "reattributed": 0, "undecodable": 0}}})),
                    backfill(json!({"type": "failed", "code": "storage_unavailable"}), 3,
                             "unknown",
                             json!({"message": "a batch failed and may have committed part of its work; read the state again"}))],
                    "snapshot_revision": "5"}),
            ),
        ],
    }
}
