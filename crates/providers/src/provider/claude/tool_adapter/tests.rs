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
    let boundary_name = "x".repeat(128 - "_mcp__evorch__".len());
    let invalid = "remote/read";
    let encoded_name = wire_name(invalid);
    let names = [
        "read",
        "write",
        "edit",
        "grep",
        "shell",
        "Read",
        "Write",
        "_read",
        "mcp__evorch__read",
        "_mcp__evorch__read",
        "web_search",
        "",
        invalid,
        "日本語",
        &long_name,
        &encoded_name,
        &boundary_name,
    ];
    let mut wire = request(&names);
    let adapter = ClaudeToolAdapter::shape_request(&mut wire).unwrap();
    assert_eq!(
        wire.tools[..5]
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["_read", "_write", "_edit", "_grep", "_shell"]
    );
    assert_eq!(wire.tools[5].name, "_mcp__evorch__Read");
    assert_eq!(wire.tools[7].name, "_mcp__evorch___read");
    assert_eq!(wire.tools[8].name, "_mcp__evorch__mcp__evorch__read");
    assert_eq!(wire.tools[9].name, "_mcp__evorch___mcp__evorch__read");
    assert_eq!(wire.tools[10].name, "_mcp__evorch__web_search");
    assert_eq!(wire.tools.last().unwrap().name.len(), 128);
    assert_eq!(
        wire.tools
            .iter()
            .map(|tool| &tool.name)
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
fn shaping_preserves_schemas_arguments_and_raw_metadata_including_native_tools() {
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
        {"type":"tool_use", "id":"custom", "name":"web_search", "input":input},
        {"type":"server_tool_use", "id":"s", "name":"web_search", "input":input},
        {"type":"mcp_tool_use", "id":"m", "name":"write", "server_name":"remote", "input":input},
        {"type":"thinking", "thinking":"thought", "signature":"opaque"},
        {"type":"text", "text":"visible", "citations":[{"type":"future", "path":"citation"}]}
    ]);
    let expected = response_message(content.clone());
    let mut wire = request(&["read", "web_search", "write"]);
    for tool in &mut wire.tools {
        tool.input_schema = schema.clone();
    }
    let original_tools = wire.tools.clone();
    wire.messages =
        serde_json::from_value(json!([{"role":"assistant", "content":content}])).unwrap();
    let adapter = ClaudeToolAdapter::shape_request(&mut wire).unwrap();
    for (tool, original) in wire.tools.iter().zip(&original_tools) {
        let mut expected_tool = original.clone();
        expected_tool.name = wire_name(&original.name);
        assert_eq!(tool, &expected_tool);
    }
    let shaped = serde_json::to_value(&wire.messages[0].content).unwrap();
    let mut expected_shaped = content.clone();
    for index in 0..3 {
        expected_shaped[index]["name"] = wire_name(content[index]["name"].as_str().unwrap()).into();
    }
    assert_eq!(shaped, expected_shaped);
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
    assert_eq!(shaped[0]["name"], "_read");
    assert_eq!(shaped[0]["input"], json!({"file_path":"alias"}));
    assert_eq!(shaped[1]["name"], "_mcp__evorch__retired");
    let mut message = response_message(shaped);
    adapter.restore_message(&mut message);
    assert_eq!(message, expected);
    let mut unknown = response_message(json!([
        {"type":"tool_use", "id":"a", "name":"Read", "input":{"file_path":"x"}},
        {"type":"tool_use", "id":"b", "name":"_mcp__evorch__unknown", "input":{}, "caller":{"type":"direct"}},
        {"type":"tool_use", "id":"c", "name":"_write", "input":{"file_path":"x"}}
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
            adapter: ClaudeToolAdapter::shape_request(&mut request(&[
                "read",
                "write",
                "shell",
                "web_search",
            ]))
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
fn stream_restores_names_immediately_and_passes_interleaved_fragments_unchanged() {
    let mut stream = StreamHarness::new();
    for (index, (wire, canonical)) in [
        ("_read", "read"),
        ("_write", "write"),
        ("_mcp__evorch__web_search", "web_search"),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(matches!(&stream.start(index, wire, json!({}))[0],
            StreamEvent::ToolCallDelta { name: Some(name), arguments_delta, .. }
            if name == canonical && arguments_delta.is_empty()));
    }
    for (index, fragment) in [
        (0, "{\"file_"),
        (1, "{\"path\":\"written\",\"content\":"),
        (2, "{\"file_path\":\"untouched\"}"),
        (
            0,
            "path\":\"read\",\"path\":\"alias\",\"nested\":{\"file_path\":\"inside\"}}",
        ),
        (1, "\"file_path stays in content\"}"),
    ] {
        let events = stream.delta(index, fragment);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], StreamEvent::ToolCallDelta { arguments_delta, .. } if arguments_delta == fragment)
        );
    }
    assert!(
        stream
            .feed(json!({"type":"content_block_stop", "index":0}))
            .is_empty()
    );
    let message = stream.completed_message();
    assert!(
        matches!(&message.content[0], ContentBlock::ToolUse { input, .. }
        if input == &json!({"file_path":"read", "path":"alias", "nested":{"file_path":"inside"}}))
    );
}

#[test]
fn stream_preserves_initial_inputs_once_for_every_local_tool_and_later_json_replaces_them() {
    let mut stream = StreamHarness::new();
    for (index, name) in [
        "_read",
        "_write",
        "_shell",
        "_mcp__evorch__web_search",
        "unknown",
    ]
    .into_iter()
    .enumerate()
    {
        stream.start(index, name, json!({"path":"initial", "file_path":"alias"}));
        stream.delta(index, " \n ");
    }
    stream.start(5, "_write", json!({"path":"old", "content":"old"}));
    stream.delta(5, "{\"path\":\"new\",\"content\":\"new\"}");
    stream.start(6, "_read", json!({}));
    let stopped = stream.feed(json!({"type":"content_block_stop", "index":0}));
    assert_eq!(stopped.len(), 1);
    assert!(
        matches!(&stopped[0], StreamEvent::ToolCallDelta { arguments_delta, .. }
        if serde_json::from_str::<Value>(arguments_delta).unwrap() == json!({"path":"initial", "file_path":"alias"}))
    );
    assert!(
        stream
            .feed(json!({"type":"content_block_stop", "index":0}))
            .is_empty()
    );
    let message = stream.completed_message();
    for block in &message.content[..5] {
        assert!(matches!(block, ContentBlock::ToolUse { input, .. }
            if input == &json!({"path":"initial", "file_path":"alias"})));
    }
    assert!(
        matches!(&message.content[5], ContentBlock::ToolUse { input, .. }
        if input == &json!({"path":"new", "content":"new"}))
    );
    assert!(
        matches!(&message.content[6], ContentBlock::ToolUse { input, .. } if input == &json!({}))
    );
}

#[test]
fn stream_leaves_unknown_names_and_malformed_argument_diagnostics_intact() {
    let mut stream = StreamHarness::new();
    stream.start(
        0,
        "_read",
        json!({"path":"must not hide malformed replacement"}),
    );
    let malformed = "{\"file_path\":unfinished";
    let events = stream.delta(0, malformed);
    assert!(
        matches!(&events[0], StreamEvent::ToolCallDelta { arguments_delta, .. } if arguments_delta == malformed)
    );
    assert!(
        stream
            .feed(json!({"type":"content_block_stop", "index":0}))
            .is_empty()
    );
    stream.start(1, "_mcp__evorch__unknown", json!({}));
    stream.delta(1, "{\"file_path\":\"untouched\"}");
    let diagnostics = stream.accumulator.malformed_tool_calls();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].name, "read");
    assert_eq!(diagnostics[0].arguments_len, malformed.len());
    let message = stream.completed_message();
    assert!(matches!(&message.content[0], ContentBlock::ToolUse { input, .. } if input.is_null()));
    assert!(
        matches!(&message.content[1], ContentBlock::ToolUse { name, input, .. }
        if name == "_mcp__evorch__unknown" && input == &json!({"file_path":"untouched"}))
    );
}

#[test]
fn stream_keeps_native_blocks_out_of_local_tool_events_and_preserves_raw_metadata() {
    let mut stream = StreamHarness::new();
    let native = [
        json!({"type":"server_tool_use", "id":"server", "name":"_read", "input":{"path":"server"}}),
        json!({"type":"mcp_tool_use", "id":"mcp", "name":"_write", "server_name":"remote", "input":{"path":"remote"}}),
    ];
    for (index, block) in native.iter().enumerate() {
        assert!(
            stream
                .feed(json!({"type":"content_block_start", "index":index, "content_block":block}))
                .is_empty()
        );
    }
    let raw = json!({"type":"tool_use", "id":"raw", "name":"_read", "input":{"path":"local"}, "caller":{"type":"direct"}});
    stream.feed(json!({"type":"content_block_start", "index":2, "content_block":raw}));
    stream.feed(json!({"type":"message_stop"}));
    let mut message = stream.inner.take_message();
    stream.adapter.restore_message(&mut message);
    let mut content = Vec::from(native);
    let mut expected_raw = raw;
    expected_raw["name"] = "read".into();
    content.push(expected_raw);
    assert_eq!(message, response_message(json!(content)));
    assert_eq!(
        stream
            .accumulator
            .clone()
            .finish(Usage::default(), FinishReason::ToolUse)
            .message
            .content
            .len(),
        1
    );
}
