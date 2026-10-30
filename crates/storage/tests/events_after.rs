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

#[test]
fn events_of_runs_can_be_read_in_pages_bounded_by_ids() {
    // Given: replies of two runs interleaved (ids 1..=9, run r1 on the odd ones)
    let dir = TempDir::new().expect("temp dir");
    let config = StorageConfig {
        db_path: dir.path().join("pages.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    for index in 1..=9 {
        let run = if index % 2 == 1 { "r1" } else { "r2" };
        storage
            .handle()
            .append_stream_event(
                "s",
                &Event::new(event_bus::MessageEvent::MessageCompleted {
                    run_id: run.into(),
                    text: format!("m{index}"),
                }),
                None,
            )
            .expect("append");
    }
    let reader = Database::open_read_only(&config.db_path).expect("read-only open");
    let runs = ["r1".to_owned()];

    // When: r1's events are read through id 8, 2 at a time, each page after the last ID of the previous
    let mut ids = Vec::new();
    let mut after = 0;
    loop {
        let (page, more) = reader
            .events_for_runs_between(&runs, after, 8, 2)
            .expect("page");
        ids.extend(page.iter().map(|stored| stored.id));
        let Some(last) = page.last().map(|stored| stored.id) else {
            break;
        };
        if !more {
            break;
        }
        after = last;
    }

    // Then: every matching event up to the bound arrives once, in order
    assert_eq!(ids, [1, 3, 5, 7]);
}

fn big_tool_output(label: &str, run_id: &str) -> Event {
    Event::new(event_bus::ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: format!("call-{label}"),
        is_error: false,
        output: Some(format!("{label} output line\n").repeat(500)),
        detail: None,
        run_id: Some(run_id.into()),
        parent_call_id: None,
    })
}

fn payload_classes(path: &std::path::Path) -> Vec<String> {
    let conn = Connection::open(path).expect("raw connection");
    let mut statement = conn
        .prepare("SELECT typeof(payload) FROM events ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn compressed_payloads_are_followed_excluded_and_paged_like_plain_ones() {
    // Given: large tool outputs (stored as zstd BLOBs) from this and another session
    let dir = TempDir::new().expect("temp dir");
    let config = StorageConfig {
        db_path: dir.path().join("compressed.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    let events = [
        ("other", big_tool_output("a", "r1")),
        ("me", big_tool_output("b", "r1")),
        ("other", big_tool_output("c", "r2")),
        ("other", big_tool_output("d", "r1")),
    ];
    for (session, event) in &events {
        storage
            .handle()
            .append_stream_event(session, event, None)
            .expect("append");
    }
    assert_eq!(payload_classes(&config.db_path), ["blob"; 4]);
    let reader = Database::open_read_only(&config.db_path).expect("read-only open");

    // When/Then: the follower decodes every row and skips its own without reading it
    let all = reader.events_after(0, 10).unwrap();
    assert_eq!(
        all.events
            .iter()
            .map(|stored| &stored.event)
            .collect::<Vec<_>>(),
        events.iter().map(|(_, event)| event).collect::<Vec<_>>()
    );
    let others = reader.events_after_excluding(0, 2, "me").unwrap();
    assert_eq!(
        others
            .events
            .iter()
            .map(|stored| stored.id)
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!((others.scanned, others.last_id), (2, 2));

    // And: the paged run read finds compressed rows through the run_id column
    let runs = ["r1".to_owned()];
    let (page, more) = reader.events_for_runs_between(&runs, 0, 4, 2).unwrap();
    assert_eq!(
        page.iter().map(|stored| stored.id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(more);
    let (rest, more) = reader.events_for_runs_between(&runs, 2, 4, 2).unwrap();
    assert_eq!(rest.iter().map(|stored| stored.id).collect::<Vec<_>>(), [4]);
    assert!(!more);
}

#[test]
fn a_corrupt_compressed_row_is_skipped_by_the_follower_and_never_read_when_excluded() {
    // Given: damaged BLOBs (zstd magic, then garbage) in another session's row and in our own
    let dir = TempDir::new().expect("temp dir");
    let config = StorageConfig {
        db_path: dir.path().join("corrupt.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    storage
        .handle()
        .append_stream_event("other", &big_tool_output("ok", "r1"), None)
        .expect("append");
    let damaged: Vec<u8> = [0x28, 0xB5, 0x2F, 0xFD, 0xFF, 0xFF, 0xFF, 0xFF].to_vec();
    let conn = Connection::open(&config.db_path).expect("raw connection");
    for session in ["other", "me"] {
        conn.execute(
            "INSERT INTO events \
             (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
             VALUES (?1, ?2, 0, 0, 'Tool', ?3)",
            params![session, event_bus::SCHEMA_VERSION, damaged],
        )
        .expect("raw insert");
    }
    let reader = Database::open_read_only(&config.db_path).expect("read-only open");

    // When: following with our own session excluded
    let page = reader.events_after_excluding(0, 10, "me").unwrap();

    // Then: the intact row arrives, the damaged foreign row is skipped like an undecodable one,
    // and the cursor passes both damaged rows
    assert_eq!(page.events.len(), 1);
    assert_eq!((page.scanned, page.last_id), (3, 3));
}
