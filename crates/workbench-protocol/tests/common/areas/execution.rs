//! Samples of the operations the `execution` area adds beyond those in the parent
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

fn run_detail() -> Value {
    json!({
        "summary": {"id": "run-1", "thread": "thread-1", "role": "orchestrator", "phase": "running", "revision": "1"},
        "name": "orchestrator",
        "lifecycle": "active",
        "source": "live",
        "model": "model-1",
        "child_count": 2,
        "revision": "7"
    })
}

fn goal() -> Value {
    json!({
        "goal": "goal-1", "root_run": "run-1", "objective": "ship it",
        "criteria": ["tests pass"],
        "checks": [{"criterion": 0, "met": true, "evidence": "ci green"}],
        "phase": "reviewing", "review_enabled": true, "checks_paused": false,
        "work_stopped": false, "review_round": 1, "max_review_rounds": 3,
        "findings": [], "epoch": "4"
    })
}

fn todo() -> Value {
    json!({"list_id": "list-1", "revision": "2",
           "items": [{"content": "write tests", "status": "in_progress"}]})
}

fn page(items: Vec<Value>) -> Value {
    json!({"items": items, "snapshot_revision": "9"})
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "execution",
        commands: Vec::<Command>::new(),
        queries: vec![
            de::<Query>(
                json!({"type": "execution", "payload": {"type": "list_run_details", "payload": {
                "thread": "thread-1", "scope": {"type": "roots"}}}}),
            ),
            de::<Query>(
                json!({"type": "execution", "payload": {"type": "get_run", "payload": {
                "thread": "thread-1", "run": "run-1"}}}),
            ),
            de::<Query>(
                json!({"type": "execution", "payload": {"type": "get_thread_execution", "payload": {
                "thread": "thread-1"}}}),
            ),
        ],
        ui_events: vec![de::<UiEvent>(json!({"type": "execution", "payload": {
            "type": "thread_execution_changed", "payload": {
                "thread": "thread-1", "goal": goal(), "todo": todo()}}}))],
        query_replies: vec![
            de::<QueryReply>(
                json!({"type": "execution", "payload": {"type": "run_details", "payload": page(vec![run_detail()])}}),
            ),
            de::<QueryReply>(
                json!({"type": "execution", "payload": {"type": "run", "payload": run_detail()}}),
            ),
            de::<QueryReply>(
                json!({"type": "execution", "payload": {"type": "thread_execution", "payload": {
                "thread": "thread-1", "root_run": "run-1", "goal": goal(), "todo": todo(),
                "ledger": {"run": "run-1", "seq": "3", "body": "notes", "truncated": false}}}}),
            ),
        ],
    }
}
