mod lifecycle;
mod request_tool;
mod run;
mod terminal;

use std::time::{Duration, SystemTime};

use crate::event::{Event, EventKind, EventMeta, LifecycleEvent};

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
