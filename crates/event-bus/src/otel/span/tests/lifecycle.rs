use crate::event::{LifecycleEvent, MessageEvent};

use super::super::{SpanAction, SpanDropKind, SpanKey, SpanKind, SpanMapper, SpanStatus};
use super::{BASE_TIME, action_attributes, event, str_attr};

#[test]
fn session_span_opens_and_closes_when_lifecycle_completes() {
    // Given: an empty mapper and deterministic lifecycle timestamps.
    let mut mapper = SpanMapper::new();

    // When: a session starts and completes.
    let started = mapper.ingest(&event(
        LifecycleEvent::Started {
            session_id: "session-1".to_owned(),
        },
        1,
    ));
    let completed = mapper.ingest(&event(
        LifecycleEvent::Completed {
            session_id: "session-1".to_owned(),
        },
        2,
    ));

    // Then: a root session span uses event timestamps and stable attributes.
    assert_eq!(
        started,
        vec![SpanAction::Start {
            key: SpanKey::Session {
                session_id: "session-1".to_owned(),
            },
            parent: None,
            name: "evorch.session".to_owned(),
            kind: SpanKind::Internal,
            start_time: BASE_TIME + std::time::Duration::from_secs(1),
            attributes: vec![str_attr("evorch.session.id", "session-1")],
        }]
    );
    assert_eq!(
        completed,
        vec![SpanAction::End {
            key: SpanKey::Session {
                session_id: "session-1".to_owned(),
            },
            end_time: BASE_TIME + std::time::Duration::from_secs(2),
            status: SpanStatus::Unset,
            final_attributes: vec![str_attr("evorch.session.id", "session-1")],
        }]
    );
}

#[test]
fn session_failure_uses_stable_error_type_without_reason() {
    // Given: an open session span.
    let mut mapper = SpanMapper::new();
    mapper.ingest(&event(
        LifecycleEvent::Started {
            session_id: "session-1".to_owned(),
        },
        1,
    ));

    // When: the session fails with a sensitive free-form reason.
    let actions = mapper.ingest(&event(
        LifecycleEvent::Failed {
            session_id: "session-1".to_owned(),
            reason: "raw secret failure".to_owned(),
        },
        2,
    ));

    // Then: only the stable classification is included.
    assert_eq!(
        action_attributes(&actions[0]),
        [
            str_attr("evorch.session.id", "session-1"),
            str_attr("error.type", "session_failed"),
        ]
    );
    assert!(!format!("{actions:?}").contains("raw secret failure"));
}

#[test]
fn unknown_end_is_noop_with_typed_drop_and_nonmapped_event_is_empty() {
    // Given: an empty mapper.
    let mut mapper = SpanMapper::new();

    // When: an unknown session is ended and a message event is ingested.
    let unknown = mapper.ingest(&event(
        LifecycleEvent::Completed {
            session_id: "missing".to_owned(),
        },
        1,
    ));
    let nonmapped = mapper.ingest(&event(
        MessageEvent::MessageDelta {
            delta: "ignored".to_owned(),
        },
        2,
    ));

    // Then: both emit no actions and only the unknown End records a typed drop.
    assert!(unknown.is_empty());
    assert!(nonmapped.is_empty());
    let drops = mapper.drain_drops();
    assert_eq!(drops.len(), 1);
    assert_eq!(drops[0].kind, SpanDropKind::UnknownSpanEnd);
}
