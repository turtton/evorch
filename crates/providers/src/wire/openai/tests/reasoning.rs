use super::super::to_wire_request;
use crate::message::ChatRequest;
use serde_json::json;

#[test]
fn reasoning_content_serialized_when_model_is_kimi_family() {
    for model in [
        "kimi-k3",
        "kimi-k2.7-code",
        "moonshot-v1-8k",
        "cli-proxy-api/kimi-k3",
        "proxy/nested/KiMi-K3",
        "proxy/MOONSHOT-v1-8k",
    ] {
        for stream in [false, true] {
            // Given: assistant history includes split, reasoning-only and empty reasoning.
            let request: ChatRequest = serde_json::from_value(json!({
                "model": model,
                "messages": [
                    {"role": "assistant", "content": [
                        {"type": "reasoning", "text": "first "},
                        {"type": "text", "text": "answer"},
                        {"type": "reasoning", "text": "second"}
                    ]},
                    {"role": "assistant", "content": [{"type": "reasoning", "text": "only"}]},
                    {"role": "assistant", "content": [{"type": "reasoning", "text": ""}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "plain"}]},
                    {"role": "user", "content": [
                        {"type": "reasoning", "text": "not assistant"},
                        {"type": "text", "text": "question"}
                    ]}
                ]
            }))
            .unwrap();
            // When: serialize the actual Chat Completions request.
            let value = serde_json::to_value(to_wire_request(&request, stream)).unwrap();
            // Then: preserve reasoning verbatim and only on assistants with reasoning blocks.
            assert_eq!(
                value["messages"],
                json!([
                    {"role": "assistant", "content": "answer", "reasoning_content": "first second"},
                    {"role": "assistant", "reasoning_content": "only"},
                    {"role": "assistant", "reasoning_content": ""},
                    {"role": "assistant", "content": "plain"},
                    {"role": "user", "content": "question"}
                ]),
                "Kimi-family model {model} must preserve assistant reasoning (stream={stream})"
            );
        }
    }
}

#[test]
fn reasoning_content_omitted_when_model_is_not_kimi_family() {
    for model in [
        "gpt-5",
        "kimi/gpt-5",
        "moonshot/gpt-5",
        "proxy/not-kimi",
        "kimi/",
    ] {
        // Given: a non-Kimi target, even if its provider prefix is Kimi.
        let request: ChatRequest = serde_json::from_value(json!({
            "model": model,
            "messages": [{"role": "assistant", "content": [
                {"type": "reasoning", "text": "private"},
                {"type": "text", "text": "answer"}
            ]}]
        }))
        .unwrap();
        // When: serialize to bytes, preserving the pre-existing wire field order.
        let bytes = serde_json::to_vec(&to_wire_request(&request, false)).unwrap();
        // Then: the entire non-Kimi request is byte-identical; no reasoning field.
        assert_eq!(bytes, format!(
            "{{\"model\":\"{model}\",\"messages\":[{{\"role\":\"assistant\",\"content\":\"answer\"}}],\"stream\":false}}"
        ).into_bytes(), "non-Kimi model {model} must omit reasoning_content");
    }
}

#[test]
fn tool_calls_pairing_preserved_when_reasoning_content_present() {
    // Given: a reasoning assistant with two calls and their corresponding results.
    let request: ChatRequest = serde_json::from_value(json!({
        "model": "kimi-k3",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "reasoning", "text": "check both"},
                {"type": "tool_use", "id": "call_1", "name": "weather", "input": {"city": "Tokyo"}},
                {"type": "tool_use", "id": "call_2", "name": "time", "input": {"zone": "JST"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_call_id": "call_1", "content": [{"type": "text", "text": "sunny"}], "is_error": false},
                {"type": "tool_result", "tool_call_id": "call_2", "content": [{"type": "text", "text": "10:00"}], "is_error": false}
            ]}
        ]
    })).unwrap();
    // When: serialize the tool round trip.
    let value = serde_json::to_value(to_wire_request(&request, false)).unwrap();
    // Then: reasoning and the complete call/result pairing coexist.
    assert_eq!(
        value["messages"],
        json!([
            {"role": "assistant", "reasoning_content": "check both", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"Tokyo\"}"}},
                {"id": "call_2", "type": "function", "function": {"name": "time", "arguments": "{\"zone\":\"JST\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "sunny"},
            {"role": "tool", "tool_call_id": "call_2", "content": "10:00"}
        ]),
        "reasoning_content must coexist with intact tool calls and paired results"
    );
}
