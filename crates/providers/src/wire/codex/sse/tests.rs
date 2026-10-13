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

fn canonical_response(contents: &str) -> Result<crate::message::ChatResponse, ProviderError> {
    let mut interpreter = CodexStreamInterpreter::new();
    let mut accumulator = StreamAccumulator::default();
    for frame in fixture_frames(contents) {
        let interpreted = interpreter.interpret(frame)?;
        for event in &interpreted.events {
            accumulator.feed(event);
        }
        if let Some((usage, reason)) = interpreted.completion {
            return Ok(accumulator.finish(usage, reason));
        }
    }
    panic!("fixture must contain a completion")
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

// Given: parallel read calls with missing/partial deltas and reversed done events
// When: accumulated / Then: each call retains its own final arguments without duplication
#[test]
fn interpreter_recovers_parallel_final_arguments() {
    let frames = fixture_frames(include_str!(
        "../../../../tests/fixtures/codex/responses_parallel_tool_calls.sse"
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
    assert!(accumulator.malformed_tool_calls().is_empty());
    let (usage, reason) = completion.unwrap();
    let response = accumulator.finish(usage, reason);
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    assert_eq!(
        response.message.content,
        vec![
            ContentBlock::ToolUse {
                id: "call-read-1".into(),
                name: "read".into(),
                input: serde_json::json!({"path":"Cargo.toml","limit":10}),
            },
            ContentBlock::ToolUse {
                id: "call-read-2".into(),
                name: "read".into(),
                input: serde_json::json!({"path":"crates/providers/Cargo.toml","offset":2}),
            },
        ]
    );
}

// Given: only one kind of final event survives / When: accumulated
// Then: arguments.done and output_item.done independently recover both calls
#[test]
fn either_final_event_recovers_missing_arguments() {
    let fixture =
        include_str!("../../../../tests/fixtures/codex/responses_parallel_tool_calls.sse");
    for omitted in [
        "response.function_call_arguments.done",
        "response.output_item.done",
        "response.output_item.added",
    ] {
        let contents = fixture
            .split("\n\n")
            .filter(|frame| !frame.starts_with(&format!("event: {omitted}\n")))
            .collect::<Vec<_>>()
            .join("\n\n");
        let response = canonical_response(&contents).unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        assert_eq!(
            response.message.content,
            vec![
                ContentBlock::ToolUse {
                    id: "call-read-1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path":"Cargo.toml","limit":10}),
                },
                ContentBlock::ToolUse {
                    id: "call-read-2".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path":"crates/providers/Cargo.toml","offset":2}),
                },
            ],
            "omitted {omitted}"
        );
    }
}

// Given: added items already contain full arguments / When: no deltas arrive
// Then: the initial snapshot is preserved and final snapshots are not duplicated
#[test]
fn added_item_arguments_are_preserved() {
    let fixture = include_str!("../../../../tests/fixtures/codex/responses_tool_call.sse");
    let contents = fixture
        .split("\n\n")
        .filter(|frame| !frame.starts_with("event: response.function_call_arguments.delta\n"))
        .map(|frame| {
            if frame.starts_with("event: response.output_item.added\n") {
                frame.replace(
                    r#""arguments":"""#,
                    r#""arguments":"{\"path\":\"Cargo.toml\"}""#,
                )
            } else {
                frame.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let response = canonical_response(&contents).unwrap();
    assert_eq!(
        response.message.content,
        vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"Cargo.toml"}),
        }]
    );
}

// Given: final snapshots disagree with received deltas / When: interpreted
// Then: fail closed without leaking either argument body in the diagnostic
#[test]
fn conflicting_final_arguments_are_rejected() {
    let fixture = include_str!("../../../../tests/fixtures/codex/responses_tool_call.sse");
    for final_event in [
        "response.function_call_arguments.done",
        "response.output_item.done",
    ] {
        let contents = fixture
            .split("\n\n")
            .filter(|frame| {
                final_event != "response.output_item.done"
                    || !frame.starts_with("event: response.function_call_arguments.done\n")
            })
            .map(|frame| {
                if frame.starts_with(&format!("event: {final_event}\n")) {
                    frame.replace("Cargo.toml", "private-final-path")
                } else {
                    frame.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let error = canonical_response(&contents).unwrap_err();
        assert!(matches!(error, ProviderError::InvalidSse { .. }));
        assert!(!error.to_string().contains("Cargo.toml"));
        assert!(!error.to_string().contains("private-final-path"));
    }
}

// Given: hosted or future output item types / When: added or done
// Then: they remain ignored rather than breaking otherwise valid responses
#[test]
fn non_function_output_items_are_ignored() {
    for event_type in ["response.output_item.added", "response.output_item.done"] {
        for item_type in ["message", "reasoning", "web_search_call", "future_item"] {
            let mut interpreter = CodexStreamInterpreter::new();
            let result = interpreter
                .interpret(SseFrame {
                    event: Some(event_type.into()),
                    data: serde_json::json!({
                        "type": event_type,
                        "output_index": 0,
                        "item": {"type": item_type},
                    })
                    .to_string(),
                })
                .unwrap();
            assert!(result.events.is_empty());
            assert!(result.completion.is_none());
        }
    }
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

// Given: schema validation or unsupported-format failures in SSE / When: parsed and classified
// Then: validation errors retain fail-closed behavior even after the error code is flattened.
#[test]
fn response_failed_schema_errors_do_not_allow_structured_output_fallback() {
    for (code, message, unsupported) in [
        (
            "invalid_json_schema",
            "json_schema contains unsupported type integer",
            false,
        ),
        (
            "schema_validation_error",
            "json_schema contains unsupported type integer",
            false,
        ),
        (
            "unsupported_parameter",
            "json_schema is not supported with this model",
            true,
        ),
    ] {
        let payload = serde_json::json!({
            "type": "response.failed",
            "response": {"error": {"code": code, "message": message}},
        });
        let frames = fixture_frames(&format!("event: response.failed\ndata: {payload}\n\n"));
        let mut interpreter = CodexStreamInterpreter::new();
        let error = frames
            .into_iter()
            .find_map(|frame| interpreter.interpret(frame).err())
            .unwrap();
        assert!(matches!(error, ProviderError::Http { status: 400, .. }));
        assert_eq!(
            error.is_structured_output_unsupported(),
            unsupported,
            "{error}"
        );
    }
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

// Given: stream containing an unknown lifecycle event / When: interpreted / Then: the unknown event is skipped and later deltas still flow
#[test]
fn interpreter_skips_unknown_events() {
    let mut interpreter = CodexStreamInterpreter::new();

    let skipped = interpreter
        .interpret(SseFrame {
            event: Some("response.in_progress".to_string()),
            data: r#"{"type":"response.in_progress","response":{"status":"in_progress"}}"#
                .to_string(),
        })
        .unwrap();
    let delta = interpreter
        .interpret(SseFrame {
            event: Some("response.output_text.delta".to_string()),
            data: r#"{"type":"response.output_text.delta","delta":"ok"}"#.to_string(),
        })
        .unwrap();

    assert!(skipped.events.is_empty());
    assert!(skipped.completion.is_none());
    assert_eq!(
        delta.events,
        vec![StreamEvent::TextDelta {
            text: "ok".to_string()
        }]
    );
}

// Given: a completed response reporting a reasoning breakdown / When: interpreted
// Then: reasoning tokens are kept as a subset of output, absent ones stay unknown
#[test]
fn completion_usage_reports_reasoning_breakdown_when_present() {
    for (details, expected) in [
        (
            r#","output_tokens_details":{"reasoning_tokens":5}"#,
            Some(5),
        ),
        ("", None),
    ] {
        let mut interpreter = CodexStreamInterpreter::new();
        let interpreted = interpreter
            .interpret(SseFrame {
                event: Some("response.completed".to_string()),
                data: format!(
                    r#"{{"type":"response.completed","response":{{"status":"completed","usage":{{"input_tokens":12,"output_tokens":9{details}}}}}}}"#
                ),
            })
            .unwrap();

        let (usage, _) = interpreted.completion.unwrap();
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.reasoning_tokens, expected);
    }
}
