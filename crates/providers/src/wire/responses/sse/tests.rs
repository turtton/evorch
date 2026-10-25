mod custom;

use super::ResponsesStreamInterpreter;
use crate::error::ProviderError;
use crate::http::stream::WireStreamInterpreter;
use crate::message::{ChatResponse, ContentBlock, FinishReason, Usage};
use crate::sse::{SseFrame, SseParser};
use crate::stream::StreamAccumulator;

fn frames(contents: &str) -> Vec<SseFrame> {
    let mut parser = SseParser::new();
    let mut frames = parser.feed(contents.as_bytes()).unwrap();
    frames.extend(parser.finish().unwrap());
    frames
}

fn frame(event: &str, data: serde_json::Value) -> SseFrame {
    SseFrame {
        event: Some(event.to_owned()),
        data: data.to_string(),
    }
}

/// Feeds every frame and returns the completed response, or the first error.
fn interpret_all(frames: Vec<SseFrame>) -> Result<Option<ChatResponse>, ProviderError> {
    let mut interpreter = ResponsesStreamInterpreter::new();
    let mut accumulator = StreamAccumulator::default();
    let mut response = None;
    for frame in frames {
        let interpreted = interpreter.interpret(frame)?;
        for event in &interpreted.events {
            accumulator.feed(event);
        }
        if let Some((usage, reason)) = interpreted.completion {
            assert!(response.is_none(), "completion must be reported once");
            let mut completed = std::mem::take(&mut accumulator).finish(usage, reason);
            interpreter.finalize_message(&mut completed.message);
            response = Some(completed);
        }
    }
    Ok(response)
}

fn incomplete(details: serde_json::Value) -> SseFrame {
    frame(
        "response.incomplete",
        serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "status": "incomplete",
                "incomplete_details": details,
                "usage": {"input_tokens": 9, "output_tokens": 4,
                    "input_tokens_details": {"cached_tokens": 2},
                    "output_tokens_details": {"reasoning_tokens": 1}},
            },
        }),
    )
}

fn text_delta(text: &str) -> SseFrame {
    frame(
        "response.output_text.delta",
        serde_json::json!({"type": "response.output_text.delta", "delta": text}),
    )
}

fn function_call_done() -> SseFrame {
    frame(
        "response.output_item.done",
        serde_json::json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                "name": "read", "arguments": "{\"path\":\"a\"}"},
        }),
    )
}

// Given: an official fixture cut off by max_output_tokens
// When: interpreted / Then: the partial answer completes once as Length with its usage
#[test]
fn incomplete_fixture_completes_partial_output_as_length() {
    let response = interpret_all(frames(include_str!(
        "../../../../tests/fixtures/openai/responses_incomplete.sse"
    )))
    .unwrap()
    .expect("response.incomplete completes the stream");

    assert_eq!(response.finish_reason, FinishReason::Length);
    assert_eq!(
        response.message.content,
        vec![ContentBlock::Text {
            text: "Truncated answer".into()
        }]
    );
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 21,
            output_tokens: 16,
            cache_read_tokens: 5,
            cache_write_tokens: 0,
            reasoning_tokens: Some(14),
        }
    );
}

// Given: each documented and unknown incomplete reason, with and without a tool call
// When: interpreted / Then: known reasons map to their canonical reason, and unknown or
// missing reasons stay a non-normal Other while keeping the partial output and usage
#[test]
fn incomplete_reason_maps_to_canonical_finish_reason() {
    for (details, tool_call, expected) in [
        (
            serde_json::json!({"reason": "max_output_tokens"}),
            false,
            FinishReason::Length,
        ),
        (
            serde_json::json!({"reason": "content_filter"}),
            false,
            FinishReason::ContentFilter,
        ),
        (
            serde_json::json!({"reason": "max_output_tokens"}),
            true,
            FinishReason::Length,
        ),
        (
            serde_json::json!({"reason": "content_filter"}),
            true,
            FinishReason::ContentFilter,
        ),
        (
            serde_json::json!({"reason": "future_reason"}),
            false,
            FinishReason::Other("incomplete: future_reason".into()),
        ),
        (
            serde_json::json!({"reason": "future_reason"}),
            true,
            FinishReason::Other("incomplete: future_reason".into()),
        ),
        (
            serde_json::json!({}),
            true,
            FinishReason::Other("incomplete".into()),
        ),
        (
            serde_json::Value::Null,
            false,
            FinishReason::Other("incomplete".into()),
        ),
    ] {
        let mut stream = vec![text_delta("partial")];
        if tool_call {
            stream.push(function_call_done());
        }
        stream.push(incomplete(details.clone()));
        stream.push(text_delta(" ignored after completion"));

        let response = interpret_all(stream).unwrap().expect("completion");

        assert_eq!(response.finish_reason, expected, "{details}");
        let keep_tool =
            tool_call && matches!(expected, FinishReason::Length | FinishReason::ContentFilter);
        assert_eq!(
            response
                .message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. })),
            keep_tool,
            "{details}"
        );
        assert!(
            response.message.content.contains(&ContentBlock::Text {
                text: "partial".into()
            }),
            "{details}"
        );
        assert_eq!(response.usage.input_tokens, 9);
        assert_eq!(response.usage.cache_read_tokens, 2);
        assert_eq!(response.usage.reasoning_tokens, Some(1));
    }
}

// Given: an official top-level error event mid-stream
// When: interpreted / Then: it fails like response.failed, with `code: message`
#[test]
fn error_event_fixture_maps_to_http_error() {
    let error = interpret_all(frames(include_str!(
        "../../../../tests/fixtures/openai/responses_error_event.sse"
    )))
    .unwrap_err();

    assert_eq!(
        error,
        ProviderError::Http {
            status: 400,
            body: "context_length_exceeded: Your input exceeds the context window of this model."
                .into(),
        }
    );
}

/// Builds the same failure as a top-level `error` event and as `response.failed`.
fn failure_frames(code: Option<&str>, message: &str) -> [SseFrame; 2] {
    [
        frame(
            "error",
            serde_json::json!({"type": "error", "code": code, "message": message}),
        ),
        frame(
            "response.failed",
            serde_json::json!({"type": "response.failed",
                "response": {"status": "failed",
                    "error": {"code": code, "message": message}}}),
        ),
    ]
}

// Given: error and response.failed events carrying known transient codes, the overload code,
// an ordinary input error, and no code
// When: interpreted
// Then: server_error becomes a retryable 5xx, rate_limit_exceeded becomes RateLimited,
// overload stays a retryable 400, and input errors stay non-retryable 400s with their message
#[test]
fn failure_events_normalize_known_codes_into_retry_and_rate_limit_classes() {
    for (code, message, expected, retryable) in [
        (
            Some("server_error"),
            "The server had an error.",
            ProviderError::Http {
                status: 500,
                body: "server_error: The server had an error.".into(),
            },
            true,
        ),
        (
            Some("rate_limit_exceeded"),
            "Rate limit reached.",
            ProviderError::RateLimited { retry_after: None },
            false,
        ),
        (
            Some("server_is_overloaded"),
            "Try again later.",
            ProviderError::Http {
                status: 400,
                body: "server_is_overloaded: Try again later.".into(),
            },
            true,
        ),
        (
            Some("invalid_prompt"),
            "Invalid prompt.",
            ProviderError::Http {
                status: 400,
                body: "invalid_prompt: Invalid prompt.".into(),
            },
            false,
        ),
        (
            None,
            "Something went wrong.",
            ProviderError::Http {
                status: 400,
                body: "Something went wrong.".into(),
            },
            false,
        ),
    ] {
        for failure in failure_frames(code, message) {
            let error = interpret_all(vec![failure, text_delta("ignored")]).unwrap_err();

            assert_eq!(error, expected, "{code:?}");
            assert_eq!(crate::retry::is_retryable(&error), retryable, "{code:?}");
        }
    }
}

// Given: an official text fixture with a reasoning summary
// When: interpreted / Then: summary and visible text are separate canonical blocks
#[test]
fn text_fixture_keeps_reasoning_summary_and_text() {
    let response = interpret_all(frames(include_str!(
        "../../../../tests/fixtures/openai/responses_text.sse"
    )))
    .unwrap()
    .expect("completion");

    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        response.message.content,
        vec![
            ContentBlock::Reasoning {
                text: "Greeting the user.".into()
            },
            ContentBlock::Text {
                text: "Hello from OpenAI.".into()
            },
        ]
    );
    assert_eq!(response.usage.reasoning_tokens, Some(2));
}
