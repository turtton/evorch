use providers::{ChatRequest, wire};
use serde_json::{Value, json};

fn request(tier: Option<&str>) -> ChatRequest {
    let mut input = json!({"model": "gpt-test", "messages": []});
    if let Some(tier) = tier {
        input["service_tier"] = json!(tier);
    }
    serde_json::from_value(input).expect("canonical request")
}

#[test]
fn canonical_service_tier_round_trip() {
    // Given: priority または未指定のcanonical request。
    for tier in [Some("priority"), None] {
        let request = request(tier);
        // When: JSON化して復元する。
        let value = serde_json::to_value(&request).expect("serialize");
        let restored: ChatRequest = serde_json::from_value(value.clone()).expect("deserialize");
        // Then: 指定時だけpriorityがあり、往復で保持される。
        assert_eq!(value.get("service_tier"), tier.map(Value::from).as_ref());
        assert_eq!(restored, request);
    }
}

#[test]
fn codex_service_tier_is_optional() {
    // Given: priority または未指定 / When: Codexへ変換 / Then: 指定時だけ送信する。
    for tier in [Some("priority"), None] {
        let value =
            serde_json::to_value(wire::codex::to_wire_request(&request(tier))).expect("wire");
        assert_eq!(value.get("service_tier"), tier.map(Value::from).as_ref());
        assert!(value.get("reasoning").is_some());
    }
}

#[test]
fn openai_service_tier_is_optional() {
    // Given: priority または未指定 / When: OpenAIへ変換 / Then: 指定時だけ送信する。
    for tier in [Some("priority"), None] {
        let value = serde_json::to_value(wire::openai::to_wire_request(&request(tier), false))
            .expect("wire");
        assert_eq!(value.get("service_tier"), tier.map(Value::from).as_ref());
    }
}

#[test]
fn anthropic_ignores_service_tier() {
    // Given: priority指定 / When: Anthropicへ変換 / Then: 標準と同一のwire本文。
    assert_eq!(
        wire::anthropic::to_wire_request(&request(Some("priority")), false),
        wire::anthropic::to_wire_request(&request(None), false),
    );
}
