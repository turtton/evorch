use serde_json::json;

use super::to_wire_request;
use crate::message::{ChatRequest, ContentBlock, Message, Role, ToolSpec};

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
        observation: None,
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
