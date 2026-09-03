//! プロバイダ間の canonical メッセージ往復契約を検証します。

mod error {
    pub use providers::error::*;
}

mod message {
    pub use providers::message::*;
}

mod openai_wire {
    #[allow(dead_code)]
    mod response_types {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/wire/openai/response_types.rs"
        ));
    }
    mod types {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/wire/openai/types.rs"
        ));
    }
    mod request {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/wire/openai/request.rs"
        ));
    }
    mod response {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/wire/openai/response.rs"
        ));
    }

    pub use request::{from_wire_messages, to_wire_request};
    pub use response::from_wire_response;
    pub use response_types::WireChatResponse;
}

mod anthropic_wire {
    const DEFAULT_MAX_TOKENS: u64 = 4096;

    mod types {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/wire/anthropic/types.rs"
        ));
    }
    mod convert {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/wire/anthropic/convert.rs"
        ));
    }

    pub use convert::{from_wire_response, to_wire_request};
    pub use types::{WireContentBlock, WireMessagesResponse, WireRole};
}

use anthropic_wire::{WireContentBlock, WireRole};
use message::{ChatRequest, ContentBlock, Message, Role};
use openai_wire::WireChatResponse;
use serde_json::json;

fn representative_request() -> ChatRequest {
    ChatRequest {
        model: "cross-provider-test".to_string(),
        messages: vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "安全に回答してください。".to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "東京の天気を教えて。".to_string(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Reasoning {
                        text: "天気ツールを使う。".to_string(),
                    },
                    ContentBlock::Text {
                        text: "確認します。".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_weather".to_string(),
                        name: "weather".to_string(),
                        input: json!({"city": "Tokyo"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: "call_weather".to_string(),
                    content: vec![message::ToolResultContent::Text {
                        text: "晴れ、25°C".to_string(),
                    }],
                    is_error: true,
                }],
            },
        ],
        tools: vec![],
        temperature: Some(0.2),
        max_tokens: Some(128),
        observation: None,
    }
}

// Given: reasoning とエラーの tool result を含む canonical request / When: OpenAI wire を往復 / Then: OpenAI の不可逆な欠落を除いて一致する
#[test]
fn canonical_request_round_trips_through_openai() {
    let request = representative_request();

    let wire = openai_wire::to_wire_request(&request, false);
    let actual = openai_wire::from_wire_messages(&wire.messages).expect("wire messages convert");

    assert_eq!(
        actual,
        vec![
            request.messages[0].clone(),
            request.messages[1].clone(),
            Message {
                role: Role::Assistant,
                content: request.messages[2].content[1..].to_vec(),
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: "call_weather".to_string(),
                    content: vec![message::ToolResultContent::Text {
                        text: "晴れ、25°C".to_string(),
                    }],
                    is_error: false,
                }],
            },
        ]
    );
    // OpenAI wire に reasoning field はなく、ToolResult の is_error も表現できない。
}

// Given: system hoisting・thinking・tool blocks を含む canonical request / When: Anthropic wire へ変換して canonical 化 / Then: system role と各 block の意味が保たれる
#[test]
fn canonical_request_round_trips_through_anthropic() {
    let request = representative_request();
    let wire = anthropic_wire::to_wire_request(&request, false);

    assert_eq!(wire.system.as_deref(), Some("安全に回答してください。"));
    let restored_system = Message {
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: wire.system.clone().expect("system must be hoisted"),
        }],
    };
    assert_eq!(restored_system, request.messages[0]);

    let user = wire.messages[0].clone();
    let restored = anthropic_wire::from_wire_response(anthropic_wire::WireMessagesResponse {
        id: None,
        kind: None,
        role: WireRole::User,
        model: None,
        content: user.content,
        stop_reason: None,
        stop_sequence: None,
        usage: Default::default(),
    });
    assert_eq!(restored.message, request.messages[1]);

    let assistant = wire.messages[1].clone();
    let restored = anthropic_wire::from_wire_response(anthropic_wire::WireMessagesResponse {
        id: None,
        kind: None,
        role: WireRole::Assistant,
        model: None,
        content: assistant.content,
        stop_reason: None,
        stop_sequence: None,
        usage: Default::default(),
    });
    assert_eq!(restored.message.role, Role::Assistant);
    assert_eq!(restored.message.content, request.messages[2].content);

    let tool_result = wire.messages[2].clone();
    let restored = anthropic_wire::from_wire_response(anthropic_wire::WireMessagesResponse {
        id: None,
        kind: None,
        role: WireRole::User,
        model: None,
        content: tool_result.content,
        stop_reason: None,
        stop_sequence: None,
        usage: Default::default(),
    });
    assert_eq!(restored.message.role, Role::User);
    assert_eq!(restored.message.content, request.messages[3].content);
}

// Given: OpenAI fixture 相当の canonical 会話 / When: Anthropic wire へ変換 / Then: panic せず stable な block 列になる
#[test]
fn openai_fixture_shaped_conversation_is_stable_on_anthropic_egress() {
    let request = representative_request();
    let wire = anthropic_wire::to_wire_request(&request, false);

    assert_eq!(wire.messages.len(), 3);
    assert!(matches!(
        wire.messages[0].content[0],
        WireContentBlock::Text { .. }
    ));
    assert!(matches!(
        wire.messages[1].content[0],
        WireContentBlock::Thinking { .. }
    ));
    assert!(matches!(
        wire.messages[1].content[2],
        WireContentBlock::ToolUse { .. }
    ));
    assert!(matches!(
        wire.messages[2].content[0],
        WireContentBlock::ToolResult { .. }
    ));
}

// Given: OpenAI response-shaped fixture / When: canonical response を Anthropic request に変換 / Then: response の block が失われず変換可能である
#[test]
fn openai_response_converts_to_anthropic_request_shape() {
    let wire: WireChatResponse = serde_json::from_value(json!({
        "id": "chatcmpl-round-trip",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "回答",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "weather", "arguments": "{\"city\":\"Tokyo\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 8, "completion_tokens": 4}
    }))
    .expect("OpenAI fixture must deserialize");

    let response = openai_wire::from_wire_response(&wire).expect("OpenAI response converts");
    let request = ChatRequest {
        model: "claude-round-trip".to_string(),
        messages: vec![response.message.clone()],
        tools: vec![],
        temperature: None,
        max_tokens: Some(64),
        observation: None,
    };
    let anthropic = anthropic_wire::to_wire_request(&request, false);

    assert_eq!(anthropic.messages.len(), 1);
    assert_eq!(anthropic.messages[0].role, WireRole::Assistant);
    assert_eq!(
        anthropic.messages[0].content,
        vec![
            WireContentBlock::Text {
                text: "回答".to_string()
            },
            WireContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "weather".to_string(),
                input: json!({"city": "Tokyo"}),
            }
        ]
    );
}
