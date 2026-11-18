//! Samples of the `evaluation` area (PR 13 reads, PR 17c jobs).
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

fn command(operation: &str, payload: Value) -> Command {
    de(json!({"type": "evaluation", "payload": {"type": operation, "payload": payload}}))
}

fn event(kind: &str, view: Value) -> UiEvent {
    de(json!({"type": "evaluation", "payload": {"type": kind, "payload": {"view": view}}}))
}

fn job(state: Value, kind: &str, effect: &str, extra: Value) -> Value {
    let mut view = json!({
        "operation": {"operation_id": "op-1", "state": state, "revision": "3"},
        "kind": kind,
        "runs": [{"label": "baseline", "index": 0, "status": "completed", "reason": "",
                  "user_turns": 2, "tokens": 1200, "tool_calls": 5, "elapsed_ms": 3400,
                  "denied_requests": 0, "pushed_refs": 0, "verification_passed": true}],
        "effect": effect
    });
    for (key, value) in extra.as_object().unwrap() {
        view[key] = value.clone();
    }
    view
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
        commands: vec![
            command(
                "start_evaluation_run",
                json!({"operation_id": "op-1", "thread": "thread-1", "candidate": "[agents.worker]\n",
                       "repeat": 2, "output": "run-1", "problem": "it looped", "verify": "cargo test",
                       "verify_timeout_secs": 600, "keep_workspace": false, "report": true}),
            ),
            command(
                "start_evaluation_report",
                json!({"operation_id": "op-2", "targets": ["run-1", "run-2"], "rejudge": true}),
            ),
            command("cancel_evaluation_job", json!({"operation_id": "op-1"})),
        ],
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
            query("get_evaluation_job", json!({"operation_id": "op-1"})),
        ],
        ui_events: vec![
            de::<UiEvent>(json!({"type": "evaluation", "payload": {
                "type": "evaluation_operation_changed", "payload": {"view": view(json!({"type": "ready"}))}}})),
            event(
                "evaluation_job_progress",
                job(
                    json!({"type": "pending"}),
                    "run",
                    "none",
                    json!({"output": "run-1"}),
                ),
            ),
            event(
                "evaluation_job_settled",
                job(
                    json!({"type": "failed", "code": "internal"}),
                    "run",
                    "unknown",
                    json!({"failure": "internal", "message": "the job's task ended without a result"}),
                ),
            ),
        ],
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
            reply(
                "job",
                job(
                    json!({"type": "ready"}),
                    "rejudge",
                    "complete",
                    json!({"reports": ["run-1", "run-2"], "runs": [],
                           "cross_thread": {"text": "| thread |", "total_bytes": 10, "truncated": false}}),
                ),
            ),
        ],
    }
}
