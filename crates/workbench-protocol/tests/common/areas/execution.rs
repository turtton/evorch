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

fn plan_progress() -> Value {
    json!({"node_count":1,"leaf_count":1,"leaf_counts":{"pending":1,"in_progress":0,"completed":0,"blocked":0,"interrupted":0},"weighted_ppm":{"pending":1000000,"in_progress":0,"completed":0,"blocked":0,"interrupted":0}})
}
fn plan_node() -> Value {
    json!({"task_id":"task-1","parent_id":null,"position":0,"depth":1,"title":"test","weight":1,"status":"pending","child_count":0,"progress_ppm":plan_progress()["weighted_ppm"]})
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "execution",
        commands: Vec::<Command>::new(),
        queries: vec![
            de::<Query>(
                json!({"type":"execution","payload":{"type":"get_conversation_plan","payload":{"thread":"thread-1"}}}),
            ),
            de::<Query>(
                json!({"type":"execution","payload":{"type":"read_conversation_plan_tree","payload":{"thread":"thread-1","limit":1}}}),
            ),
            de::<Query>(
                json!({"type":"execution","payload":{"type":"get_conversation_plan_task","payload":{"thread":"thread-1","task_id":"task-1"}}}),
            ),
            de::<Query>(
                json!({"type":"execution","payload":{"type":"read_conversation_plan_history","payload":{"thread":"thread-1","limit":1}}}),
            ),
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
        ui_events: vec![
            de::<UiEvent>(
                json!({"type":"execution","payload":{"type":"conversation_plan_changed","payload":{"thread":"thread-1","revision":"1"}}}),
            ),
            de::<UiEvent>(json!({"type": "execution", "payload": {
            "type": "thread_execution_changed", "payload": {
                "thread": "thread-1", "goal": goal(), "todo": todo()}}})),
        ],
        query_replies: vec![
            de::<QueryReply>(
                json!({"type":"execution","payload":{"type":"conversation_plan","payload":json!({"thread":"thread-1","plan_id":"plan-1","revision":"1","updated_at_unix_ms":1_700_000_000_123i64,"progress":plan_progress()})}}),
            ),
            de::<QueryReply>(
                json!({"type":"execution","payload":{"type":"conversation_plan_tree","payload":json!({"thread":"thread-1","plan_id":"plan-1","revision":"1","progress":plan_progress(),"nodes":[plan_node()],"next_cursor":null})}}),
            ),
            de::<QueryReply>(
                json!({"type":"execution","payload":{"type":"conversation_plan_task","payload":json!({"thread":"thread-1","plan_id":"plan-1","revision":"1","node":{"task_id":"task-1","parent_id":null,"position":0,"depth":1,"title":"test","description":"detail","result":"","blocker":"","weight":1,"status":"pending","child_ids":[],"progress_ppm":plan_progress()["weighted_ppm"]}})}}),
            ),
            de::<QueryReply>(
                json!({"type":"execution","payload":{"type":"conversation_plan_history","payload":json!({"thread":"thread-1","plan_id":"plan-1","revision":"1","entries":[{"event_id":"7","revision":"1","updated_at_unix_ms":1_700_000_000_123i64,"change":{"cause":"model_update","source_run_id":"run-1","changes":[{"change":"cleared","removed":0}],"progress":plan_progress()}}],"next_after_revision":null})}}),
            ),
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
