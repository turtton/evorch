//! Only additive management samples: existing wire goldens remain unchanged.
use super::AreaSamples;
use serde_json::{Value, json};
fn de<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}
fn graph() -> Value {
    json!({"graph_id":"g1","restart_tx_id":"tx1","capture_id":"cap1","expected_revision":"1","expected_core_epoch":"2","owner_identity":null,"owner_generation":null,"operation_id":"op1"})
}
pub fn samples() -> AreaSamples {
    let graph = json!({"binding":graph(),"confirmed":true});
    AreaSamples {
        name: "lifecycle",
        commands: vec![
            de(
                json!({"type":"lifecycle","payload":{"type":"restart","payload":{"type":"request","payload":{"binding":{"target_id":"current","expected_core_epoch":"2","target_revision":"1","operation_id":"op1"},"confirmed":true}}}}),
            ),
            de(
                json!({"type":"lifecycle","payload":{"type":"quarantine","payload":{"type":"discard","payload":graph}}}),
            ),
            de(
                json!({"type":"lifecycle","payload":{"type":"owned_graphs","payload":{"type":"release","payload":graph}}}),
            ),
        ],
        queries: vec![
            de(
                json!({"type":"lifecycle","payload":{"type":"restart","payload":{"type":"targets"}}}),
            ),
            de(
                json!({"type":"lifecycle","payload":{"type":"quarantine","payload":{"type":"list"}}}),
            ),
            de(
                json!({"type":"lifecycle","payload":{"type":"owned_graphs","payload":{"type":"undisposed"}}}),
            ),
        ],
        ui_events: vec![de(
            json!({"type":"lifecycle","payload":{"type":"admin_changed","payload":{"revision":"1","restarting":true}}}),
        )],
        query_replies: vec![
            de(
                json!({"type":"lifecycle","payload":{"type":"restart","payload":{"type":"targets","payload":{"revision":"1","targets":[{"id":"current","revision":"1"}],"truncated":false}}}}),
            ),
            de(
                json!({"type":"lifecycle","payload":{"type":"quarantine","payload":{"type":"list","payload":{"items":[],"revision":"1","truncated":false}}}}),
            ),
            de(
                json!({"type":"lifecycle","payload":{"type":"owned_graphs","payload":{"type":"undisposed","payload":{"count":"0","references":[],"snapshot_revision":"1","truncated":false}}}}),
            ),
        ],
    }
}
