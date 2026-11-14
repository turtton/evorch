//! Samples of the operations the `teams` area adds beyond those in the parent
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

fn overview() -> Value {
    json!({
        "thread": "thread-1", "source": "persisted", "ready": 1, "claimed": 1, "complete": 0,
        "members": [{"owner": "worker-1", "run": "run-2", "claimed_tasks": 1}],
        "revision": "5"
    })
}

fn task() -> Value {
    json!({"task_id": "task-1", "state": "claimed", "paths": ["src/lib.rs"],
           "owner": "worker-1", "owner_run": "run-2", "claim_generation": "1", "revision": "6"})
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "teams",
        commands: Vec::<Command>::new(),
        queries: vec![
            de::<Query>(
                json!({"type": "teams", "payload": {"type": "get_team", "payload": {"thread": "thread-1"}}}),
            ),
            de::<Query>(
                json!({"type": "teams", "payload": {"type": "list_team_tasks", "payload": {"thread": "thread-1"}}}),
            ),
            de::<Query>(
                json!({"type": "teams", "payload": {"type": "get_team_task", "payload": {
                "thread": "thread-1", "task_id": "task-1"}}}),
            ),
        ],
        ui_events: vec![de::<UiEvent>(json!({"type": "teams", "payload": {
            "type": "team_changed", "payload": {"team": overview()}}}))],
        query_replies: vec![
            de::<QueryReply>(
                json!({"type": "teams", "payload": {"type": "team", "payload": overview()}}),
            ),
            de::<QueryReply>(
                json!({"type": "teams", "payload": {"type": "tasks", "payload": {
                "items": [task()], "snapshot_revision": "9"}}}),
            ),
            de::<QueryReply>(
                json!({"type": "teams", "payload": {"type": "task", "payload": task()}}),
            ),
        ],
    }
}
