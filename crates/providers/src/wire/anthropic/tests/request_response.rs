use super::super::*;
use crate::message::{
    ChatRequest, ChatResponse, ContentBlock, FinishReason, Message, Role, ToolResultContent,
    ToolSpec, Usage,
};
use serde_json::json;

// Given: system・thinking・tool result・tool 定義を含む canonical request / When: wire request に変換 / Then: Anthropic の実フィールド形状になる
#[test]
fn canonical_request_converts_to_anthropic_wire_shape() {
    let request = ChatRequest {
        model: "claude-test".to_string(),
        messages: vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "first".to_string(),
                }],
            },
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "second".to_string(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Reasoning {
                    text: "考え中".to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: "toolu_1".to_string(),
                    content: vec![ToolResultContent::Text {
                        text: "晴れ".to_string(),
                    }],
                    is_error: false,
                }],
            },
        ],
        tools: vec![ToolSpec {
            name: "weather".to_string(),
            description: "天気を取得".to_string(),
            input_schema: json!({"type": "object"}),
        }],
        temperature: Some(0.2),
        max_tokens: None,
        observation: None,
    };

    let wire = serde_json::to_value(to_wire_request(&request, true)).unwrap();

    assert_eq!(
        wire,
        json!({
            "model": "claude-test",
            "max_tokens": 4096,
            "system": "first\n\nsecond",
            "messages": [
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "考え中"}]},
                {"role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": "toolu_1",
                    "content": [{"type": "text", "text": "晴れ"}], "is_error": false
                }]}
            ],
            "tools": [{"name": "weather", "description": "天気を取得", "input_schema": {"type": "object"}}],
            "temperature": 0.2,
            "stream": true
        })
    );
}

// Given: tool result と user reasoning を含む canonical request / When: wire request に変換 / Then: user role を強制し reasoning は text になる
#[test]
fn user_only_blocks_follow_anthropic_role_constraints() {
    let request = ChatRequest {
        model: "claude-test".to_string(),
        messages: vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: "toolu_1".to_string(),
                    content: vec![],
                    is_error: true,
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Reasoning {
                    text: "内部メモ".to_string(),
                }],
            },
        ],
        tools: vec![],
        temperature: None,
        max_tokens: Some(32),
        observation: None,
    };

    let wire = serde_json::to_value(to_wire_request(&request, false)).unwrap();

    assert_eq!(wire["messages"][0]["role"], "user");
    assert_eq!(
        wire["messages"][1]["content"][0],
        json!({"type": "text", "text": "内部メモ"})
    );
    assert_eq!(wire["max_tokens"], 32);
    assert_eq!(wire["stream"], false);
}

// Given: cache usage と全 content block を含む wire response / When: canonical response に変換 / Then: role・内容・4 usage フィールドが保存される
#[test]
fn wire_response_converts_to_canonical_response() {
    let wire: WireMessagesResponse = serde_json::from_value(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-test",
        "content": [
            {"type": "text", "text": "回答"},
            {"type": "thinking", "thinking": "思考"},
            {"type": "tool_use", "id": "toolu_1", "name": "weather", "input": {"city": "東京"}},
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "晴れ"}], "is_error": false}
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": {"input_tokens": 11, "output_tokens": 7, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 3}
    }))
    .unwrap();

    let response = from_wire_response(wire);

    assert_eq!(
        response,
        ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "回答".to_string()
                    },
                    ContentBlock::Reasoning {
                        text: "思考".to_string()
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_1".to_string(),
                        name: "weather".to_string(),
                        input: json!({"city": "東京"})
                    },
                    ContentBlock::ToolResult {
                        tool_call_id: "toolu_1".to_string(),
                        content: vec![ToolResultContent::Text {
                            text: "晴れ".to_string()
                        }],
                        is_error: false
                    },
                ]
            },
            usage: Usage {
                input_tokens: 11,
                output_tokens: 7,
                cache_read_tokens: 3,
                cache_write_tokens: 5
            },
            finish_reason: FinishReason::ToolUse,
        }
    );
}

// Given: Anthropic の既知・未知 stop reason / When: canonical に変換 / Then: 対応する finish reason になる
#[test]
fn stop_reason_mapping_covers_known_and_unknown_values() {
    let cases = [
        (Some("end_turn"), FinishReason::Stop),
        (Some("max_tokens"), FinishReason::Length),
        (Some("tool_use"), FinishReason::ToolUse),
        (Some("stop_sequence"), FinishReason::Stop),
        (
            Some("model_context_window_exceeded"),
            FinishReason::Other("model_context_window_exceeded".to_string()),
        ),
        (None, FinishReason::Stop),
    ];

    for (wire, expected) in cases {
        assert_eq!(to_finish_reason(wire), expected);
    }
}
