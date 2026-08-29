use super::super::*;
use crate::message::{ContentBlock, FinishReason, Role, Usage};
use serde_json::json;

// Given: content・tool_calls・cached_tokens を含む wire response / When: canonical response に変換 / Then: 全値と OpenAI 固有 usage が対応する
#[test]
fn wire_response_maps_to_canonical_response() {
    let wire: WireChatResponse = serde_json::from_value(json!({
        "id": "chatcmpl-1",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "answer",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "weather", "arguments": "{\"city\":\"Tokyo\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": 4,
            "total_tokens": 16,
            "prompt_tokens_details": {"cached_tokens": 7}
        }
    }))
    .expect("fixture must deserialize");

    let response = from_wire_response(&wire).expect("wire response must convert");

    assert_eq!(response.message.role, Role::Assistant);
    assert_eq!(
        response.message.content,
        vec![
            ContentBlock::Text {
                text: "answer".to_string()
            },
            ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "weather".to_string(),
                input: json!({"city": "Tokyo"}),
            }
        ]
    );
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 12,
            output_tokens: 4,
            cache_read_tokens: 7,
            cache_write_tokens: 0,
        }
    );
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
}

// Given: 既知値と未知値の finish_reason / When: canonical 値へ変換 / Then: 既知値は専用 variant、未知値は Other になる
#[test]
fn finish_reason_mapping_is_exhaustive_for_known_values() {
    let cases = [
        ("stop", FinishReason::Stop),
        ("length", FinishReason::Length),
        ("tool_calls", FinishReason::ToolUse),
        ("content_filter", FinishReason::ContentFilter),
        (
            "function_call",
            FinishReason::Other("function_call".to_string()),
        ),
        (
            "vendor_value",
            FinishReason::Other("vendor_value".to_string()),
        ),
    ];

    for (wire, expected) in cases {
        assert_eq!(to_finish_reason(wire), expected);
    }
}
