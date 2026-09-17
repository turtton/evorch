use serde_json::json;

use super::to_wire_request;
use crate::message::{ChatRequest, ContentBlock, Message, Role, ToolSpec};

#[test]
fn cache_tools_are_byte_stable_when_registration_order_changes() {
    // Given: a tool set in opposite registration orders.
    let mut input = request();
    input.tools = ["zeta", "alpha"]
        .map(|name| ToolSpec {
            name: name.into(),
            description: name.into(),
            input_schema: json!({"type":"object"}),
        })
        .to_vec();
    let mut shuffled = input.clone();
    shuffled.tools.reverse();
    // When: both permutations are serialized.
    let first = serde_json::to_value(to_wire_request(&input)).unwrap();
    let second = serde_json::to_value(to_wire_request(&shuffled)).unwrap();
    // Then: the wire tool array is byte-identical.
    assert_eq!(
        serde_json::to_vec(&first["tools"]).unwrap(),
        serde_json::to_vec(&second["tools"]).unwrap()
    );
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5-codex".to_string(),
        messages: vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "Follow the repository rules.".to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Inspect the workspace.".to_string(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "I will inspect it.".to_string(),
                }],
            },
        ],
        tools: vec![ToolSpec {
            name: "read_file".to_string(),
            description: "Read a workspace file".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }],
        temperature: Some(0.2),
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        observation: None,
    }
}

#[test]
fn wire_request_maps_reasoning_effort_when_configured() {
    for (effort, expected) in [
        (Some("high"), "high"),
        (Some("low"), "low"),
        (None, "medium"),
    ] {
        // Given: 推論強度を指定または省略した canonical request。
        let mut input = serde_json::to_value(request()).unwrap();
        if let Some(effort) = effort {
            input["reasoning_effort"] = json!(effort);
        }
        let canonical: ChatRequest = serde_json::from_value(input).unwrap();
        // When: Codex wire JSON に変換する。
        let value = serde_json::to_value(to_wire_request(&canonical)).unwrap();
        // Then: 指定強度を使用し、未指定だけ medium にする。
        assert_eq!(value["reasoning"]["effort"], expected);
    }
}

// Given: canonical Codex request / When: wire JSON is serialized / Then: backend-required fields are forced
#[test]
fn wire_request_forces_store_false_stream_true() {
    let wire = to_wire_request(&request());

    let value = serde_json::to_value(wire).unwrap();

    assert_eq!(value["model"], "gpt-5-codex");
    assert_eq!(value["store"], false);
    assert_eq!(value["stream"], true);
    assert_eq!(value["tool_choice"], "auto");
    assert_eq!(value["parallel_tool_calls"], true);
    assert_eq!(value["reasoning"]["effort"], "medium");
    assert_eq!(value["reasoning"]["summary"], "auto");
    assert_eq!(value["include"], json!([]));
    assert!(value["instructions"].is_string());
    assert!(value["input"].is_array());
}

// Given: max_tokens is set / When: Codex wire JSON is serialized / Then: unconfirmed max_output_tokens is absent
#[test]
fn wire_request_omits_max_output_tokens() {
    let mut canonical = request();
    canonical.max_tokens = Some(1024);

    let value = serde_json::to_value(to_wire_request(&canonical)).unwrap();

    assert!(!value.as_object().unwrap().contains_key("max_output_tokens"));
}

// Given: system/user/assistant messages and a tool / When: converted / Then: Responses shapes preserve roles and schemas
#[test]
fn wire_request_maps_tools_and_messages() {
    let value = serde_json::to_value(to_wire_request(&request())).unwrap();

    let input = value["input"].as_array().unwrap();
    let tools = value["tools"].as_array().unwrap();

    assert_eq!(value["instructions"], "Follow the repository rules.");
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[0]["content"][0]["text"], "Inspect the workspace.");
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(input[1]["content"][0]["type"], "output_text");
    assert_eq!(input[1]["content"][0]["text"], "I will inspect it.");
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["name"], "read_file");
    assert_eq!(tools[0]["description"], "Read a workspace file");
    assert_eq!(tools[0]["parameters"]["required"], json!(["path"]));
}

// Given: a tool round trip (assistant ToolUse + user ToolResult) / When: converted / Then: replayed as function_call and function_call_output items
#[test]
fn wire_request_replays_tool_round_trip() {
    let mut canonical = request();
    canonical.messages.push(Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "Reading the file.".to_string(),
            },
            ContentBlock::ToolUse {
                id: "call_123".to_string(),
                name: "read_file".to_string(),
                input: json!({"path": "AGENTS.md"}),
            },
        ],
    });
    canonical.messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call_123".to_string(),
            content: vec![crate::message::ToolResultContent::Text {
                text: "file body".to_string(),
            }],
            is_error: false,
        }],
    });

    let value = serde_json::to_value(to_wire_request(&canonical)).unwrap();
    let input = value["input"].as_array().unwrap();

    assert_eq!(input.len(), 5);
    assert_eq!(input[2]["type"], "message");
    assert_eq!(input[2]["role"], "assistant");
    assert_eq!(input[2]["content"][0]["text"], "Reading the file.");
    let call = &input[3];
    assert_eq!(call["type"], "function_call");
    assert_eq!(call["call_id"], "call_123");
    assert_eq!(call["name"], "read_file");
    assert_eq!(call["arguments"], json!({"path": "AGENTS.md"}).to_string());
    let output = &input[4];
    assert_eq!(output["type"], "function_call_output");
    assert_eq!(output["call_id"], "call_123");
    assert_eq!(output["output"], "file body");
    assert!(
        input
            .iter()
            .filter(|item| item["type"] == "message")
            .all(|item| !item["content"].as_array().unwrap().is_empty()),
        "空の content を持つ message 項目を送らない"
    );
}
