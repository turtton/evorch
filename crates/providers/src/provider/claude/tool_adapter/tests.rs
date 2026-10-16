use std::collections::BTreeSet;

use serde_json::json;

use super::*;
use crate::message::{FinishReason, Usage};
use crate::stream::StreamAccumulator;
use crate::wire::anthropic::{AnthropicStreamInterpreter, from_wire_response};

fn request(names: &[&str]) -> WireMessagesRequest {
    serde_json::from_value(json!({
        "model":"claude-sonnet-4-6", "max_tokens":1024, "stream":false, "messages":[],
        "tools":names.iter().map(|name| json!({
            "name":name, "description":"original contract", "input_schema":{
                "type":"object", "properties":{"path":{"type":"string"}},
                "required":["path"], "additionalProperties":false
            }
        })).collect::<Vec<_>>()
    }))
    .unwrap()
}

fn response_message(content: Value) -> Message {
    from_wire_response(
        serde_json::from_value(json!({
            "role":"assistant", "content":content, "stop_reason":"tool_use"
        }))
        .unwrap(),
    )
    .message
}

#[test]
fn names_are_bounded_distinct_and_independent_of_the_catalog() {
    let long_name = "x".repeat(140);
    let invalid = "remote/read";
    let encoded_name = wire_name(invalid);
    let names = [
        "read",
        "write",
        "Read",
        "Write",
        "_read",
        "mcp__evorch__read",
        "shell",
        "web_search",
        "",
        invalid,
        "日本語",
        &long_name,
        &encoded_name,
    ];
    let mut wire = request(&names);
    let adapter = ClaudeToolAdapter::shape_request(&mut wire).unwrap();
    assert_eq!(wire.tools[0].name, "Read");
    assert_eq!(wire.tools[1].name, "Write");
    assert_eq!(wire.tools[2].name, "mcp__evorch__Read");
    assert_eq!(wire.tools[4].name, "mcp__evorch___read");
    assert_eq!(wire.tools[5].name, "mcp__evorch__mcp__evorch__read");
    assert_eq!(wire.tools[6].name, "mcp__evorch__shell");
    assert_eq!(wire.tools[7].name, "mcp__evorch__web_search");
    assert_eq!(
        wire.tools
            .iter()
            .map(|t| &t.name)
            .collect::<BTreeSet<_>>()
            .len(),
        names.len()
    );
    for (tool, original) in wire.tools.iter().zip(names) {
        assert!(tool.name.len() <= 128);
        assert!(
            tool.name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        );
        let mut alone = request(&[original]);
        ClaudeToolAdapter::shape_request(&mut alone).unwrap();
        assert_eq!(tool, &alone.tools[0]);
        let mut message = response_message(json!([{
            "type":"tool_use", "id":"call", "name":tool.name, "input":{}
        }]));
        adapter.restore_message(&mut message);
        assert!(
            matches!(&message.content[0], ContentBlock::ToolUse { name, .. } if name == original)
        );
    }
}

#[test]
fn schema_and_history_round_trip_without_losing_aliases_or_native_metadata() {
    let schema = json!({
        "type":"object", "properties":{
            "path":{"type":"string", "description":"primary"},
            "file_path":{"type":"string", "description":"legacy alias"},
            "content":{"type":"string"}, "approval":{"type":"boolean"},
            "nested":{"type":"object", "properties":{"path":{"type":"string"}}}
        }, "required":["path", "file_path", "content"], "additionalProperties":false,
        "$defs":{"unchanged":{"required":["path"]}}
    });
    let input = json!({"path":"primary", "file_path":"alias", "content":"file_path and path",
        "approval":true, "nested":{"path":"nested", "file_path":"nested alias"}});
    let content = json!([
        {"type":"tool_use", "id":"r", "name":"read", "input":input, "caller":{"type":"direct"}},
        {"type":"tool_use", "id":"w", "name":"write", "input":input},
        {"type":"tool_use", "id":"custom", "name":"shell", "input":input},
        {"type":"server_tool_use", "id":"s", "name":"read", "input":input},
        {"type":"mcp_tool_use", "id":"m", "name":"write", "server_name":"remote", "input":input},
        {"type":"thinking", "thinking":"thought", "signature":"opaque"},
        {"type":"text", "text":"visible", "citations":[{"type":"future", "path":"citation"}]}
    ]);
    let expected = response_message(content.clone());
    let mut wire = request(&["read", "shell", "write"]);
    for tool in &mut wire.tools {
        tool.input_schema = schema.clone();
    }
    wire.messages =
        serde_json::from_value(json!([{"role":"assistant", "content":content}])).unwrap();
    let adapter = ClaudeToolAdapter::shape_request(&mut wire).unwrap();
    assert_eq!(
        wire.tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["Read", "mcp__evorch__shell", "Write"]
    );
    assert_eq!(wire.tools[1].input_schema, schema);
    for index in [0, 2] {
        assert_eq!(wire.tools[index].description, "original contract");
        assert_eq!(
            wire.tools[index].input_schema["properties"]["file_path"],
            schema["properties"]["path"]
        );
        assert_eq!(
            wire.tools[index].input_schema["properties"]["path"],
            schema["properties"]["file_path"]
        );
        assert_eq!(
            wire.tools[index].input_schema["required"],
            json!(["file_path", "path", "content"])
        );
        swap_path_schema(&mut wire.tools[index].input_schema);
        assert_eq!(wire.tools[index].input_schema, schema);
    }
    let shaped = serde_json::to_value(&wire.messages[0].content).unwrap();
    assert_eq!(shaped[0]["name"], "Read");
    assert_eq!(shaped[0]["input"]["file_path"], "primary");
    assert_eq!(shaped[0]["input"]["path"], "alias");
    assert_eq!(shaped[0]["input"]["nested"], input["nested"]);
    assert_eq!(shaped[0]["caller"], content[0]["caller"]);
    assert_eq!(
        &shaped.as_array().unwrap()[3..],
        &content.as_array().unwrap()[3..]
    );
    let mut restored = response_message(shaped);
    adapter.restore_message(&mut restored);
    assert_eq!(restored, expected);
}

#[test]
fn removed_tools_are_registered_from_history_and_unknown_response_names_are_untouched() {
    let mut wire = request(&[]);
    wire.messages = serde_json::from_value(json!([{"role":"assistant", "content":[
        {"type":"tool_use", "id":"r", "name":"read", "input":{"file_path":"alias"}},
        {"type":"tool_use", "id":"old", "name":"retired", "input":{}, "caller":{"type":"direct"}}
    ]}]))
    .unwrap();
    let expected = response_message(serde_json::to_value(&wire.messages[0].content).unwrap());
    let adapter = ClaudeToolAdapter::shape_request(&mut wire).unwrap();
    let shaped = serde_json::to_value(&wire.messages[0].content).unwrap();
    assert_eq!(shaped[0]["name"], "Read");
    assert_eq!(shaped[0]["input"], json!({"path":"alias"}));
    assert_eq!(shaped[1]["name"], "mcp__evorch__retired");
    let mut message = response_message(shaped);
    adapter.restore_message(&mut message);
    assert_eq!(message, expected);
    let mut unknown = response_message(json!([
        {"type":"tool_use", "id":"a", "name":"_read", "input":{"file_path":"x"}},
        {"type":"tool_use", "id":"b", "name":"mcp__evorch__unknown", "input":{}, "caller":{"type":"direct"}},
        {"type":"tool_use", "id":"c", "name":"Write", "input":{"file_path":"x"}}
    ]));
    let original = unknown.clone();
    adapter.restore_message(&mut unknown);
    assert_eq!(unknown, original);
}

struct StreamHarness {
    inner: AnthropicStreamInterpreter,
    adapter: ClaudeToolAdapter,
    accumulator: StreamAccumulator,
}

impl StreamHarness {
    fn new() -> Self {
        Self {
            inner: AnthropicStreamInterpreter::new(),
            adapter: ClaudeToolAdapter::shape_request(&mut request(&["read", "write", "shell"]))
                .unwrap(),
            accumulator: StreamAccumulator::default(),
        }
    }

    fn feed(&mut self, value: Value) -> Vec<StreamEvent> {
        let frame = SseFrame {
            event: None,
            data: value.to_string(),
        };
        let mut events = self.inner.interpret(&frame).unwrap();
        self.adapter.restore_events(&frame, &mut events).unwrap();
        for event in &events {
            self.accumulator.feed(event);
        }
        events
    }

    fn start(&mut self, index: usize, name: &str, input: Value) -> Vec<StreamEvent> {
        self.feed(json!({"type":"content_block_start", "index":index,
            "content_block":{"type":"tool_use", "id":format!("id-{index}"), "name":name, "input":input}}))
    }

    fn delta(&mut self, index: usize, text: &str) -> Vec<StreamEvent> {
        self.feed(json!({"type":"content_block_delta", "index":index,
            "delta":{"type":"input_json_delta", "partial_json":text}}))
    }

    fn completed_message(&mut self) -> Message {
        self.feed(json!({"type":"message_stop"}));
        let mut message = self.inner.take_message();
        self.adapter.restore_message(&mut message);
        assert_eq!(
            self.accumulator
                .clone()
                .finish(Usage::default(), FinishReason::ToolUse)
                .message,
            message
        );
        message
    }
}

#[test]
fn stream_renames_only_complete_top_level_keys_with_interleaved_fragments() {
    let mut stream = StreamHarness::new();
    assert!(matches!(&stream.start(0, "Read", json!({}))[0],
        StreamEvent::ToolCallDelta { name: Some(name), arguments_delta, .. } if name == "read" && arguments_delta.is_empty()));
    stream.start(1, "Write", json!({}));
    stream.start(2, "mcp__evorch__shell", json!({}));
    assert!(stream.delta(0, "{\"file_").is_empty());
    assert!(
        stream
            .delta(1, "{\"file_path\":\"written\",\"content\":")
            .is_empty()
    );
    let custom = stream.delta(2, "{\"file_path\":\"untouched\"}");
    assert!(
        matches!(&custom[0], StreamEvent::ToolCallDelta { arguments_delta, .. } if arguments_delta == "{\"file_path\":\"untouched\"}")
    );
    assert!(
        stream
            .delta(
                0,
                "path\":\"read\",\"path\":\"alias\",\"nested\":{\"file_path\":\"inside\"}}"
            )
            .is_empty()
    );
    assert!(
        stream
            .delta(1, "\"file_path stays in content\"}")
            .is_empty()
    );
    let stopped = stream.feed(json!({"type":"content_block_stop", "index":0}));
    assert_eq!(stopped.len(), 1);
    assert!(
        matches!(&stopped[0], StreamEvent::ToolCallDelta { arguments_delta, .. }
        if serde_json::from_str::<Value>(arguments_delta).unwrap() == json!({"path":"read", "file_path":"alias", "nested":{"file_path":"inside"}}))
    );
    let message = stream.completed_message();
    assert!(
        matches!(&message.content[1], ContentBlock::ToolUse { name, input, .. }
        if name == "write" && input == &json!({"path":"written", "content":"file_path stays in content"}))
    );
    assert!(
        matches!(&message.content[2], ContentBlock::ToolUse { name, input, .. }
        if name == "shell" && input == &json!({"file_path":"untouched"}))
    );
}

#[test]
fn stream_preserves_initial_inputs_and_later_json_replaces_them() {
    let mut stream = StreamHarness::new();
    stream.start(0, "Read", json!({"file_path":"initial", "path":"alias"}));
    stream.start(1, "Write", json!({"file_path":"old", "content":"old"}));
    stream.start(2, "Read", json!({}));
    stream.delta(0, " \n ");
    stream.delta(1, "{\"file_path\":\"new\",\"content\":\"new\"}");
    let message = stream.completed_message();
    assert!(
        matches!(&message.content[0], ContentBlock::ToolUse { input, .. }
        if input == &json!({"path":"initial", "file_path":"alias"}))
    );
    assert!(
        matches!(&message.content[1], ContentBlock::ToolUse { input, .. }
        if input == &json!({"path":"new", "content":"new"}))
    );
    assert!(
        matches!(&message.content[2], ContentBlock::ToolUse { input, .. } if input == &json!({}))
    );
}

#[test]
fn stream_leaves_unknown_names_and_malformed_argument_diagnostics_intact() {
    let mut stream = StreamHarness::new();
    stream.start(0, "Read", json!({}));
    let malformed = "{\"file_path\":unfinished";
    stream.delta(0, malformed);
    let stopped = stream.feed(json!({"type":"content_block_stop", "index":0}));
    assert!(
        matches!(&stopped[0], StreamEvent::ToolCallDelta { arguments_delta, .. } if arguments_delta == malformed)
    );
    stream.start(1, "mcp__evorch__unknown", json!({}));
    let unknown = stream.delta(1, "{\"file_path\":\"untouched\"}");
    assert_eq!(unknown.len(), 1);
    let diagnostics = stream.accumulator.malformed_tool_calls();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].name, "read");
    assert_eq!(diagnostics[0].arguments_len, malformed.len());
    let message = stream.completed_message();
    assert!(matches!(&message.content[0], ContentBlock::ToolUse { input, .. } if input.is_null()));
    assert!(
        matches!(&message.content[1], ContentBlock::ToolUse { name, input, .. }
        if name == "mcp__evorch__unknown" && input == &json!({"file_path":"untouched"}))
    );
}
