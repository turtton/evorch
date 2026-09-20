use super::super::*;
use crate::error::ProviderError;
use crate::message::{FinishReason, Usage};
use crate::sse::SseFrame;
use crate::stream::StreamEvent;
use serde_json::json;

fn frame(data: serde_json::Value) -> SseFrame {
    SseFrame {
        event: None,
        data: data.to_string(),
    }
}

// Given: テキストと交互の tool call 断片、finish_reason、usage、DONE / When: 順番に解釈 / Then: exact delta 列と完了メタデータを得る
#[test]
fn stream_interpreter_emits_deltas_and_captures_completion_parts() {
    let frames = [
        frame(json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hi "}, "finish_reason": null}]
        })),
        frame(json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call_0", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":"}},
                {"index": 1, "id": "call_1", "type": "function", "function": {"name": "time", "arguments": "{\"zone\":"}}
            ]}, "finish_reason": null}]
        })),
        frame(json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "delta": {"content": "done", "tool_calls": [
                {"index": 1, "function": {"arguments": "\"JST\"}"}},
                {"index": 0, "function": {"arguments": "\"Tokyo\"}"}}
            ]}, "finish_reason": "tool_calls"}]
        })),
        frame(json!({
            "id": "chatcmpl-1",
            "choices": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13, "prompt_tokens_details": {"cached_tokens": 2}}
        })),
        SseFrame {
            event: None,
            data: "[DONE]".to_string(),
        },
    ];
    let mut interpreter = OpenAiStreamInterpreter::new();

    let actual = frames
        .iter()
        .map(|frame| interpreter.interpret(frame))
        .collect::<Result<Vec<_>, _>>()
        .expect("stream fixtures must parse")
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    assert_eq!(
        actual,
        vec![
            StreamEvent::TextDelta {
                text: "Hi ".to_string()
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_0".to_string()),
                name: Some("weather".to_string()),
                arguments_delta: "{\"city\":".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                id: Some("call_1".to_string()),
                name: Some("time".to_string()),
                arguments_delta: "{\"zone\":".to_string(),
            },
            StreamEvent::TextDelta {
                text: "done".to_string()
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                id: None,
                name: None,
                arguments_delta: "\"JST\"}".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "\"Tokyo\"}".to_string(),
            },
        ]
    );
    assert!(interpreter.is_done());
    assert_eq!(
        interpreter.take_result(),
        (
            Some(Usage {
                input_tokens: 10,
                output_tokens: 3,
                cache_read_tokens: 2,
                cache_write_tokens: 0,
            }),
            Some(FinishReason::ToolUse)
        )
    );
}

// Given: DONE frame / When: 解釈 / Then: event を出さず done 状態になる
#[test]
fn done_frame_only_marks_interpreter_done() {
    let mut interpreter = OpenAiStreamInterpreter::new();
    let frame = SseFrame {
        event: None,
        data: "[DONE]".to_string(),
    };

    let events = interpreter.interpret(&frame).expect("DONE must be valid");

    assert!(events.is_empty());
    assert!(interpreter.is_done());
}

// Given: JSON ではない chunk / When: 解釈 / Then: InvalidJson を返す
#[test]
fn malformed_stream_chunk_returns_invalid_json() {
    let mut interpreter = OpenAiStreamInterpreter::new();
    let frame = SseFrame {
        event: None,
        data: "{not-json".to_string(),
    };

    let error = interpreter
        .interpret(&frame)
        .expect_err("malformed chunk must fail");

    assert!(matches!(error, ProviderError::InvalidJson { .. }));
}

// Given: 未知フィールドのみの chunk / When: 解釈して通常の差分を続ける / Then: 状態を変えずスキップし TextDelta を返す
#[test]
fn structurally_unknown_stream_chunk_is_skipped_without_state_change() {
    assert_chunk_is_skipped_without_state_change(json!({"unexpected": true}));
}

// Given: choices が空の metadata chunk / When: 解釈して通常の差分を続ける / Then: 状態を変えずスキップし TextDelta を返す
#[test]
fn metadata_stream_chunk_is_skipped_without_state_change() {
    assert_chunk_is_skipped_without_state_change(json!({
        "id": "chatcmpl-x",
        "model": "kimi-k3",
        "object": "chat.completion.chunk",
        "created": 1,
        "choices": []
    }));
}

fn assert_chunk_is_skipped_without_state_change(chunk: serde_json::Value) {
    let mut interpreter = OpenAiStreamInterpreter::new();
    let seed = frame(json!({
        "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 2}}
    }));
    assert!(
        interpreter
            .interpret(&seed)
            .expect("seed must parse")
            .is_empty()
    );

    let events = interpreter
        .interpret(&frame(chunk))
        .expect("inactive chunk must be skipped");
    let content = interpreter
        .interpret(&frame(json!({"choices": [{"delta": {"content": "Hi"}}]})))
        .expect("content after skip must parse");

    assert!(events.is_empty());
    assert!(!interpreter.is_done());
    assert_eq!(
        content,
        vec![StreamEvent::TextDelta {
            text: "Hi".to_string()
        }]
    );
    assert_eq!(
        interpreter.take_result(),
        (
            Some(Usage {
                input_tokens: 10,
                output_tokens: 3,
                cache_read_tokens: 2,
                cache_write_tokens: 0,
            }),
            Some(FinishReason::ToolUse)
        )
    );
}

// Given: provider の error payload / When: 解釈 / Then: 原因メッセージを含む Request エラーを返す
#[test]
fn error_stream_chunk_returns_request_with_provider_message() {
    let mut interpreter = OpenAiStreamInterpreter::new();
    let frame = frame(json!({"error": {
        "message": "context length exceeded",
        "type": "invalid_request_error",
        "code": "context_length_exceeded"
    }}));

    let error = interpreter
        .interpret(&frame)
        .expect_err("provider error chunk must fail");

    assert!(error.to_string().contains("context length exceeded"));
    assert!(matches!(error, ProviderError::Request(_)));
}
