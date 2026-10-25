//! The trace helpers must reject a trace whose cache observation is missing, zero,
//! inconsistent, or not reused, instead of silently skipping the cache check.

use event_bus::{Event, EventKind, ProviderEvent, RequestPurpose};
use runtime::RunId;
use serde_json::{Value, json};

use super::{assert_cache_chain, exchanges, exchanges_with_failures};

const RUN: RunId = RunId::new(1);

fn started(id: &str) -> Event {
    Event::new(EventKind::Provider(ProviderEvent::RequestStarted {
        request_id: id.into(),
        provider: "openai-codex".into(),
        profile: Some("work".into()),
        protocol: "openai-codex-responses".into(),
        model: "mock-model".into(),
        streaming: true,
        run_id: Some(RUN.to_string()),
        reasoning_effort: None,
        purpose: Some(RequestPurpose::Agent),
    }))
}

fn completed(id: &str, input_tokens: u64, cache_read_tokens: u64) -> Event {
    Event::new(EventKind::Provider(ProviderEvent::RequestCompleted {
        request_id: id.into(),
        provider: "openai-codex".into(),
        profile: Some("work".into()),
        protocol: "openai-codex-responses".into(),
        model: "mock-model".into(),
        streaming: true,
        duration_ms: 1,
        input_tokens,
        output_tokens: 1,
        cache_read_tokens,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        service_tier: None,
        run_id: Some(RUN.to_string()),
        purpose: Some(RequestPurpose::Agent),
        reasoning_tokens: None,
    }))
}

fn failed(id: &str) -> Event {
    Event::new(ProviderEvent::RequestFailed {
        detail: None,
        request_id: id.into(),
        provider: "openai-codex".into(),
        profile: Some("work".into()),
        protocol: "openai-codex-responses".into(),
        model: "mock-model".into(),
        streaming: true,
        duration_ms: 1,
        failure: event_bus::ProviderFailureKind::Other,
        run_id: Some(RUN.to_string()),
        purpose: Some(RequestPurpose::Agent),
    })
}

/// Two append-only requests: the second extends the first by one user message.
fn bodies() -> Vec<Value> {
    let first = json!({"model": "mock-model", "instructions": "fixed", "tools": [],
        "include": ["reasoning.encrypted_content"],
        "input": [{"type": "message", "role": "user", "content": "go"}]});
    let mut second = first.clone();
    second["input"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "message", "role": "user", "content": "next"}));
    vec![first, second]
}

/// `usage[i]` completes request `i`; `None` leaves it without a completion.
fn trace(usage: &[Option<(u64, u64)>]) -> Vec<Event> {
    let mut events = Vec::new();
    for (index, usage) in usage.iter().enumerate() {
        let id = format!("req-{index}");
        events.push(started(&id));
        if let Some((input, cached)) = usage {
            events.push(completed(&id, *input, *cached));
        }
    }
    events
}

// Given: two append-only requests whose second reports the whole first input cached
// When: the helpers check the trace
// Then: it passes (positive control for the negative cases below)
#[test]
fn a_reused_positive_trace_passes() {
    let events = trace(&[Some((100, 0)), Some((120, 100))]);
    assert_cache_chain(&exchanges(bodies(), RUN, &events), &[]);
}

// Given: a successful-looking request without a completion event
// When: the helpers pair the trace
// Then: the missing cache observation is rejected
#[test]
#[should_panic(expected = "request 1 has no completed usage")]
fn missing_usage_is_rejected() {
    exchanges(bodies(), RUN, &trace(&[Some((100, 0)), None]));
}

// Given: a request whose usage is all zero
// When: the helpers pair the trace
// Then: the empty observation is rejected
#[test]
#[should_panic(expected = "request 1 reported no input tokens")]
fn all_zero_usage_is_rejected() {
    exchanges(bodies(), RUN, &trace(&[Some((100, 0)), Some((0, 0))]));
}

// Given: a request reporting more cached than input tokens
// When: the helpers pair the trace
// Then: the inconsistent observation is rejected
#[test]
#[should_panic(expected = "exceeds input")]
fn cached_beyond_input_is_rejected() {
    exchanges(bodies(), RUN, &trace(&[Some((100, 0)), Some((120, 121))]));
}

// Given: an append-only request whose mock cache did not reuse the previous input
// When: the cache chain is checked
// Then: the lost prefix is rejected even though the wire prefix is intact
#[test]
#[should_panic(expected = "previous input=100")]
fn an_unreused_previous_input_is_rejected() {
    let events = trace(&[Some((100, 0)), Some((120, 99))]);
    assert_cache_chain(&exchanges(bodies(), RUN, &events), &[]);
}

// Given: positive input usage but no cache read on an ordinary follow-up
// When: the independent wire check would accept the unchanged prefix
// Then: the cache observation still rejects the trace
#[test]
#[should_panic(expected = "cached=0, previous input=100")]
fn zero_cache_on_an_ordinary_follow_up_is_rejected() {
    let events = trace(&[Some((100, 0)), Some((120, 0))]);
    assert_cache_chain(&exchanges(bodies(), RUN, &events), &[]);
}

// Given: a text re-summary request with compaction purpose but no completed usage
// When: the trace is paired before any prefix-step exemptions are applied
// Then: summary requests cannot silently bypass the usage requirement
#[test]
#[should_panic(expected = "request 1 has no completed usage")]
fn missing_summary_usage_is_rejected() {
    let mut events = trace(&[Some((100, 0)), None]);
    let EventKind::Provider(ProviderEvent::RequestStarted { purpose, .. }) =
        &mut events.last_mut().unwrap().kind
    else {
        unreachable!()
    };
    *purpose = Some(RequestPurpose::Compaction);
    exchanges(bodies(), RUN, &events);
}

// Given: a request declared as a failing fallback attempt that actually completed
// When: the helpers pair the trace
// Then: the declaration is rejected, so failures cannot hide observations
#[test]
#[should_panic(expected = "request 1 was expected to fail")]
fn a_declared_failure_must_not_complete() {
    exchanges_with_failures(
        bodies(),
        RUN,
        &trace(&[Some((100, 0)), Some((120, 100))]),
        &[1],
    );
}

// Given: an incomplete request merely declared as a failure
// When: no provider failure event corroborates that declaration
// Then: the exception cannot disguise a missing successful observation
#[test]
#[should_panic(expected = "request 1 has no failure event")]
fn a_failure_exception_requires_a_failure_event() {
    exchanges_with_failures(bodies(), RUN, &trace(&[Some((100, 0)), None]), &[1]);
}

// Given: a declared failing attempt that extends the previous request
// When: the cache chain is checked
// Then: only that declared request may lack usage
#[test]
fn only_declared_failures_may_lack_usage() {
    let mut requests = bodies();
    requests.push(requests[1].clone());
    let mut events = trace(&[Some((100, 0)), None, Some((120, 100))]);
    events.insert(3, failed("req-1"));
    assert_cache_chain(&exchanges_with_failures(requests, RUN, &events, &[1]), &[]);
}
