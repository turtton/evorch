mod admission;
mod attrs;
mod budget;
mod lifecycle;
mod request_tool;
mod run;
mod terminal;

use std::time::{Duration, SystemTime};

use crate::event::{
    AgentRunPhase, Event, EventKind, EventMeta, LifecycleEvent, ProviderEvent, ToolEvent,
};

use super::{SpanAction, SpanAttribute, SpanAttributeValue};

const BASE_TIME: SystemTime = SystemTime::UNIX_EPOCH;

fn event(kind: impl Into<EventKind>, seconds: u64) -> Event {
    Event {
        meta: EventMeta {
            schema_version: 1,
            monotonic: Duration::from_secs(seconds),
            wall_clock: BASE_TIME + Duration::from_secs(seconds),
        },
        kind: kind.into(),
    }
}

fn start_run(run_id: &str, parent_run_id: Option<&str>, seconds: u64) -> Event {
    event(
        LifecycleEvent::AgentRunStarted {
            run_id: run_id.to_owned(),
            parent_run_id: parent_run_id.map(str::to_owned),
            agent_name: format!("agent-{run_id}"),
            role: "worker".to_owned(),
        },
        seconds,
    )
}

fn session_started(session_id: &str, seconds: u64) -> Event {
    event(
        LifecycleEvent::Started {
            session_id: session_id.to_owned(),
        },
        seconds,
    )
}

fn session_completed(session_id: &str, seconds: u64) -> Event {
    event(
        LifecycleEvent::Completed {
            session_id: session_id.to_owned(),
        },
        seconds,
    )
}

fn run_done(run_id: &str, seconds: u64) -> Event {
    event(
        LifecycleEvent::AgentRunStateChanged {
            run_id: run_id.to_owned(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        },
        seconds,
    )
}

fn request_started(request_id: &str, run_id: &str, seconds: u64) -> Event {
    request_started_custom(request_id, run_id, "anthropic", "gpt-test", seconds)
}

fn request_started_custom(
    request_id: &str,
    run_id: &str,
    provider: &str,
    model: &str,
    seconds: u64,
) -> Event {
    event(
        ProviderEvent::RequestStarted {
            request_id: request_id.to_owned(),
            provider: provider.to_owned(),
            profile: None,
            protocol: "anthropic-messages".to_owned(),
            model: model.to_owned(),
            streaming: false,
            run_id: Some(run_id.to_owned()),
        },
        seconds,
    )
}

fn request_completed(request_id: &str, seconds: u64) -> Event {
    event(
        ProviderEvent::RequestCompleted {
            request_id: request_id.to_owned(),
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
            run_id: None,
        },
        seconds,
    )
}

fn tool_started(call_id: &str, run_id: &str, seconds: u64) -> Event {
    event(
        ToolEvent::ToolStarted {
            tool_name: "search".to_owned(),
            call_id: call_id.to_owned(),
            run_id: Some(run_id.to_owned()),
        },
        seconds,
    )
}

fn str_attr(key: &str, value: &str) -> SpanAttribute {
    SpanAttribute {
        key: key.to_owned(),
        value: SpanAttributeValue::Str(value.to_owned()),
    }
}

fn i64_attr(key: &str, value: i64) -> SpanAttribute {
    SpanAttribute {
        key: key.to_owned(),
        value: SpanAttributeValue::I64(value),
    }
}

fn action_attributes(action: &SpanAction) -> &[SpanAttribute] {
    match action {
        SpanAction::Start { attributes, .. } => attributes,
        SpanAction::End {
            final_attributes, ..
        } => final_attributes,
    }
}
