use super::super::*;
use crate::error::ProviderError;
use crate::message::{FinishReason, Usage};
use crate::sse::SseFrame;
use crate::stream::StreamEvent;
use serde_json::json;

fn frame(event: Option<&str>, data: serde_json::Value) -> SseFrame {
    SseFrame {
        event: event.map(str::to_string),
        data: data.to_string(),
    }
}

// Given: tool use と usage を含む Anthropic SSE 列 / When: 順番に解釈 / Then: canonical delta と最終結果が正確に得られる
#[test]
fn interpreter_maps_scripted_stream_and_merges_result() {
    let frames = [
        frame(
            Some("message_start"),
            json!({"type": "message_start", "message": {
                "id": "msg_1", "model": "claude-test",
                "usage": {"input_tokens": 10, "output_tokens": 0, "cache_creation_input_tokens": 4, "cache_read_input_tokens": 2}
            }}),
        ),
        frame(
            Some("content_block_start"),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "weather", "input": {}}}),
        ),
        frame(
            Some("content_block_delta"),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"city\":"}}),
        ),
        frame(
            None,
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "\"東京\"}"}}),
        ),
        frame(
            Some("content_block_stop"),
            json!({"type": "content_block_stop", "index": 0}),
        ),
        frame(
            Some("content_block_delta"),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "完了"}}),
        ),
        frame(
            Some("message_delta"),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 8}}),
        ),
        frame(Some("message_stop"), json!({"type": "message_stop"})),
    ];
    let mut interpreter = AnthropicStreamInterpreter::new();

    let events = frames
        .iter()
        .map(|item| interpreter.interpret(item).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        events,
        vec![
            vec![],
            vec![StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("toolu_1".to_string()),
                name: Some("weather".to_string()),
                arguments_delta: String::new()
            }],
            vec![StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "{\"city\":".to_string()
            }],
            vec![StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "\"東京\"}".to_string()
            }],
            vec![],
            vec![StreamEvent::TextDelta {
                text: "完了".to_string()
            }],
            vec![],
            vec![],
        ]
    );
    assert!(interpreter.is_done());
    assert_eq!(interpreter.message_id(), Some("msg_1"));
    assert_eq!(interpreter.model(), Some("claude-test"));
    assert_eq!(
        interpreter.take_result(),
        (
            Usage {
                input_tokens: 10,
                output_tokens: 8,
                cache_read_tokens: 2,
                cache_write_tokens: 4
            },
            FinishReason::ToolUse,
        )
    );
}

// Given: thinking delta の複数行 JSON / When: event 名なしで解釈 / Then: JSON type から ReasoningDelta になる
#[test]
fn interpreter_parses_multiline_json_and_dispatches_from_json_type() {
    let data = "{\n\"type\":\"content_block_delta\",\n\"index\":2,\n\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"途中\"}\n}";
    let mut interpreter = AnthropicStreamInterpreter::new();

    let events = interpreter
        .interpret(&SseFrame {
            event: None,
            data: data.to_string(),
        })
        .unwrap();

    assert_eq!(
        events,
        vec![StreamEvent::ReasoningDelta {
            text: "途中".to_string()
        }]
    );
}

// Given: Anthropic error SSE / When: 解釈 / Then: detail を保持した HTTP 400 エラーになる
#[test]
fn interpreter_maps_error_event_to_typed_error() {
    let mut interpreter = AnthropicStreamInterpreter::new();

    let error = interpreter.interpret(&frame(
        Some("error"),
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    )).unwrap_err();

    assert_eq!(
        error,
        ProviderError::Http {
            status: 400,
            body: "overloaded_error: Overloaded".to_string()
        }
    );
}

// Given: 未知の Anthropic SSE type / When: 解釈 / Then: InvalidSse になる
#[test]
fn interpreter_rejects_unknown_event_type() {
    let mut interpreter = AnthropicStreamInterpreter::new();

    let error = interpreter
        .interpret(&frame(None, json!({"type": "future_event"})))
        .unwrap_err();

    assert!(matches!(error, ProviderError::InvalidSse { .. }));
}
