mod custom;
mod failure;

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
    interpret_all_in(crate::ResponsesDialect::OpenAi, frames)
}

/// Same as [`interpret_all`], bound to a scope of the given dialect.
fn interpret_all_in(
    dialect: crate::ResponsesDialect,
    frames: Vec<SseFrame>,
) -> Result<Option<ChatResponse>, ProviderError> {
    let mut interpreter =
        ResponsesStreamInterpreter::with_origin(crate::ResolvedResponsesScope::resolve(
            &serde_json::from_value(serde_json::json!({"model":"m","messages":[]})).unwrap(),
            dialect,
            Some("test"),
            "https://example.test/v1/responses",
            crate::ResponsesAccount::Unavailable,
        ));
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
            context_tokens: None,
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

#[test]
fn b3_done_reasoning_is_retained_not_added() {
    let item = |cipher: &str| serde_json::json!({"type":"reasoning", "id":"rs_1", "summary":[], "encrypted_content":cipher, "status":"completed", "future":{"日本語":"\n"}});
    let response = interpret_all(vec![
        frame(
            "response.output_item.added",
            serde_json::json!({"output_index":0,"item":item("partial-A")}),
        ),
        frame(
            "response.output_item.done",
            serde_json::json!({"output_index":0,"item":item("final-B")}),
        ),
        frame(
            "response.completed",
            serde_json::json!({"response":{"usage":{"input_tokens":1,"output_tokens":1}}}),
        ),
    ])
    .unwrap()
    .unwrap();
    let content = serde_json::to_value(response.message.content).unwrap();
    assert_eq!(content[0]["type"], "responses_reasoning");
    assert_eq!(content[0]["item"], item("final-B"));
    assert!(!content.to_string().contains("partial-A"));
}

fn b3_item(id: &str, cipher: &str) -> serde_json::Value {
    serde_json::json!({"type":"reasoning","id":id,"summary":[],"encrypted_content":cipher,"status":"completed"})
}
fn b3_done(index: u64, item: serde_json::Value) -> SseFrame {
    frame(
        "response.output_item.done",
        serde_json::json!({"output_index":index,"item":item}),
    )
}
fn b3_complete() -> SseFrame {
    frame(
        "response.completed",
        serde_json::json!({"response":{"usage":{"input_tokens":9,"output_tokens":4,"output_tokens_details":{"reasoning_tokens":2}}}}),
    )
}

#[test]
fn b3_multiple_items_restore_text_and_validated_parallel_tools_in_output_order() {
    use serde_json::json;
    let response = interpret_all(vec![
        b3_done(0, b3_item("rs_0", "zero")),
        frame("response.reasoning_summary_text.delta", json!({"delta":"summary once"})),
        frame("response.output_text.delta", json!({"output_index":1,"delta":"text A"})),
        b3_done(1, json!({"type":"message","id":"msg1","content":[{"type":"output_text","text":"text A"}]})),
        b3_done(2, b3_item("rs_2", "two")),
        frame("response.output_text.delta", json!({"output_index":3,"delta":"text B"})),
        b3_done(3, json!({"type":"message","id":"msg3","content":[{"type":"output_text","text":"text B"}]})),
        b3_done(4, json!({"type":"function_call","id":"fc4","call_id":"call4","name":"read","arguments":"malformed"})),
        b3_done(5, json!({"type":"custom_tool_call","id":"ct5","call_id":"call5","name":"apply_patch","input":"patch 日本語\n"})),
        b3_complete(),
    ]).unwrap().unwrap();
    let blocks = serde_json::to_value(&response.message.content).unwrap();
    assert_eq!(
        blocks
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "reasoning",
            "responses_reasoning",
            "text",
            "responses_reasoning",
            "text",
            "tool_use",
            "tool_use"
        ]
    );
    assert_eq!(blocks[0]["text"], "summary once");
    assert_eq!(blocks[1]["item"]["id"], "rs_0");
    assert_eq!(blocks[3]["item"]["id"], "rs_2");
    assert_eq!(blocks[5]["input"], serde_json::Value::Null);
    assert_eq!(blocks[6]["input"], json!({"input":"patch 日本語\n"}));
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    assert_eq!(response.usage.reasoning_tokens, Some(2));
}

#[test]
fn b3_unconfirmed_malformed_or_unpositioned_items_are_not_retained() {
    use serde_json::json;
    let valid = b3_item("rs_1", "secret-marker");
    let mut malformed = vec![];
    for (key, value) in [
        ("id", json!(null)),
        ("id", json!("")),
        ("summary", json!({})),
        ("encrypted_content", json!("")),
        ("encrypted_content", json!(null)),
        ("encrypted_content", json!(123)),
        ("status", json!("incomplete")),
        ("status", json!("in_progress")),
    ] {
        let mut item = valid.clone();
        item[key] = value;
        malformed.push(item);
    }
    for item in malformed {
        let result = interpret_all(vec![b3_done(0, item), b3_complete()])
            .unwrap()
            .unwrap();
        assert!(
            !serde_json::to_string(&result.message)
                .unwrap()
                .contains("responses_reasoning")
        );
    }
    for event in ["response.output_item.added", "response.output_item.done"] {
        let mut value = json!({"item":valid});
        if event.ends_with("added") {
            value["output_index"] = json!(0);
        }
        let result = interpret_all(vec![frame(event, value), b3_complete()])
            .unwrap()
            .unwrap();
        assert!(result.message.content.is_empty());
    }
    let result = interpret_all(vec![
        b3_done(0, valid),
        text_delta("legacy no index"),
        b3_complete(),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(
        result.message.content,
        vec![ContentBlock::Text {
            text: "legacy no index".into()
        }]
    );
}

#[test]
fn b3_duplicate_done_is_idempotent_conflicts_are_payload_free() {
    let item = b3_item("rs_1", "secret-marker");
    let result = interpret_all(vec![
        b3_done(0, item.clone()),
        b3_done(0, item.clone()),
        b3_complete(),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(result.message.content.len(), 1);
    for (index, next) in [
        (0, b3_item("rs_1", "different-secret")),
        (0, b3_item("rs_2", "secret-marker")),
        (1, item.clone()),
    ] {
        let error =
            interpret_all(vec![b3_done(0, item.clone()), b3_done(index, next)]).unwrap_err();
        assert!(matches!(error, ProviderError::InvalidSse { .. }));
        assert!(!format!("{error:?}").contains("secret-marker"));
        assert!(!error.to_string().contains("different-secret"));
    }
}

#[test]
fn b3_large_ciphertext_unknown_fields_and_incomplete_keep_only_confirmed_items() {
    let cipher = "密\n🔒".repeat(100_000);
    let mut item = b3_item("rs_1", &cipher);
    item["summary"] = serde_json::json!([{"type":"summary_text","text":"summary"}]);
    item["future"] = serde_json::json!({"unknown":"\n日本語"});
    let result = interpret_all(vec![
        b3_done(0, item.clone()),
        incomplete(serde_json::json!({"reason":"max_output_tokens"})),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(
        serde_json::to_value(&result.message.content).unwrap()[0]["item"],
        item
    );
    assert_eq!(result.finish_reason, FinishReason::Length);
}

#[test]
fn b3_added_without_completion_does_not_create_a_message_and_index_conflicts_fail_closed() {
    use serde_json::json;
    assert!(
        interpret_all(vec![frame(
            "response.output_item.added",
            json!({"output_index":0,"item":b3_item("rs1","partial-secret")})
        )])
        .unwrap()
        .is_none()
    );
    for events in [
        vec![
            b3_done(0, b3_item("rs1", "secret")),
            frame(
                "response.output_text.delta",
                json!({"output_index":0,"delta":"text"}),
            ),
        ],
        vec![
            frame(
                "response.output_text.delta",
                json!({"output_index":0,"delta":"text"}),
            ),
            b3_done(0, b3_item("rs1", "secret")),
        ],
        vec![
            frame(
                "response.output_item.added",
                json!({"output_index":0,"item":b3_item("rs1","partial")}),
            ),
            b3_done(0, json!({"type":"message","id":"rs1","content":[]})),
        ],
    ] {
        assert!(matches!(
            interpret_all(events),
            Err(ProviderError::InvalidSse { .. })
        ));
    }
    let failure = frame(
        "response.failed",
        json!({"response":{"error":{"code":"invalid_prompt","message":"failed"}}}),
    );
    assert!(
        interpret_all(vec![
            frame(
                "response.output_item.added",
                json!({"output_index":0,"item":b3_item("rs1","partial")})
            ),
            failure
        ])
        .is_err()
    );
}

#[test]
fn b3_review_m1_interpreter_debug_redacts_pending_and_confirmed_ciphertext() {
    use serde_json::json;
    let mut interpreter = ResponsesStreamInterpreter::new();
    for (event, sentinel) in [
        (
            "response.output_item.added",
            "m1-partial-ciphertext-sentinel",
        ),
        ("response.output_item.done", "m1-final-ciphertext-sentinel"),
    ] {
        interpreter
            .interpret(frame(
                event,
                json!({"output_index":0,"item":b3_item("rs_debug",sentinel)}),
            ))
            .unwrap();
        for debug in [
            format!("{interpreter:?}"),
            format!("{interpreter:#?}"),
            format!("{:?}", interpreter.ordering),
        ] {
            assert!(
                !debug.contains(sentinel),
                "interpreter/order Debug exposed ciphertext"
            );
        }
    }
}

#[test]
fn b3_review_m2_equal_concatenation_cannot_prove_text_boundaries() {
    use serde_json::json;
    for (left_done, right_done) in [("AB", ""), ("", "AB")] {
        let response = interpret_all(vec![
            frame("response.output_text.delta",json!({"output_index":0,"delta":"A"})),
            b3_done(0,json!({"type":"message","id":"msg0","content":[{"type":"output_text","text":left_done}]})),
            b3_done(1,b3_item("rs1","m2-ciphertext")),
            frame("response.output_text.delta",json!({"output_index":2,"delta":"B"})),
            b3_done(2,json!({"type":"message","id":"msg2","content":[{"type":"output_text","text":right_done}]})),
            b3_complete(),
        ]).unwrap().unwrap();
        assert_eq!(
            response.message.content,
            vec![ContentBlock::Text { text: "AB".into() }],
            "index mismatch must preserve legacy text without an opaque item"
        );
    }
}

#[test]
fn b3_review_m2_matching_per_index_chunks_preserve_reasoning_boundary() {
    use serde_json::json;
    let response = interpret_all(vec![
        frame("response.output_text.delta",json!({"output_index":0,"delta":"A"})),
        frame("response.output_text.delta",json!({"output_index":0,"delta":"あ"})),
        b3_done(0,json!({"type":"message","id":"msg0","content":[{"type":"output_text","text":"A"},{"type":"output_text","text":"あ"}]})),
        b3_done(1,b3_item("rs1","ciphertext")),
        frame("response.output_text.delta",json!({"output_index":2,"delta":"B"})),
        b3_done(2,json!({"type":"message","id":"msg2","content":[{"type":"output_text","text":"B"}]})),
        b3_complete(),
    ]).unwrap().unwrap();
    assert_eq!(response.message.content.len(), 4);
    assert_eq!(
        response.message.content[0],
        ContentBlock::Text { text: "A".into() }
    );
    assert_eq!(
        response.message.content[1],
        ContentBlock::Text { text: "あ".into() }
    );
    assert!(
        matches!(&response.message.content[2],ContentBlock::ResponsesReasoning {item,..} if item["id"] == "rs1")
    );
    assert_eq!(
        response.message.content[3],
        ContentBlock::Text { text: "B".into() }
    );
}
