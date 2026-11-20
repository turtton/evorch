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
    let owned = json!({"binding":{"graph":graph(),"owners":[],"expected_control_revision":"0"},"confirmed":true});
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
                json!({"type":"lifecycle","payload":{"type":"owned_graphs","payload":{"type":"release","payload":owned}}}),
            ),
        ],
        queries: vec![
            de(
                json!({"type":"lifecycle","payload":{"type":"operation_status","payload":{"operation_id":"op1"}}}),
            ),
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
                json!({"type":"lifecycle","payload":{"type":"operation_status","payload":{"operation_id":"op1","state":"unknown","outcome":"unknown","audit_warning":true}}}),
            ),
            de(
                json!({"type":"lifecycle","payload":{"type":"restart","payload":{"type":"targets","payload":{"revision":"1","targets":[{"id":"current","revision":"1"}],"truncated":false}}}}),
            ),
            de(
                json!({"type":"lifecycle","payload":{"type":"quarantine","payload":{"type":"list","payload":{"items":[],"revision":"1","truncated":false}}}}),
            ),
            de(
                json!({"type":"lifecycle","payload":{"type":"owned_graphs","payload":{"type":"undisposed","payload":{"count":"0","references":[],"snapshot_revision":"1","truncated":false,"over_limit_count":"0","incomplete_disposals":{"count":"0","references":[],"truncated":false}}}}}),
            ),
        ],
    }
}

/// Nested Q2 variants have stable suffixed names without changing old golden keys.
pub fn extra_goldens() -> Vec<(String, Value)> {
    let mut out = vec![];
    let mut samples = samples();
    samples.queries[0] = de(
        json!({"type":"lifecycle","payload":{"type":"owned_graphs","payload":{"type":"detail","payload":{"graph_ref":"g1"}}}}),
    );
    out.push((
        "query/lifecycle/owned_graphs/detail".into(),
        serde_json::to_value(&samples.queries[0]).unwrap(),
    ));
    samples.queries[0] = de(
        json!({"type":"lifecycle","payload":{"type":"owned_graphs","payload":{"type":"operation","payload":{"operation_id":"op1"}}}}),
    );
    out.push((
        "query/lifecycle/owned_graphs/operation".into(),
        serde_json::to_value(&samples.queries[0]).unwrap(),
    ));
    samples.query_replies[0] = de(
        json!({"type":"lifecycle","payload":{"type":"owned_graphs","payload":{"type":"detail","payload":{"graph_ref":"g1","snapshot_revision":"1","binding":{"graph":{"graph_id":"g1","restart_tx_id":"tx1","capture_id":"cap1","expected_revision":"1","expected_core_epoch":"2","owner_identity":null,"owner_generation":null,"operation_id":"unassigned"},"owners":[],"expected_control_revision":"0"},"restart_phase":"adopted","disposition":"retry_pending","claim":"absent","ownership":"absent","control":"manual_pending","eligibility":"adopted_retry_pending","resume":{"type":"blocked","problem":"current_binding_required","detail":"no_claim"},"release":{"type":"available"},"progress":{"eligibility":"confirmed_applied","prepared_release":"unknown","owners":"not_started","scratch":"not_started","completion":"unknown"},"owner_count":"0","owner_refs":[],"owner_refs_truncated":false,"operation_limit":{"type":"within_limit"}}}}}),
    );
    out.push((
        "query_reply/lifecycle/owned_graphs/detail".into(),
        serde_json::to_value(&samples.query_replies[0]).unwrap(),
    ));
    samples.query_replies[0] = de(
        json!({"type":"lifecycle","payload":{"type":"owned_graphs","payload":{"type":"operation","payload":{"receipt":{"operation_id":"op1","state":"completed","outcome":"failed","audit_warning":false},"graph_ref":"g1","step":"prepared_release","observation":"unknown","progress":{"eligibility":"confirmed_applied","prepared_release":"unknown","owners":"not_started","scratch":"not_started","completion":"unknown"},"next_action":"observe_same_operation"}}}}),
    );
    out.push((
        "query_reply/lifecycle/owned_graphs/operation".into(),
        serde_json::to_value(&samples.query_replies[0]).unwrap(),
    ));
    out
}
