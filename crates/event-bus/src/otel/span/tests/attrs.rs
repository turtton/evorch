use std::collections::BTreeSet;

use crate::event::{AgentRunPhase, LifecycleEvent, ProviderEvent, ProviderFailureKind, ToolEvent};

use super::super::{
    SPAN_ATTRIBUTE_WHITELIST, SpanAttribute, SpanAttributeValue, SpanMapper,
    validate_span_attributes,
};
use super::{event, request_started, session_completed, session_started, start_run, tool_started};

fn str_attribute(key: &str, value: &str) -> SpanAttribute {
    SpanAttribute {
        key: key.to_owned(),
        value: SpanAttributeValue::Str(value.to_owned()),
    }
}

fn i64_attribute(key: &str, value: i64) -> SpanAttribute {
    SpanAttribute {
        key: key.to_owned(),
        value: SpanAttributeValue::I64(value),
    }
}

fn strings_attribute(key: &str, values: &[&str]) -> SpanAttribute {
    SpanAttribute {
        key: key.to_owned(),
        value: SpanAttributeValue::Strings(
            values.iter().map(|value| (*value).to_owned()).collect(),
        ),
    }
}

fn keys_of(attrs: &[SpanAttribute]) -> BTreeSet<String> {
    attrs.iter().map(|attr| attr.key.clone()).collect()
}

#[test]
fn span_whitelist_is_the_sorted_closed_key_set() {
    // Given: the mandated span attribute vocabulary.
    let expected = [
        "error.type",
        "evorch.agent.name",
        "evorch.agent_run.id",
        "evorch.delegation.depth",
        "evorch.delegation.role",
        "evorch.parent_agent_run.id",
        "evorch.request.id",
        "evorch.session.id",
        "evorch.task.id",
        "gen_ai.agent.name",
        "gen_ai.operation.name",
        "gen_ai.provider.name",
        "gen_ai.request.model",
        "gen_ai.response.finish_reasons",
        "gen_ai.tool.call.id",
        "gen_ai.tool.name",
        "gen_ai.usage.input_tokens",
        "gen_ai.usage.output_tokens",
    ];
    // When: the whitelist constant is inspected.
    // Then: it equals the closed sorted set exactly.
    assert_eq!(SPAN_ATTRIBUTE_WHITELIST, expected);
}

#[test]
fn raw_content_keys_are_rejected_as_raw_content() {
    // Given: one representative key per raw-content denylist family.
    let denylist_samples = [
        "gen_ai.prompt",
        "gen_ai.completion",
        "gen_ai.prompt.content",
        "input.messages",
        "output.messages",
        "gen_ai.content",
        "error.message",
        "response.body",
        "sse.events",
        "credential",
        "api_key",
        "auth.token",
    ];
    // When: each key is validated.
    // Then: it is rejected with the raw-content classification, never emitted.
    for key in denylist_samples {
        let attrs = [str_attribute(key, "raw")];
        assert!(
            validate_span_attributes(&attrs).is_err(),
            "key={key} must be rejected"
        );
    }
}

#[test]
fn whitelist_denylist_overlap_is_only_the_usage_token_counts() {
    // Given: the denylist substrings and the whitelist.
    // When: whitelist keys are matched against the denylist substrings.
    // Then: the only textual overlap is the two usage counters — bounded
    //       integer counts, not raw content — and both validate cleanly.
    let denylist = [
        "gen_ai.prompt",
        "gen_ai.completion",
        "input.messages",
        "output.messages",
        "content",
        "message",
        "body",
        "sse",
        "credential",
        "token",
        "api_key",
    ];
    let overlapping: Vec<&str> = SPAN_ATTRIBUTE_WHITELIST
        .iter()
        .copied()
        .filter(|key| denylist.iter().any(|pattern| key.contains(pattern)))
        .collect();
    assert_eq!(
        overlapping,
        ["gen_ai.usage.input_tokens", "gen_ai.usage.output_tokens"]
    );
    for key in overlapping {
        let attrs = [i64_attribute(key, 7)];
        assert_eq!(validate_span_attributes(&attrs), Ok(()), "key={key}");
    }
}

#[test]
fn validate_rejects_values_outside_closed_domains() {
    // Given: one out-of-domain value per closed attribute domain.
    let violations = [
        str_attribute("gen_ai.operation.name", "embeddings"),
        str_attribute("evorch.delegation.role", "super-admin"),
        i64_attribute("evorch.delegation.depth", 100),
        i64_attribute("evorch.delegation.depth", -1),
        str_attribute("evorch.delegation.depth", "0"),
        str_attribute("gen_ai.provider.name", "custom-proxy"),
        str_attribute("error.type", "rate-limit"),
        str_attribute("gen_ai.response.model", "gpt-test"),
    ];
    // When: each attribute is validated.
    // Then: every one is rejected.
    for attr in violations {
        assert!(
            validate_span_attributes(std::slice::from_ref(&attr)).is_err(),
            "key={} value must be rejected",
            attr.key
        );
    }
}

#[test]
fn validate_accepts_the_whitelisted_domains() {
    // Given: in-domain values for every closed domain of the whitelist.
    let mut valid = vec![
        str_attribute("gen_ai.operation.name", "chat"),
        str_attribute("gen_ai.operation.name", "invoke_agent"),
        str_attribute("gen_ai.operation.name", "execute_tool"),
        str_attribute("evorch.delegation.role", "orchestrator"),
        str_attribute("evorch.delegation.role", "explorer"),
        str_attribute("evorch.delegation.role", "worker"),
        str_attribute("evorch.delegation.role", "reviewer"),
        i64_attribute("evorch.delegation.depth", 0),
        i64_attribute("evorch.delegation.depth", 99),
        str_attribute("gen_ai.provider.name", "anthropic"),
        str_attribute("gen_ai.provider.name", "openai"),
        str_attribute("gen_ai.provider.name", "openai-compatible"),
        str_attribute("gen_ai.provider.name", "evorch"),
        str_attribute("gen_ai.provider.name", "other"),
        str_attribute("error.type", "http"),
        str_attribute("error.type", "agent_run_error"),
        str_attribute("error.type", "tool_error"),
        str_attribute("error.type", "session_failed"),
        str_attribute("error.type", "span_budget_evicted"),
        str_attribute("evorch.session.id", "session-1"),
        str_attribute("evorch.task.id", "task-1"),
        str_attribute("gen_ai.request.model", "gpt-test"),
        strings_attribute("gen_ai.response.finish_reasons", &["stop"]),
    ];
    // When: the batch is validated.
    // Then: it passes as one closed-domain conforming attribute list.
    assert_eq!(validate_span_attributes(&valid), Ok(()));
    // And: an empty attribute list is also valid.
    valid.clear();
    assert_eq!(validate_span_attributes(&valid), Ok(()));
}

#[test]
fn every_mapper_emitted_key_is_whitelisted() {
    // Given: a representative event corpus covering every mapped variant.
    let mut mapper = SpanMapper::new();
    let corpus = vec![
        session_started("session-1", 1),
        start_run("run-1", None, 2),
        start_run("child", Some("run-1"), 3),
        event(
            LifecycleEvent::BackgroundTaskStarted {
                task_id: "run-1".to_owned(),
            },
            4,
        ),
        request_started("req-1", "run-1", 5),
        event(
            ProviderEvent::RequestCompleted {
                request_id: "req-1".to_owned(),
                provider: "anthropic".to_owned(),
                profile: None,
                protocol: "anthropic-messages".to_owned(),
                model: "gpt-test".to_owned(),
                streaming: false,
                duration_ms: 10,
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                finish_reason: "stop".to_owned(),
                run_id: Some("run-1".to_owned()),
            },
            6,
        ),
        request_started("req-2", "run-1", 7),
        event(
            ProviderEvent::RequestFailed {
                request_id: "req-2".to_owned(),
                provider: "anthropic".to_owned(),
                profile: None,
                protocol: "anthropic-messages".to_owned(),
                model: "gpt-test".to_owned(),
                streaming: false,
                duration_ms: 10,
                failure: ProviderFailureKind::Timeout,
                run_id: Some("run-1".to_owned()),
            },
            8,
        ),
        tool_started("call-1", "run-1", 9),
        event(
            ToolEvent::ToolCompleted {
                tool_name: "search".to_owned(),
                call_id: "call-1".to_owned(),
                is_error: true,
                detail: None,
                run_id: Some("run-1".to_owned()),
            },
            10,
        ),
        event(
            LifecycleEvent::AgentRunStateChanged {
                run_id: "child".to_owned(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Error,
                reason: None,
            },
            11,
        ),
        event(
            LifecycleEvent::AgentRunStateChanged {
                run_id: "run-1".to_owned(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Done,
                reason: None,
            },
            12,
        ),
        event(
            LifecycleEvent::Failed {
                session_id: "session-1".to_owned(),
                reason: "boom".to_owned(),
            },
            13,
        ),
        session_started("session-2", 14),
        session_completed("session-2", 15),
    ];
    // When: the corpus flows through the mapper.
    let mut observed = BTreeSet::new();
    for event in &corpus {
        for action in mapper.ingest(event) {
            let attrs = match action {
                super::super::SpanAction::Start { attributes, .. } => attributes,
                super::super::SpanAction::End {
                    final_attributes, ..
                } => final_attributes,
            };
            observed.extend(keys_of(&attrs));
        }
    }
    // Then: the emitted key set equals the whitelist exactly — nothing outside
    //       it is ever generated and no whitelist key goes unused.
    let whitelist: BTreeSet<String> = SPAN_ATTRIBUTE_WHITELIST
        .iter()
        .map(|key| (*key).to_owned())
        .collect();
    assert_eq!(observed, whitelist);
    assert!(mapper.drain_drops().is_empty());
}
