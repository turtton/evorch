use super::super::*;
use crate::message::{ChatRequest, ContentBlock, Message, Role, ToolResultContent, ToolSpec};
use serde_json::json;

// Given: 全 role とツール往復を含む canonical request / When: OpenAI wire request に変換 / Then: 公式 Chat Completions JSON 形状になる
#[test]
fn canonical_request_maps_to_chat_completions_json() {
    let request = ChatRequest {
        model: "gpt-test".to_string(),
        messages: vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "system".to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "weather?".to_string(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Reasoning {
                        text: "omitted".to_string(),
                    },
                    ContentBlock::Text {
                        text: "checking".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "weather".to_string(),
                        input: json!({"city": "Tokyo"}),
                    },
                    ContentBlock::ToolUse {
                        id: "call_2".to_string(),
                        name: "time".to_string(),
                        input: json!({"zone": "JST"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_call_id: "call_1".to_string(),
                        content: vec![ToolResultContent::Text {
                            text: "sunny".to_string(),
                        }],
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_call_id: "call_2".to_string(),
                        content: vec![ToolResultContent::Text {
                            text: "10:00".to_string(),
                        }],
                        is_error: false,
                    },
                ],
            },
        ],
        tools: vec![ToolSpec {
            name: "weather".to_string(),
            description: "Get weather".to_string(),
            input_schema: json!({"type": "object"}),
        }],
        temperature: Some(0.2),
        max_tokens: Some(128),
        observation: None,
    };

    let wire = to_wire_request(&request, true);
    let actual = serde_json::to_value(&wire).expect("wire request must serialize");

    assert_eq!(
        actual,
        json!({
            "model": "gpt-test",
            "messages": [
                {"role": "system", "content": "system"},
                {"role": "user", "content": "weather?"},
                {
                    "role": "assistant",
                    "content": "checking",
                    "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"Tokyo\"}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "time", "arguments": "{\"zone\":\"JST\"}"}}
                    ]
                },
                {"role": "tool", "content": "sunny", "tool_call_id": "call_1"},
                {"role": "tool", "content": "10:00", "tool_call_id": "call_2"}
            ],
            "tools": [{
                "type": "function",
                "function": {"name": "weather", "description": "Get weather", "parameters": {"type": "object"}}
            }],
            "temperature": 0.2,
            "max_tokens": 128,
            "stream": true,
            "stream_options": {"include_usage": true}
        })
    );
    assert_eq!(
        from_wire_messages(&wire.messages).expect("wire messages must convert"),
        vec![
            request.messages[0].clone(),
            request.messages[1].clone(),
            Message {
                role: Role::Assistant,
                content: request.messages[2].content[1..].to_vec(),
            },
            request.messages[3].clone(),
        ]
    );
}
