//! The deferred readers a GUI uses to follow and to rebuild a conversation: rows of some
//! runs found through the run index, and a page of the log after a cursor, each with the
//! rows that cannot be read standing in their place and no blob read.

use event_bus::{Event, EventKind, LifecycleEvent, MessageEvent};
use rusqlite::{Connection, params};
use storage::{BodyRef, Database, DeferredRow, Storage, StorageConfig};
use tempfile::TempDir;

fn long_text(label: &str) -> String {
    format!("{label} ").repeat(1_000)
}

fn started(run: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: None,
        agent_name: "chat:Worker:t".into(),
        role: "Worker".into(),
    })
}

fn answer(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::MessageCompleted {
        run_id: run.into(),
        text: text.into(),
    })
}

/// A log: run-a and run-b interleaved, a row of run-a that cannot be read between
/// them, and a row of no run that cannot be read either.
struct Log {
    _dir: TempDir,
    config: StorageConfig,
    texts: Vec<String>,
}

fn log() -> Log {
    let dir = TempDir::new().expect("temp dir");
    let config = StorageConfig {
        db_path: dir.path().join("deferred.db"),
        ..StorageConfig::default()
    };
    let texts = vec![long_text("alpha"), long_text("beta"), long_text("gamma")];
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    let append = |stream: &str, event: &Event| {
        handle.append_event(Some(stream), event).expect("append");
    };
    append("a", &started("run-a")); // 1
    append("b", &started("run-b")); // 2
    append("a", &answer("run-a", &texts[0])); // 3
    append("b", &answer("run-b", &texts[1])); // 4
    storage.close();
    let raw = Connection::open(&config.db_path).expect("raw connection");
    // 5: a row of run-a whose payload is not an event of this build.
    raw.execute(
        "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
         payload, run_id) VALUES ('a', ?1, 0, 5, 'Message', '{\"kind\":\"Removed\"}', 'run-a')",
        params![event_bus::SCHEMA_VERSION],
    )
    .expect("raw insert");
    // 6: the same, naming no run.
    raw.execute(
        "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
         payload) VALUES ('a', ?1, 0, 6, 'Message', '{\"kind\":\"Removed\"}')",
        params![event_bus::SCHEMA_VERSION],
    )
    .expect("raw insert");
    drop(raw);
    let storage = Storage::open(config.clone()).expect("storage must open");
    storage
        .handle()
        .append_event(Some("a"), &answer("run-a", &texts[2]))
        .expect("append"); // 7
    storage.close();
    Log {
        _dir: dir,
        config,
        texts,
    }
}

fn label(row: &DeferredRow) -> String {
    match row {
        DeferredRow::Event(event) => format!("event {}", event.id),
        DeferredRow::Failed(failure) => format!("failed {}", failure.id),
    }
}

#[test]
fn the_rows_of_a_run_come_in_order_with_an_unreadable_one_in_its_place() {
    let log = log();
    let reader = Database::open_read_only(&log.config.db_path).expect("read-only open");
    let mut rows = Vec::new();

    reader
        .for_each_event_for_runs_deferred(&["run-a".to_owned()], 0, i64::MAX, |row| rows.push(row))
        .unwrap();

    // Run-b's events and the row that names no run are not run-a's.
    assert_eq!(
        rows.iter().map(label).collect::<Vec<_>>(),
        ["event 1", "event 3", "failed 5", "event 7"]
    );
    let DeferredRow::Failed(failure) = &rows[2] else {
        panic!("the failure")
    };
    assert_eq!(failure.run_id.as_deref(), Some("run-a"));
    assert!(failure.wall_clock.is_some(), "its time is still known");
}

#[test]
fn the_bounds_of_a_runs_read_are_exclusive_below_and_inclusive_above() {
    let log = log();
    let reader = Database::open_read_only(&log.config.db_path).expect("read-only open");
    let mut rows = Vec::new();

    reader
        .for_each_event_for_runs_deferred(&["run-a".to_owned()], 1, 5, |row| rows.push(row))
        .unwrap();

    assert_eq!(
        rows.iter().map(label).collect::<Vec<_>>(),
        ["event 3", "failed 5"]
    );
}

#[test]
fn a_visitor_may_read_bodies_on_the_connection_that_is_reading_the_rows() {
    // The GUI turns each row into an event while it is visited, which reads the
    // bodies of nested positions on the same connection.
    let log = log();
    let reader = Database::open_read_only(&log.config.db_path).expect("read-only open");
    let mut read = Vec::new();

    reader
        .for_each_event_for_runs_deferred(&["run-a".to_owned()], 0, i64::MAX, |row| {
            if let DeferredRow::Event(event) = row
                && let EventKind::Message(MessageEvent::MessageCompleted { text, .. }) =
                    &event.event.kind
            {
                let reference = BodyRef::from_token(text).expect("a long text is a token");
                let answers = reader
                    .resolve_bodies(&[reference])
                    .expect("a body is read while the rows are");
                read.push(answers.into_iter().next().unwrap().expect("the text"));
            }
        })
        .unwrap();

    assert_eq!(read, [log.texts[0].clone(), log.texts[2].clone()]);
}

#[test]
fn a_page_after_a_cursor_leaves_out_one_session_and_keeps_unreadable_rows_in_place() {
    let log = log();
    let reader = Database::open_read_only(&log.config.db_path).expect("read-only open");

    let page = reader.deferred_events_after(0, 100, Some("b")).unwrap();

    assert_eq!(
        page.rows.iter().map(label).collect::<Vec<_>>(),
        ["event 1", "event 3", "failed 5", "failed 6", "event 7"],
        "run-b's rows are passed over; both unreadable rows stand where they were"
    );
    assert_eq!((page.last_id, page.scanned), (7, 7));
    // A long text is still a token: nothing was read.
    let DeferredRow::Event(event) = &page.rows[1] else {
        panic!("event")
    };
    let EventKind::Message(MessageEvent::MessageCompleted { text, .. }) = &event.event.kind else {
        panic!("message")
    };
    assert!(BodyRef::from_token(text).is_some());
}

#[test]
fn a_page_ends_at_its_limit_and_the_next_one_starts_after_it() {
    let log = log();
    let reader = Database::open_read_only(&log.config.db_path).expect("read-only open");

    let first = reader.deferred_events_after(0, 3, None).unwrap();
    let second = reader
        .deferred_events_after(first.last_id, 3, None)
        .unwrap();
    let third = reader
        .deferred_events_after(second.last_id, 3, None)
        .unwrap();

    let ids = |page: &storage::DeferredPage| page.rows.iter().map(label).collect::<Vec<_>>();
    assert_eq!(ids(&first), ["event 1", "event 2", "event 3"]);
    assert_eq!(ids(&second), ["event 4", "failed 5", "failed 6"]);
    assert_eq!(ids(&third), ["event 7"]);
    assert_eq!((first.last_id, second.last_id, third.last_id), (3, 6, 7));
    assert!(third.scanned < 3, "a short page reached the end of the log");
    assert!(
        reader
            .deferred_events_after(third.last_id, 3, None)
            .unwrap()
            .rows
            .is_empty()
    );
}
