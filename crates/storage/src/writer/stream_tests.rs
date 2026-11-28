use event_bus::{Event, MessageEvent};

use crate::{Database, HardLimits, LimitKind, Storage, StorageConfig, StorageError};

#[test]
fn stream_append_preserves_session_identity_and_normal_session_limits() {
    // Given: no remaining capacity in a bounded session.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        hard_limits: HardLimits {
            max_session_bytes: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let writer = Storage::open(config.clone()).unwrap();
    let event = Event::new(MessageEvent::MessageDelta {
        run_id: Some("run-1".into()),
        delta: "answer".into(),
    });
    // When: a cross-session stream accepts the event.
    writer.handle().append_stream_event("gui", &event).unwrap();
    // Then: session queries still work and ordinary appends remain bounded.
    let events = Database::open(&config)
        .unwrap()
        .events_by_session("gui")
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event);
    assert!(matches!(
        writer.handle().append_event(Some("gui"), &event),
        Err(StorageError::LimitExceeded {
            limit: LimitKind::SessionSize,
            ..
        })
    ));
}

#[test]
fn stream_append_keeps_event_size_limit_but_only_warns_on_volume_limits() {
    for (limits, expected) in [
        (
            HardLimits {
                max_event_bytes: 0,
                ..Default::default()
            },
            Some(LimitKind::EventSize),
        ),
        (
            HardLimits {
                max_daily_event_bytes: 0,
                ..Default::default()
            },
            None,
        ),
        (
            HardLimits {
                max_db_bytes: 0,
                ..Default::default()
            },
            None,
        ),
    ] {
        // Given: an exhausted non-session storage limit.
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("events.db"),
            hard_limits: limits,
            ..Default::default()
        };
        let writer = Storage::open(config.clone()).unwrap();
        let event = Event::new(MessageEvent::MessageDelta {
            run_id: Some("run-1".into()),
            delta: "answer".into(),
        });
        // When: the GUI stream attempts to persist an event.
        let result = writer.handle().append_stream_event("gui", &event);
        // Then: only the single-event limit rejects the write; the volume limits warn.
        let stored = Database::open(&config)
            .unwrap()
            .events_all_ordered()
            .unwrap()
            .len();
        match expected {
            Some(limit) => {
                assert!(
                    matches!(result, Err(StorageError::LimitExceeded { limit: actual, .. }) if actual == limit),
                    "expected {limit:?}: {result:?}"
                );
                assert_eq!(stored, 0);
            }
            None => {
                assert_eq!(result, Ok(()));
                assert_eq!(stored, 1);
            }
        }
    }
}
