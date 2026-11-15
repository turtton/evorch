//! Samples of the `diagnostics` area (PR 13, read side).
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

fn totals() -> Value {
    json!({"requests": 3, "failed": 1, "input_tokens": 1000, "output_tokens": 200,
           "cache_read_tokens": 800, "cache_write_tokens": 100, "reasoning_tokens": 50,
           "recorded_cost_micro_usd": 12500, "unpriced_tokens": 0, "unreported_requests": 0})
}

fn query(operation: &str, payload: Value) -> Query {
    de(json!({"type": "diagnostics", "payload": {"type": operation, "payload": payload}}))
}

fn reply(kind: &str, payload: Value) -> QueryReply {
    de(json!({"type": "diagnostics", "payload": {"type": kind, "payload": payload}}))
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "diagnostics",
        commands: Vec::<Command>::new(),
        queries: vec![
            query(
                "get_usage_summary",
                json!({"operation_id": "op-1", "range": "last7_days"}),
            ),
            query(
                "get_run_context",
                json!({"operation_id": "op-2", "run": "run-1"}),
            ),
            query("get_diagnostics_operation", json!({"operation_id": "op-1"})),
        ],
        ui_events: vec![de::<UiEvent>(json!({"type": "diagnostics", "payload": {
            "type": "diagnostics_operation_changed", "payload": {"view": view(json!({"type": "ready"}))}}}))],
        query_replies: vec![
            reply(
                "usage",
                json!({"operation": view(json!({"type": "ready"})), "data": {
                    "range": "last7_days", "from_day": "2026-01-01", "to_day": "2026-01-07",
                    "totals": totals(),
                    "days": [{"day": "2026-01-07", "totals": totals()}], "days_omitted": 0,
                    "models": [{"provider": "openai", "model": "gpt", "totals": totals()}],
                    "models_omitted": 0}}),
            ),
            reply(
                "context",
                json!({"operation": view(json!({"type": "ready"})), "data": {
                    "overview": {"run": "run-1", "role": "worker", "phase": "checkpoint",
                                 "selected_model": "openai/gpt", "updated_at_ms": 1700000000000u64,
                                 "tools": ["read"], "tools_omitted": 0, "message_count": 2,
                                 "compaction_checkpoints": 0,
                                 "segments": [{"label": "user", "tokens": 20}]},
                    "messages": {"items": [
                        {"index": 0, "role": "user", "summary": "hello", "tokens": 20, "listed": true}],
                        "snapshot_revision": "3"}}}),
            ),
            reply("operation", view(json!({"type": "pending"}))),
        ],
    }
}
