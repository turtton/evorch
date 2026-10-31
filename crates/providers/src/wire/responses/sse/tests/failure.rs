use std::time::Duration;

use serde_json::json;

use super::{frame, frames, interpret_all, interpret_all_in, text_delta};
use crate::ResponsesDialect::{Codex, OpenAi};
use crate::error::ProviderError;
use crate::wire::responses::ResponsesStreamInterpreter;

fn http(status: u16, body: &str) -> ProviderError {
    ProviderError::Http {
        status,
        body: body.into(),
    }
}

/// The failure as a flat `error` event, a nested `error` event, and `response.failed`.
fn shapes(error: serde_json::Value) -> [crate::sse::SseFrame; 3] {
    let flat = error.as_object().unwrap().clone();
    let mut flat_event = json!({"type": "error"});
    for key in ["code", "message"] {
        if let Some(value) = flat.get(key) {
            flat_event[key] = value.clone();
        }
    }
    [
        frame("error", flat_event),
        frame("error", json!({"type": "error", "error": error})),
        frame(
            "response.failed",
            json!({"type": "response.failed", "response": {"error": error}}),
        ),
    ]
}

fn fail(dialect: crate::ResponsesDialect, failure: crate::sse::SseFrame) -> ProviderError {
    interpret_all_in(dialect, vec![failure, text_delta("ignored")]).unwrap_err()
}

// Given: every error code Codex CLI 0.166.0 distinguishes, as flat error, nested error and
// response.failed events
// When: interpreted with the Codex dialect
// Then: each shape yields the same class, and the body starts with the code
#[test]
fn codex_dialect_classifies_each_code_for_every_event_shape() {
    for (code, status) in [
        ("server_error", 500),
        ("server_is_overloaded", 400),
        ("policy_capacity_unavailable", 503),
        ("insufficient_quota", 402),
        ("credit_balance_exhausted", 402),
        ("organization_spend_limit_exceeded", 402),
        ("project_spend_limit_exceeded", 402),
        ("usage_not_included", 402),
        ("usage_limit_reached", 402),
        ("context_length_exceeded", 400),
        ("model_not_found", 404),
        ("flex_unavailable", 400),
        ("invalid_prompt", 400),
        ("cyber_policy", 400),
        ("bio_policy", 400),
        ("misalignment_policy_violation", 400),
        ("invalid_request_error", 400),
    ] {
        for failure in shapes(json!({"code": code, "message": "details"})) {
            assert_eq!(
                fail(Codex, failure),
                http(status, &format!("{code}: details")),
                "{code}"
            );
        }
    }
}

// Given: rate_limit_exceeded and slow_down in every event shape, without any delay hint
// When: interpreted with the Codex dialect / Then: both become RateLimited with no delay
#[test]
fn codex_rate_limit_codes_become_rate_limited() {
    for code in ["rate_limit_exceeded", "slow_down"] {
        for failure in shapes(json!({"code": code, "message": "Slow down."})) {
            assert_eq!(
                fail(Codex, failure),
                ProviderError::RateLimited { retry_after: None },
                "{code}"
            );
        }
    }
}

// Given: the same codes under the OpenAI dialect (and the dialect-less constructor)
// When: interpreted / Then: only server_error and rate_limit_exceeded are special; others stay 400
#[test]
fn openai_dialect_keeps_its_existing_classification() {
    for (code, expected) in [
        ("server_error", http(500, "server_error: details")),
        (
            "rate_limit_exceeded",
            ProviderError::RateLimited { retry_after: None },
        ),
        ("slow_down", http(400, "slow_down: details")),
        (
            "usage_limit_reached",
            http(400, "usage_limit_reached: details"),
        ),
        (
            "insufficient_quota",
            http(400, "insufficient_quota: details"),
        ),
        ("model_not_found", http(400, "model_not_found: details")),
        (
            "policy_capacity_unavailable",
            http(400, "policy_capacity_unavailable: details"),
        ),
        ("unknown_code", http(400, "unknown_code: details")),
    ] {
        for failure in shapes(json!({"code": code, "message": "details",
            "headers": {"retry-after": "9"}}))
        {
            assert_eq!(fail(OpenAi, failure.clone()), expected, "{code}");
            let mut interpreter = ResponsesStreamInterpreter::new();
            let error =
                crate::http::stream::WireStreamInterpreter::interpret(&mut interpreter, failure)
                    .unwrap_err();
            assert_eq!(error, expected, "new(): {code}");
        }
    }
}

// Given: retry-after as integer seconds, in a differently cased header, and as a number
// When: a Codex rate limit failure is interpreted
// Then: the header delay is used, in preference to a delay in the message
#[test]
fn codex_rate_limit_uses_error_headers_before_the_message() {
    for headers in [
        json!({"retry-after": "7"}),
        json!({"Retry-After": "7"}),
        json!({"retry-after": 7}),
    ] {
        // The flat shape has no `error.headers`; top-level `headers` is not read.
        for failure in shapes(json!({"code": "rate_limit_exceeded",
            "message": "Please try again in 1.5s.", "headers": headers}))
        .into_iter()
        .skip(1)
        {
            assert_eq!(
                fail(Codex, failure),
                ProviderError::RateLimited {
                    retry_after: Some(Duration::from_secs(7))
                }
            );
        }
    }
}

// Given: a retry-after header holding a future HTTP-date and a past one
// When: a Codex rate limit failure is interpreted
// Then: the future date becomes the remaining delay and the past date becomes zero
#[test]
fn codex_rate_limit_accepts_http_date_retry_after() {
    let future = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(600));
    let [_, failure, _] = shapes(json!({"code": "rate_limit_exceeded", "message": "x",
        "headers": {"retry-after": future}}));
    let ProviderError::RateLimited {
        retry_after: Some(delay),
    } = fail(Codex, failure)
    else {
        panic!("expected a RateLimited error with a delay");
    };
    assert!(delay > Duration::from_secs(590) && delay <= Duration::from_secs(600));

    let [_, _, failure] = shapes(json!({"code": "rate_limit_exceeded", "message": "x",
        "headers": {"retry-after": "Wed, 21 Oct 2015 07:28:00 GMT"}}));
    assert_eq!(
        fail(Codex, failure),
        ProviderError::RateLimited {
            retry_after: Some(Duration::ZERO)
        }
    );
}

// Given: rate limit failures that carry the delay only in the message
// When: interpreted with the Codex dialect
// Then: the delay is extracted for rate_limit_exceeded and slow_down, and absent otherwise
#[test]
fn codex_rate_limit_extracts_the_delay_from_the_message() {
    for (code, message, expected) in [
        (
            "slow_down",
            "Slow down. Please try again in 2s.",
            Some(2_000),
        ),
        ("rate_limit_exceeded", "try again in 350ms", Some(350)),
        (
            "rate_limit_exceeded",
            "try again in 1.5 seconds",
            Some(1_500),
        ),
        ("rate_limit_exceeded", "No hint here.", None),
    ] {
        for failure in shapes(json!({"code": code, "message": message})) {
            assert_eq!(
                fail(Codex, failure),
                ProviderError::RateLimited {
                    retry_after: expected.map(Duration::from_millis)
                },
                "{message}"
            );
        }
    }
    // The message hint is not used for other codes.
    assert_eq!(
        fail(
            Codex,
            frame(
                "error",
                json!({"error": {"code": "server_error", "message": "try again in 2s"}})
            )
        ),
        http(500, "server_error: try again in 2s")
    );
}

// Given: nested error events with the nested fields, with only top-level fields, and with
// a mix of both
// When: interpreted under either dialect / Then: none is InvalidSse and nested fields win
#[test]
fn error_events_accept_nested_and_top_level_fields() {
    for dialect in [OpenAi, Codex] {
        for (event, expected) in [
            (
                json!({"type": "error", "error": {"code": "invalid_prompt", "message": "nested"}}),
                http(400, "invalid_prompt: nested"),
            ),
            (
                json!({"type": "error", "code": "invalid_prompt", "message": "flat"}),
                http(400, "invalid_prompt: flat"),
            ),
            (
                json!({"type": "error", "code": "ignored", "message": "top",
                    "error": {"code": "invalid_prompt"}}),
                http(400, "invalid_prompt: top"),
            ),
            (
                json!({"type": "error", "error": "plain message"}),
                http(400, "plain message"),
            ),
            (
                json!({"type": "error", "error": {"code": "server_error"}}),
                http(500, "server_error"),
            ),
            (
                json!({"type": "error"}),
                http(400, "stream failure event received"),
            ),
        ] {
            assert_eq!(
                fail(dialect, frame("error", event)),
                expected,
                "{dialect:?}"
            );
        }
    }
}

// Given: response.failed without a message / When: interpreted
// Then: the failure is still reported by its code instead of InvalidSse
#[test]
fn response_failed_without_message_is_reported_by_code() {
    let failure = frame(
        "response.failed",
        json!({"type": "response.failed", "response": {"error": {"code": "invalid_prompt"}}}),
    );

    assert_eq!(fail(Codex, failure), http(400, "invalid_prompt"));
}

// Given: the Codex failure fixtures / When: interpreted with each dialect
// Then: Codex gets the specific class and the OpenAI dialect keeps its existing 400 / RateLimited
#[test]
fn failure_fixtures_classify_per_dialect() {
    let rate_limit =
        include_str!("../../../../../tests/fixtures/codex/responses_failed_rate_limit_headers.sse");
    let usage =
        include_str!("../../../../../tests/fixtures/codex/responses_failed_usage_limit.sse");
    let context =
        include_str!("../../../../../tests/fixtures/codex/responses_failed_context_length.sse");
    let nested =
        include_str!("../../../../../tests/fixtures/codex/responses_failed_nested_error.sse");
    let run = |dialect, fixture| interpret_all_in(dialect, frames(fixture)).unwrap_err();

    assert_eq!(
        run(Codex, rate_limit),
        ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(7))
        }
    );
    assert_eq!(
        run(OpenAi, rate_limit),
        ProviderError::RateLimited { retry_after: None }
    );
    assert_eq!(
        run(Codex, usage),
        http(402, "usage_limit_reached: The usage limit has been reached")
    );
    assert_eq!(
        run(OpenAi, usage),
        http(400, "usage_limit_reached: The usage limit has been reached")
    );
    let context_body =
        "context_length_exceeded: Your input exceeds the context window of this model.";
    for dialect in [Codex, OpenAi] {
        let error = run(dialect, context);
        assert_eq!(error, http(400, context_body));
        assert!(error.is_context_window_exceeded());
    }
    assert_eq!(
        run(Codex, nested),
        http(404, "model_not_found: The model `gpt-9` does not exist")
    );
    assert_eq!(
        run(OpenAi, nested),
        http(400, "model_not_found: The model `gpt-9` does not exist")
    );
}

// Given: a Codex rate limit failure followed by more events / When: interpreted
// Then: the stream ends at the failure (evorch terminates on every failure event)
#[test]
fn failure_events_terminate_the_stream_in_both_dialects() {
    for dialect in [OpenAi, Codex] {
        let result = interpret_all_in(
            dialect,
            vec![
                frame(
                    "error",
                    json!({"error": {"code": "slow_down", "message": "m"}}),
                ),
                text_delta("never reached"),
            ],
        );
        assert!(result.is_err(), "{dialect:?}");
    }
    assert!(interpret_all(vec![text_delta("ok")]).unwrap().is_none());
}

// Given: Codex rate limits whose header or message delay cannot become a deadline
// When: interpreted / Then: RateLimited carries no delay instead of an overflowing one
#[test]
fn codex_rate_limit_drops_delays_that_overflow_a_deadline() {
    let huge_header = u64::MAX.to_string();
    for error in [
        json!({"code": "rate_limit_exceeded", "message": "x",
            "headers": {"retry-after": huge_header}}),
        json!({"code": "slow_down", "message": "try again in 9223372036854775807s"}),
    ] {
        for failure in shapes(error).into_iter().skip(1) {
            assert_eq!(
                fail(Codex, failure),
                ProviderError::RateLimited { retry_after: None }
            );
        }
    }
}
