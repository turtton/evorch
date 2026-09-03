use super::CodexStreamInterpreter;
use crate::error::ProviderError;
use crate::http::stream::WireStreamInterpreter;
use crate::message::{ContentBlock, FinishReason};
use crate::sse::{SseFrame, SseParser};
use crate::stream::{StreamAccumulator, StreamEvent};

fn fixture_frames(contents: &str) -> Vec<SseFrame> {
    let mut parser = SseParser::new();
    let mut frames = parser.feed(contents.as_bytes()).unwrap();
    frames.extend(parser.finish().unwrap());
    frames
}

// Given: successful Responses SSE fixture / When: interpreted / Then: text deltas and usage completion are emitted once
#[test]
fn interpreter_emits_text_deltas_and_completion() {
    let frames = fixture_frames(include_str!(
        "../../../../tests/fixtures/codex/responses_success.sse"
    ));
    let mut interpreter = CodexStreamInterpreter::new();
    let mut events = Vec::new();
    let mut completion = None;

    for frame in frames {
        let interpreted = interpreter.interpret(frame).unwrap();
        events.extend(interpreted.events);
        completion = completion.or(interpreted.completion);
    }
    let after_done = interpreter
        .interpret(SseFrame {
            event: Some("response.output_text.delta".to_string()),
            data: r#"{"type":"response.output_text.delta","delta":"ignored"}"#.to_string(),
        })
        .unwrap();

    assert_eq!(
        events,
        vec![
            StreamEvent::TextDelta {
                text: "Hello".to_string()
            },
            StreamEvent::TextDelta {
                text: " world".to_string()
            }
        ]
    );
    let (usage, reason) = completion.unwrap();
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(usage.cache_read_tokens, 0);
    assert_eq!(reason, FinishReason::Stop);
    assert!(after_done.events.is_empty());
    assert!(after_done.completion.is_none());
}

// Given: function-call Responses SSE fixture / When: deltas are interpreted and accumulated / Then: one canonical ToolUse is assembled
#[test]
fn interpreter_emits_tool_call_events() {
    let frames = fixture_frames(include_str!(
        "../../../../tests/fixtures/codex/responses_tool_call.sse"
    ));
    let mut interpreter = CodexStreamInterpreter::new();
    let mut accumulator = StreamAccumulator::default();
    let mut completion = None;

    for frame in frames {
        let interpreted = interpreter.interpret(frame).unwrap();
        for event in &interpreted.events {
            accumulator.feed(event);
        }
        completion = completion.or(interpreted.completion);
    }
    let (usage, reason) = completion.unwrap();
    let response = accumulator.finish(usage, reason);

    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    assert_eq!(
        response.message.content,
        vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path":"Cargo.toml"}),
        }]
    );
}

// Given: response.failed fixture / When: interpreted / Then: provider HTTP error preserves wire details
#[test]
fn interpreter_maps_response_failed_to_error() {
    let frames = fixture_frames(include_str!(
        "../../../../tests/fixtures/codex/responses_failed.sse"
    ));
    let mut interpreter = CodexStreamInterpreter::new();

    let error = frames
        .into_iter()
        .find_map(|frame| interpreter.interpret(frame).err())
        .unwrap();

    assert_eq!(
        error,
        ProviderError::Http {
            status: 400,
            body: "invalid_request_error: request rejected".to_string(),
        }
    );
}

// Given: malformed JSON frame / When: interpreted / Then: the boundary returns InvalidJson
#[test]
fn interpreter_rejects_invalid_json_frame() {
    let mut interpreter = CodexStreamInterpreter::new();

    let error = interpreter
        .interpret(SseFrame {
            event: Some("response.output_text.delta".to_string()),
            data: "{garbage".to_string(),
        })
        .unwrap_err();

    assert!(matches!(error, ProviderError::InvalidJson { .. }));
}
