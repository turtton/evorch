use crate::event::{LifecycleEvent, ProviderEvent, ProviderFailureKind, ToolEvent};

use super::super::{SpanAction, SpanMapper, SpanStatus};
use super::{action_attributes, event, i64_attr, start_run, str_attr, strings_attr};

#[test]
fn request_success_ends_with_usage_and_finish_reason_attributes() {
    // Given: an open request span.
    let mut mapper = SpanMapper::new();
    mapper.ingest(&start_run("run-1", None, 1));
    mapper.ingest(&event(
        ProviderEvent::RequestStarted {
            request_id: "request-1".to_owned(),
            provider: "anthropic".to_owned(),
            profile: None,
            protocol: "anthropic-messages".to_owned(),
            model: "claude-test".to_owned(),
            streaming: false,
            run_id: Some("run-1".to_owned()),
        },
        2,
    ));
    // When: the request completes.
    let actions = mapper.ingest(&event(
        ProviderEvent::RequestCompleted {
            request_id: "request-1".to_owned(),
            provider: "anthropic".to_owned(),
            profile: None,
            protocol: "anthropic-messages".to_owned(),
            model: "claude-test".to_owned(),
            streaming: false,
            duration_ms: 10,
            input_tokens: 11,
            output_tokens: 12,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: "tool_use".to_owned(),
            run_id: Some("run-1".to_owned()),
        },
        3,
    ));
    // Then: terminal attributes follow start attrs in deterministic order.
    assert!(matches!(
        actions[0],
        SpanAction::End {
            status: SpanStatus::Unset,
            ..
        }
    ));
    assert_eq!(
        &action_attributes(&actions[0])[5..],
        [
            i64_attr("gen_ai.usage.input_tokens", 11),
            i64_attr("gen_ai.usage.output_tokens", 12),
            strings_attr("gen_ai.response.finish_reasons", &["tool_use"])
        ]
    );
}

#[test]
fn request_failure_uses_stable_classification_without_raw_data() {
    // Given: an open request span.
    let mut mapper = SpanMapper::new();
    mapper.ingest(&start_run("run-1", None, 1));
    mapper.ingest(&event(
        ProviderEvent::RequestStarted {
            request_id: "request-1".to_owned(),
            provider: "openai".to_owned(),
            profile: None,
            protocol: "openai-chat-completions".to_owned(),
            model: "gpt-test".to_owned(),
            streaming: false,
            run_id: Some("run-1".to_owned()),
        },
        2,
    ));
    // When: an HTTP request failure includes a status-bearing classification.
    let actions = mapper.ingest(&event(
        ProviderEvent::RequestFailed {
            request_id: "request-1".to_owned(),
            provider: "openai".to_owned(),
            profile: None,
            protocol: "openai-chat-completions".to_owned(),
            model: "gpt-test".to_owned(),
            streaming: false,
            duration_ms: 10,
            failure: ProviderFailureKind::Http { status: 503 },
            run_id: Some("run-1".to_owned()),
        },
        3,
    ));
    // Then: error.type is stable and does not expose status or free-form data.
    assert!(matches!(
        actions[0],
        SpanAction::End {
            status: SpanStatus::Error,
            ..
        }
    ));
    assert_eq!(
        action_attributes(&actions[0]).last(),
        Some(&str_attr("error.type", "http"))
    );
    assert!(!format!("{actions:?}").contains("503"));
}

#[test]
fn tool_error_excludes_detail_and_first_token_is_nonmapped() {
    // Given: an open tool span.
    let mut mapper = SpanMapper::new();
    mapper.ingest(&start_run("run-1", None, 1));
    mapper.ingest(&event(
        ToolEvent::ToolStarted {
            tool_name: "search".to_owned(),
            call_id: "call-1".to_owned(),
            run_id: Some("run-1".to_owned()),
        },
        2,
    ));
    // When: first-token telemetry arrives and the tool ends with raw detail.
    let first_token = mapper.ingest(&event(
        ProviderEvent::FirstTokenObserved {
            request_id: "request-1".to_owned(),
            provider: "openai".to_owned(),
            profile: None,
            protocol: "openai-chat-completions".to_owned(),
            model: "gpt-test".to_owned(),
            ttft_ms: 5,
            run_id: Some("run-1".to_owned()),
        },
        3,
    ));
    let completed = mapper.ingest(&event(
        ToolEvent::ToolCompleted {
            tool_name: "search".to_owned(),
            call_id: "call-1".to_owned(),
            is_error: true,
            detail: Some(serde_json::json!({"secret": "raw detail"})),
            run_id: Some("run-1".to_owned()),
        },
        4,
    ));
    // Then: first token emits nothing and tool error contains classification only.
    assert!(first_token.is_empty());
    assert_eq!(
        action_attributes(&completed[0]).last(),
        Some(&str_attr("error.type", "tool_error"))
    );
    assert!(!format!("{completed:?}").contains("raw detail"));
}

#[test]
fn unrelated_background_task_is_ignored() {
    // Given: one open run.
    let mut mapper = SpanMapper::new();
    mapper.ingest(&start_run("run-1", None, 1));
    // When: a background task with a different ID starts.
    let actions = mapper.ingest(&event(
        LifecycleEvent::BackgroundTaskStarted {
            task_id: "different".to_owned(),
        },
        2,
    ));
    // Then: no action or drop is recorded.
    assert!(actions.is_empty());
    assert!(mapper.drain_drops().is_empty());
}
