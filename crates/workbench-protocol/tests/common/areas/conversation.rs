//! Samples of the operations the `conversation` area adds beyond those in the parent module.
use super::AreaSamples;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use workbench_protocol::queries::{Query, QueryReply};

fn de<T: DeserializeOwned>(wire: Value) -> T {
    serde_json::from_value(wire).unwrap()
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "conversation",
        commands: vec![],
        queries: vec![de::<Query>(json!({"type": "conversation", "payload": {
            "type": "get_input_draft", "payload": {
                "thread": "thread-1", "submission_id": "sub-a"}}}))],
        ui_events: vec![],
        query_replies: vec![de::<QueryReply>(
            json!({"type": "conversation", "payload": {
            "type": "input_draft", "payload": {
                "thread": "thread-1", "submission_id": "sub-a", "run": "run-1",
                "state": "dropped", "text": "look at these", "image_count": 2,
                "attachment_count": 3}}}),
        )],
    }
}
