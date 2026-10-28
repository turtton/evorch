use crate::event::{ProviderEvent, ToolEvent};

use super::super::{SpanAction, SpanDropKind, SpanKey, SpanKind, SpanMapper};
use super::{BASE_TIME, event, start_run, str_attr};

#[test]
fn chat_and_tool_spans_parent_to_known_agent() {
    // Given: a known agent run.
    let mut mapper = SpanMapper::new();
    mapper.ingest(&start_run("run-1", None, 1));

    // When: a provider request and tool execution start.
    let chat = mapper.ingest(&event(
        ProviderEvent::RequestStarted {
            request_id: "request-1".to_owned(),
            provider: "OPENAI".to_owned(),
            profile: None,
            protocol: "openai-chat-completions".to_owned(),
            model: "gpt-test".to_owned(),
            streaming: true,
            run_id: Some("run-1".to_owned()),
            reasoning_effort: None,
            purpose: None,
        },
        2,
    ));
    let tool = mapper.ingest(&event(
        ToolEvent::ToolStarted {
            input: None,
            tool_name: "search".to_owned(),
            call_id: "call-1".to_owned(),
            run_id: Some("run-1".to_owned()),
            parent_call_id: None,
        },
        3,
    ));

    // Then: both parent to agent:{run_id}, with fixed names and attr order.
    assert_eq!(
        chat,
        vec![SpanAction::Start {
            key: SpanKey::Request {
                request_id: "request-1".to_owned(),
            },
            parent: Some(SpanKey::Agent {
                run_id: "run-1".to_owned(),
            }),
            name: "chat gpt-test".to_owned(),
            kind: SpanKind::Client,
            start_time: BASE_TIME + std::time::Duration::from_secs(2),
            attributes: vec![
                str_attr("gen_ai.operation.name", "chat"),
                str_attr("gen_ai.provider.name", "openai"),
                str_attr("gen_ai.request.model", "gpt-test"),
                str_attr("evorch.agent_run.id", "run-1"),
                str_attr("evorch.request.id", "request-1"),
            ],
        }]
    );
    assert_eq!(
        tool,
        vec![SpanAction::Start {
            key: SpanKey::Tool {
                call_id: "call-1".to_owned(),
            },
            parent: Some(SpanKey::Agent {
                run_id: "run-1".to_owned(),
            }),
            name: "execute_tool search".to_owned(),
            kind: SpanKind::Internal,
            start_time: BASE_TIME + std::time::Duration::from_secs(3),
            attributes: vec![
                str_attr("gen_ai.operation.name", "execute_tool"),
                str_attr("gen_ai.provider.name", "evorch"),
                str_attr("gen_ai.tool.name", "search"),
                str_attr("gen_ai.tool.call.id", "call-1"),
                str_attr("evorch.agent_run.id", "run-1"),
            ],
        }]
    );
}

#[test]
fn missing_run_id_and_unknown_parent_are_typed_drops() {
    // Given: an empty mapper.
    let mut mapper = SpanMapper::new();

    // When: starts omit run_id or reference an unknown run.
    let missing_request = mapper.ingest(&event(
        ProviderEvent::RequestStarted {
            request_id: "missing-run".to_owned(),
            provider: "unknown".to_owned(),
            profile: None,
            protocol: "test".to_owned(),
            model: "model".to_owned(),
            streaming: false,
            run_id: None,
            reasoning_effort: None,
            purpose: None,
        },
        1,
    ));
    let missing_tool = mapper.ingest(&event(
        ToolEvent::ToolStarted {
            input: None,
            tool_name: "tool".to_owned(),
            call_id: "missing-tool-run".to_owned(),
            run_id: None,
            parent_call_id: None,
        },
        2,
    ));
    let unknown_parent = mapper.ingest(&event(
        ProviderEvent::RequestStarted {
            request_id: "unknown-parent".to_owned(),
            provider: "unknown".to_owned(),
            profile: None,
            protocol: "test".to_owned(),
            model: "model".to_owned(),
            streaming: false,
            run_id: Some("absent".to_owned()),
            reasoning_effort: None,
            purpose: None,
        },
        3,
    ));

    // Then: no spans start and each drop is classified.
    assert!(missing_request.is_empty());
    assert!(missing_tool.is_empty());
    assert!(unknown_parent.is_empty());
    let drops = mapper.drain_drops();
    assert_eq!(
        drops.iter().map(|drop| drop.kind).collect::<Vec<_>>(),
        vec![
            SpanDropKind::MissingRunId,
            SpanDropKind::MissingRunId,
            SpanDropKind::UnknownParent,
        ]
    );
}

#[test]
fn child_tool_span_parents_to_open_parent_tool_else_agent() {
    // Given: a known agent run with an open parent tool span (e.g. run_js).
    let mut mapper = SpanMapper::new();
    mapper.ingest(&start_run("run-1", None, 1));
    mapper.ingest(&event(
        ToolEvent::ToolStarted {
            input: None,
            tool_name: "run_js".to_owned(),
            call_id: "call-1".to_owned(),
            run_id: Some("run-1".to_owned()),
            parent_call_id: None,
        },
        2,
    ));
    let child_started = |call_id: &str, parent: &str, seconds| {
        event(
            ToolEvent::ToolStarted {
                input: None,
                tool_name: "read".to_owned(),
                call_id: call_id.to_owned(),
                run_id: Some("run-1".to_owned()),
                parent_call_id: Some(parent.to_owned()),
            },
            seconds,
        )
    };

    // When: a child names the open parent, and another names a parent that is not open.
    let child = mapper.ingest(&child_started("call-1/0", "call-1", 3));
    let orphan = mapper.ingest(&child_started("call-9/0", "call-9", 4));

    // Then: the first parents to the tool span, the second falls back to the agent.
    let parent_of = |actions: &[SpanAction]| match actions {
        [SpanAction::Start { parent, .. }] => parent.clone(),
        other => panic!("expected one Start, got {other:?}"),
    };
    assert_eq!(
        parent_of(&child),
        Some(SpanKey::Tool {
            call_id: "call-1".to_owned()
        })
    );
    assert_eq!(
        parent_of(&orphan),
        Some(SpanKey::Agent {
            run_id: "run-1".to_owned()
        })
    );
}
