use serde_json::{Value, json};

use super::super::{AnthropicStreamInterpreter, WireMessagesResponse, from_wire_response};
use crate::message::{ContentBlock, FinishReason};
use crate::sse::SseFrame;
use crate::stream::StreamEvent;

fn event(interpreter: &mut AnthropicStreamInterpreter, value: Value) -> Vec<StreamEvent> {
    interpreter
        .interpret(&SseFrame {
            event: None,
            data: value.to_string(),
        })
        .unwrap()
}

#[test]
fn native_blocks_and_extension_metadata_survive_in_index_order() {
    let blocks = vec![
        json!({"type":"thinking","thinking":"","signature":"complete-signature"}),
        json!({"type":"redacted_thinking","data":"opaque"}),
        json!({"type":"text","text":"answer","citations":[{"type":"future_citation","source":"citation"}]}),
        json!({"type":"tool_use","id":"local","name":"read","input":{"path":"a"},"caller":{"type":"direct"}}),
        json!({"type":"server_tool_use","id":"server","name":"web_search","input":{"query":"a"}}),
        json!({"type":"web_search_tool_result","tool_use_id":"server","content":[]}),
        json!({"type":"web_fetch_tool_result","tool_use_id":"server","content":{"type":"web_fetch_tool_error","error_code":"unavailable"}}),
        json!({"type":"tool_search_tool_result","tool_use_id":"server","content":[]}),
        json!({"type":"code_execution_tool_result","tool_use_id":"server","content":{"type":"code_execution_result","stdout":"ok"}}),
        json!({"type":"bash_code_execution_tool_result","tool_use_id":"server","content":{"type":"bash_code_execution_result","stdout":"ok"}}),
        json!({"type":"text_editor_code_execution_tool_result","tool_use_id":"server","content":{"type":"text_editor_code_execution_view_result","content":"ok"}}),
        json!({"type":"container_upload","file_id":"file-1"}),
        json!({"type":"advisor_tool_result","tool_use_id":"server","content":[]}),
        json!({"type":"mcp_tool_use","id":"mcp","name":"remote","server_name":"server","input":{"q":"a"}}),
        json!({"type":"mcp_tool_result","tool_use_id":"mcp","content":[],"is_error":false}),
        json!({"type":"mcp_tool_listing","server_name":"server","tools":[]}),
        json!({"type":"compaction","content":"summary","encrypted_content":"opaque-compaction"}),
        json!({"type":"fallback","model":"fallback-model"}),
        json!({"type":"future_block","opaque":{"field":[1,2,3]}}),
    ];
    let mut interpreter = AnthropicStreamInterpreter::new();
    let mut deltas = Vec::new();
    for (index, block) in blocks.iter().enumerate().rev() {
        deltas.extend(event(
            &mut interpreter,
            json!({"type":"content_block_start","index":index,"content_block":block}),
        ));
        event(
            &mut interpreter,
            json!({"type":"content_block_delta","index":index,"delta":{"type":"future_delta","data":"opaque"}}),
        );
        event(
            &mut interpreter,
            json!({"type":"content_block_stop","index":index}),
        );
    }
    event(&mut interpreter, json!({"type":"message_stop"}));
    let message = interpreter.take_message();
    let raw: Vec<_> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Anthropic { block, .. } => Some(block.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(raw, blocks);
    assert_eq!(
        deltas
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolCallDelta { .. }))
            .count(),
        1
    );
    assert_eq!(
        message
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .count(),
        1
    );
    assert_eq!(
        message
            .content
            .iter()
            .filter_map(ContentBlock::text)
            .collect::<String>(),
        "answer"
    );
    assert!(message.content[0].is_reasoning());
    assert_eq!(message.content[0].reasoning_text(), Some(""));
    assert_eq!(message.content[1].reasoning_text(), None);
    let response: WireMessagesResponse =
        serde_json::from_value(json!({"role":"assistant","content":blocks})).unwrap();
    assert_eq!(from_wire_response(response).message, message);
}

#[test]
fn signatures_and_compaction_replace_fields_while_usage_merges() {
    let mut interpreter = AnthropicStreamInterpreter::new();
    event(
        &mut interpreter,
        json!({"type":"message_start","message":{"usage":{"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":3}}}),
    );
    event(
        &mut interpreter,
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":"initial"}}),
    );
    for signature in ["intermediate", "final"] {
        assert!(event(&mut interpreter, json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":signature}})).is_empty());
    }
    event(
        &mut interpreter,
        json!({"type":"content_block_start","index":1,"content_block":{"type":"compaction","content":null}}),
    );
    for (content, encrypted) in [
        (json!("first"), json!("old")),
        (Value::Null, Value::Null),
        (json!("last"), json!("new")),
    ] {
        event(
            &mut interpreter,
            json!({"type":"content_block_delta","index":1,"delta":{"type":"compaction_delta","content":content,"encrypted_content":encrypted}}),
        );
    }
    event(
        &mut interpreter,
        json!({"type":"message_delta","delta":{"stop_reason":"pause_turn"},"usage":{"input_tokens":10,"cache_creation_input_tokens":4,"output_tokens":20}}),
    );
    event(
        &mut interpreter,
        json!({"type":"message_delta","delta":{},"usage":{"output_tokens":25}}),
    );
    event(&mut interpreter, json!({"type":"message_stop"}));
    let content = interpreter.take_message().content;
    assert!(
        matches!(&content[0], ContentBlock::Anthropic { block, .. } if block == &json!({"type":"thinking","thinking":"","signature":"final"}))
    );
    assert!(
        matches!(&content[1], ContentBlock::Anthropic { block, .. } if block == &json!({"type":"compaction","content":"last","encrypted_content":"new"}))
    );
    let (usage, finish) = interpreter.take_result();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens
        ),
        (17, 25, 3, 4)
    );
    assert_eq!(finish, FinishReason::Other("pause_turn".into()));
}

#[test]
fn malformed_known_payloads_and_api_errors_still_fail() {
    let malformed = [
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"id","input":{}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":7}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking"}}),
        json!({"type":"content_block_start","index":0,"content_block":{"opaque":"missing-discriminator"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":null}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":17}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":"invalid"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"compaction_delta","encrypted_content":3}}),
        json!({"type":"content_block_stop","index":"bad-index"}),
        json!({"type":"message_delta","delta":{},"usage":{"output_tokens":"not-a-number"}}),
        json!({"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}),
    ];
    for value in malformed {
        let result = AnthropicStreamInterpreter::new().interpret(&SseFrame {
            event: None,
            data: value.to_string(),
        });
        assert!(result.is_err(), "accepted malformed frame: {value}");
    }
}
