//! Samples of the `memory` area (PR 13 reads, PR 14 updates).
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

fn state() -> Value {
    json!({
        "id": "lesson-1", "status": "candidate", "favorite": false, "revision": "3",
        "actions": [
            {"action": "set_favorite", "state": {"type": "allowed"}},
            {"action": "reject", "state": {"type": "allowed"}},
            {"action": "clear_favorite", "state": {"type": "denied", "reason": "not_applicable_in_state"}}
        ]
    })
}

fn command(operation: &str, payload: Value) -> Command {
    de(json!({"type": "memory", "payload": {"type": operation, "payload": payload}}))
}

fn memory_update() -> Value {
    json!({"operation_id": "op-3", "project": "project-1",
           "target": {"id": "lesson-1", "expected_revision": "3"}})
}

fn update_view(state: Value, extra: Value) -> Value {
    let mut view = json!({"operation": {"operation_id": "op-3", "state": state, "revision": "5"},
        "project": "project-1", "id": "lesson-1", "action": "reject"});
    for (key, value) in extra.as_object().unwrap() {
        view[key] = value.clone();
    }
    view
}

fn query(operation: &str, payload: Value) -> Query {
    de(json!({"type": "memory", "payload": {"type": operation, "payload": payload}}))
}

fn reply(kind: &str, payload: Value) -> QueryReply {
    de(json!({"type": "memory", "payload": {"type": kind, "payload": payload}}))
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "memory",
        commands: vec![
            command("set_memory_favorite", memory_update()),
            command("clear_memory_favorite", memory_update()),
            command("validate_memory", memory_update()),
            command("promote_memory", memory_update()),
            command("reject_memory", memory_update()),
        ],
        queries: vec![
            query(
                "list_memory",
                json!({"operation_id": "op-1", "project": "project-1", "status": "promoted",
                       "favorite": true, "text": "retry budget"}),
            ),
            query(
                "get_memory",
                json!({"operation_id": "op-2", "project": "project-1", "id": "lesson-1"}),
            ),
            query("get_memory_operation", json!({"operation_id": "op-1"})),
            query("get_memory_update", json!({"operation_id": "op-3"})),
        ],
        ui_events: vec![
            de::<UiEvent>(json!({"type": "memory", "payload": {
                "type": "memory_operation_changed", "payload": {"view": view(json!({"type": "ready"}))}}})),
            de::<UiEvent>(json!({"type": "memory", "payload": {
                "type": "memory_update_completed", "payload": {"update": update_view(
                    json!({"type": "failed", "code": "conflict"}),
                    json!({"denial": "favorite_must_be_cleared",
                           "message": "the current state does not allow this action"}))}}})),
        ],
        query_replies: vec![
            reply(
                "list",
                json!({"operation": view(json!({"type": "ready"})), "data": {
                    "items": [{"state": state(), "scope": "project", "preview": "Prefer the retry budget"}],
                    "snapshot_revision": "9"}}),
            ),
            reply(
                "detail",
                json!({"operation": view(json!({"type": "ready"})), "data": {
                    "state": state(), "scope": "user", "task_id": "task-1",
                    "content": {"text": "Prefer the retry budget.", "total_bytes": 24, "truncated": false},
                    "evidence": {"text": "run-1", "total_bytes": 5, "truncated": false}}}),
            ),
            reply("operation", view(json!({"type": "pending"}))),
            reply(
                "update",
                update_view(
                    json!({"type": "ready"}),
                    json!({"revision": "6", "changed": true}),
                ),
            ),
        ],
    }
}
