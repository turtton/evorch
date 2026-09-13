use event_bus::ToolEvent;
use serde_json::json;

#[test]
fn approval_input_round_trips_when_present_or_absent() {
    // Given: 引数付きと引数なしの承認要求。
    for input in [json!({"command": "pwd"}), serde_json::Value::Null] {
        let wire = json!({
            "kind": "ApprovalRequested",
            "payload": {"tool_name": "shell", "call_id": "c1", "input": input}
        });
        // When: wire を復元して再シリアライズする。
        let event: ToolEvent = serde_json::from_value(wire.clone()).expect("deserialize");
        let restored = serde_json::to_value(event).expect("serialize");
        // Then: input の有無を含めて保持する。
        assert_eq!(restored, wire);
    }
}

#[test]
fn approval_input_defaults_when_legacy_event_omits_field() {
    // Given: input フィールドのない旧イベント。
    let wire = json!({
        "kind": "ApprovalRequested",
        "payload": {"tool_name": "shell", "call_id": "c1"}
    });
    // When: 現在の型へ復元する。
    let event: ToolEvent = serde_json::from_value(wire).expect("legacy deserialize");
    // Then: 引数なしとして再シリアライズできる。
    let restored = serde_json::to_value(event).expect("serialize");
    assert_eq!(
        restored["payload"].get("input"),
        Some(&serde_json::Value::Null)
    );
}
