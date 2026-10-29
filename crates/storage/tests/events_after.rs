//! 他プロセスが書いたイベントログを、カーソルで追従して読む API を検証します。

use event_bus::{Event, LifecycleEvent};
use rusqlite::{Connection, params};
use storage::{Database, Storage, StorageConfig};
use tempfile::TempDir;

fn started(label: &str) -> Event {
    Event::new(LifecycleEvent::Started {
        session_id: label.into(),
    })
}

fn session_ids(page: &storage::EventPage) -> Vec<String> {
    page.events
        .iter()
        .map(|stored| match &stored.event.kind {
            event_bus::EventKind::Lifecycle(LifecycleEvent::Started { session_id }) => {
                session_id.clone()
            }
            other => panic!("unexpected event {other:?}"),
        })
        .collect()
}

#[test]
fn events_after_pages_in_order_without_overlap_through_a_read_only_connection() {
    // Given: 5 events written by a writer and a read-only connection opened beside it
    let dir = TempDir::new().expect("temp dir");
    let config = StorageConfig {
        db_path: dir.path().join("follow.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    for index in 0..5 {
        handle
            .append_event(Some("s"), &started(&format!("e{index}")))
            .expect("append");
    }
    let reader = Database::open_read_only(&config.db_path).expect("read-only open");
    assert_eq!(reader.last_event_id().unwrap(), 5);

    // When: pages of 2 are read starting from the beginning
    let first = reader.events_after(0, 2).unwrap();
    let second = reader.events_after(first.last_id, 2).unwrap();
    let third = reader.events_after(second.last_id, 2).unwrap();
    let empty = reader.events_after(third.last_id, 2).unwrap();

    // Then: each id is returned once in ascending order and the cursor is exclusive
    assert_eq!(session_ids(&first), ["e0", "e1"]);
    assert_eq!(session_ids(&second), ["e2", "e3"]);
    assert_eq!(session_ids(&third), ["e4"]);
    assert_eq!((first.last_id, second.last_id, third.last_id), (2, 4, 5));
    assert!(empty.events.is_empty());
    assert_eq!(empty.last_id, 5, "an empty page keeps the cursor");
}

#[test]
fn events_after_advances_the_cursor_past_undecodable_rows() {
    // Given: a decodable event followed by a row of a removed variant
    let dir = TempDir::new().expect("temp dir");
    let config = StorageConfig {
        db_path: dir.path().join("undecodable.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    storage
        .handle()
        .append_event(Some("s"), &started("kept"))
        .expect("append");
    Connection::open(&config.db_path)
        .expect("raw connection")
        .execute(
            "INSERT INTO events \
             (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
             VALUES ('s', ?1, 0, 0, 'Snapshot', '{\"kind\":\"Removed\",\"payload\":{}}')",
            params![event_bus::SCHEMA_VERSION],
        )
        .expect("raw insert");
    let reader = Database::open_read_only(&config.db_path).expect("read-only open");

    // When: the page ends on the undecodable row
    let page = reader.events_after(0, 10).unwrap();

    // Then: it is skipped but the cursor passes it, so it is not rescanned
    assert_eq!(session_ids(&page), ["kept"]);
    assert_eq!(page.last_id, 2);
    assert!(
        reader
            .events_after(page.last_id, 10)
            .unwrap()
            .events
            .is_empty()
    );
}

#[test]
fn data_version_changes_only_when_another_connection_commits() {
    // Given: a read-only follower and a writer on the same file
    let dir = TempDir::new().expect("temp dir");
    let config = StorageConfig {
        db_path: dir.path().join("version.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    storage
        .handle()
        .append_event(Some("s"), &started("first"))
        .expect("append");
    let reader = Database::open_read_only(&config.db_path).expect("read-only open");
    let before = reader.data_version().unwrap();

    // When/Then: nothing changed -> same version; a foreign commit -> a different one
    assert_eq!(reader.data_version().unwrap(), before);
    storage
        .handle()
        .append_event(Some("s"), &started("second"))
        .expect("append");
    assert_ne!(reader.data_version().unwrap(), before);
}

#[test]
fn excluded_session_rows_advance_the_cursor_without_being_returned() {
    // Given: rows of two sessions interleaved
    let dir = TempDir::new().expect("temp dir");
    let config = StorageConfig {
        db_path: dir.path().join("exclude.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    for (session, label) in [("me", "m0"), ("other", "o1"), ("me", "m2"), ("me", "m3")] {
        handle
            .append_stream_event(session, &started(label), None)
            .expect("append");
    }
    let reader = Database::open_read_only(&config.db_path).expect("read-only open");

    // When: the follower reads with its own session excluded, two rows at a time
    let first = reader.events_after_excluding(0, 2, "me").unwrap();
    let second = reader
        .events_after_excluding(first.last_id, 2, "me")
        .unwrap();

    // Then: only the other session's row is returned and every row was scanned
    assert_eq!(session_ids(&first), ["o1"]);
    assert_eq!((first.scanned, first.last_id), (2, 2));
    assert!(second.events.is_empty());
    assert_eq!((second.scanned, second.last_id), (2, 4));
}
