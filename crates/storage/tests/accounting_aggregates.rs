//! Transactional byte quotas stay correct across raw SQL changes and writers.

use std::time::{Duration, UNIX_EPOCH};

use event_bus::{Event, EventMeta, MessageEvent};
use rusqlite::{Connection, params};
use storage::{Database, HardLimits, LimitKind, Storage, StorageConfig, StorageError};

const DAY: i64 = 86_400_000_000_000;

fn config(path: std::path::PathBuf) -> StorageConfig {
    StorageConfig {
        db_path: path,
        flush_interval: Duration::from_secs(3_600),
        checkpoint_interval: Duration::from_secs(3_600),
        ..Default::default()
    }
}

fn raw_insert(conn: &Connection, session: Option<&str>, ns: i64, payload: &str) -> i64 {
    conn.execute(
        "INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload)
         VALUES (?1,1,0,?2,'Message',?3)",
        params![session, ns, payload],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn assert_totals_match_events(conn: &Connection) {
    // EXCEPT in both directions also catches missing aggregate rows. Zero-byte
    // rows may be omitted after deletion, and must not affect the sums.
    let sessions: i64 = conn.query_row(
        "WITH actual AS (SELECT session_id, SUM(OCTET_LENGTH(payload)) AS bytes FROM events
                           WHERE session_id IS NOT NULL GROUP BY session_id HAVING bytes > 0),
              totals AS (SELECT session_id, payload_bytes FROM event_session_bytes WHERE payload_bytes > 0)
         SELECT (SELECT COUNT(*) FROM (SELECT * FROM actual EXCEPT SELECT * FROM totals))
              + (SELECT COUNT(*) FROM (SELECT * FROM totals EXCEPT SELECT * FROM actual))",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(sessions, 0, "session totals must match UTF-8 payload bytes");
    let days: i64 = conn.query_row(
        "WITH actual AS (SELECT wall_clock_ns / 86400000000000 - (wall_clock_ns % 86400000000000 < 0),
                                 SUM(OCTET_LENGTH(payload)) AS bytes FROM events GROUP BY 1 HAVING bytes > 0),
              totals AS (SELECT utc_day, payload_bytes FROM event_day_bytes WHERE payload_bytes > 0)
         SELECT (SELECT COUNT(*) FROM (SELECT * FROM actual EXCEPT SELECT * FROM totals))
              + (SELECT COUNT(*) FROM (SELECT * FROM totals EXCEPT SELECT * FROM actual))",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(
        days, 0,
        "daily totals must match all events, including NULL sessions"
    );
}

#[test]
fn raw_inserts_updates_deletes_and_rollbacks_preserve_totals() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path().join("aggregate.db"));
    drop(Database::open(&config).unwrap());
    let conn = Connection::open(&config.db_path).unwrap();
    let first = raw_insert(&conn, Some("session"), DAY - 1, "日本語");
    let second = raw_insert(&conn, Some("stream"), DAY, "ab");
    raw_insert(&conn, None, DAY + 1, "null");
    raw_insert(&conn, None, -1, "pre-epoch");
    raw_insert(&conn, Some("empty"), i64::MIN, "");
    assert_totals_match_events(&conn);
    conn.execute(
        "UPDATE events SET session_id = NULL, wall_clock_ns = ?1, payload = 'changed' WHERE id = ?2",
        params![2 * DAY, first],
    ).unwrap();
    assert_totals_match_events(&conn);
    conn.execute(
        "UPDATE events SET session_id = 'new', wall_clock_ns = -1, payload = '変更' WHERE id = ?1",
        [second],
    )
    .unwrap();
    assert_totals_match_events(&conn);
    conn.execute_batch("BEGIN").unwrap();
    raw_insert(&conn, Some("rolled-back"), 2 * DAY, "uncommitted");
    conn.execute("DELETE FROM events WHERE id = ?1", [first])
        .unwrap();
    conn.execute(
        "UPDATE events SET payload = 'temporary' WHERE id = ?1",
        [second],
    )
    .unwrap();
    assert_totals_match_events(&conn);
    conn.execute_batch("ROLLBACK").unwrap();
    assert_totals_match_events(&conn);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM event_session_bytes WHERE session_id = 'rolled-back'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    conn.execute_batch("DELETE FROM events").unwrap();
    assert_totals_match_events(&conn);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM event_session_bytes", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM event_day_bytes", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn replacements_cannot_silently_bypass_delete_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path().join("replace.db"));
    drop(Database::open(&config).unwrap());
    let conn = Connection::open(&config.db_path).unwrap();
    conn.pragma_update(None, "recursive_triggers", false)
        .unwrap();
    let first = raw_insert(&conn, Some("session"), DAY, "first");
    let second = raw_insert(&conn, Some("session"), DAY, "second");
    assert!(conn.execute(
        "INSERT OR REPLACE INTO events(id,session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload)
         VALUES (?1,'replacement',1,0,0,'Message','bad')", [first],
    ).is_err());
    assert!(
        conn.execute(
            "UPDATE OR REPLACE events SET id = ?1 WHERE id = ?2",
            params![first, second]
        )
        .is_err()
    );
    assert_totals_match_events(&conn);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

fn event(day: u64) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::ZERO,
            wall_clock: UNIX_EPOCH + Duration::from_secs(day * 86_400 + 1),
        },
        kind: MessageEvent::MessageDelta {
            delta: "日本語".into(),
            run_id: None,
        }
        .into(),
    }
}

#[test]
fn alternating_writers_reseed_from_totals_and_enforce_exact_limits_after_external_changes() {
    let event = event(1);
    let bytes = serde_json::to_vec(&event.kind).unwrap().len() as u64;
    for limit in [LimitKind::SessionSize, LimitKind::DailyBytes] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path().join("writers.db"));
        config.hard_limits = HardLimits {
            max_session_bytes: if limit == LimitKind::SessionSize {
                bytes * 3
            } else {
                u64::MAX
            },
            max_daily_event_bytes: if limit == LimitKind::DailyBytes {
                bytes * 3
            } else {
                u64::MAX
            },
            ..Default::default()
        };
        let first = Storage::open(config.clone()).unwrap();
        let second = Storage::open(config.clone()).unwrap();
        let a = first.handle();
        let b = second.handle();
        a.append_event(Some("session"), &event).unwrap();
        b.append_event(Some("session"), &event).unwrap();
        a.append_event(Some("session"), &event).unwrap();
        let overflow = || StorageError::LimitExceeded {
            limit,
            actual: bytes * 4,
            max: bytes * 3,
        };
        assert_eq!(b.append_event(Some("session"), &event), Err(overflow()));
        let conn = Connection::open(&config.db_path).unwrap();
        // Removing committed history must release capacity for either writer.
        conn.execute_batch("DELETE FROM events WHERE id = (SELECT MIN(id) FROM events)")
            .unwrap();
        b.append_event(Some("session"), &event).unwrap();
        assert_eq!(a.append_event(Some("session"), &event), Err(overflow()));
        if limit == LimitKind::DailyBytes {
            // Raw events before this UTC day do not count; later dates do count.
            conn.execute_batch(
                "UPDATE events SET wall_clock_ns = 0 WHERE id = (SELECT MIN(id) FROM events)",
            )
            .unwrap();
            a.append_event(Some("session"), &event).unwrap();
            assert_eq!(b.append_event(Some("session"), &event), Err(overflow()));
        } else {
            conn.execute_batch(
                "UPDATE events SET session_id = NULL WHERE id = (SELECT MIN(id) FROM events)",
            )
            .unwrap();
            a.append_event(Some("session"), &event).unwrap();
            assert_eq!(b.append_event(Some("session"), &event), Err(overflow()));
        }
        assert_totals_match_events(&conn);
        first.close();
        second.close();
    }
}

#[test]
fn earlier_day_limit_counts_later_days_but_excludes_pre_epoch_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path().join("days.db"));
    let bytes = serde_json::to_vec(&event(0).kind).unwrap().len() as u64;
    config.hard_limits.max_daily_event_bytes = bytes * 2;
    let storage = Storage::open(config.clone()).unwrap();
    let conn = Connection::open(&config.db_path).unwrap();
    raw_insert(&conn, None, -1, &"x".repeat(bytes as usize * 3));
    let handle = storage.handle();
    handle.append_event(None, &event(2)).unwrap();
    handle.append_event(None, &event(0)).unwrap();
    assert_eq!(
        handle.append_event(None, &event(0)),
        Err(StorageError::LimitExceeded {
            limit: LimitKind::DailyBytes,
            actual: bytes * 3,
            max: bytes * 2,
        })
    );
    assert_totals_match_events(&conn);
    storage.close();
}
