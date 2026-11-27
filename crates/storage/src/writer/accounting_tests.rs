//! Writer lifetime accounting must keep appends independent of stored history size.

use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};

use event_bus::{EventMeta, MessageEvent};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

use super::tests::capture_logs;
use super::*;
use crate::{Database, LimitKind};

const DAILY_WARNING: &str = "daily event byte limit exceeded";

fn writer_state(conn: Connection, config: StorageConfig) -> WriterState {
    let now = Instant::now();
    WriterState {
        conn,
        accounting: event::EventAccounting::default(),
        statistics: StorageStatistics::default(),
        next_flush_at: now + config.flush_interval,
        next_checkpoint_at: now + config.checkpoint_interval,
        config,
        pending: HashMap::new(),
        writes_suspended: false,
        soft_warned: false,
        over_limit_warned: false,
        temp_warned: false,
        diagnostic_cursor: None,
        reclaim_wal_pending: false,
        reclaim_after: None,
        index: crate::writer::index::IndexJob::inert(),
        task_progress_cursor: Default::default(),
        event_retention_cursor: Default::default(),
    }
}

fn fixture() -> WriterState {
    writer_state(
        Database::open_in_memory().unwrap().conn,
        StorageConfig::default(),
    )
}

fn event_at(seconds: u64, text: &str) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_secs(seconds),
            wall_clock: UNIX_EPOCH + Duration::from_secs(seconds),
        },
        kind: MessageEvent::MessageDelta {
            delta: text.into(),
            run_id: None,
        }
        .into(),
    }
}

fn payload_bytes(event: &Event) -> u64 {
    serde_json::to_vec(&event.kind).unwrap().len() as u64
}

fn append(
    state: &mut WriterState,
    session: Option<&str>,
    event: &Event,
) -> Result<(), StorageError> {
    append_event_to_conn(state, &session.map(str::to_owned), event)
}

fn event_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap()
}

fn limit_error(limit: LimitKind, actual: u64, max: u64) -> StorageError {
    StorageError::LimitExceeded { limit, actual, max }
}

#[test]
fn warm_writer_appends_growing_history_without_reading_stored_payloads() {
    let mut state = fixture();
    let event = event_at(1, "日本語の差分");
    append(&mut state, Some("session"), &event).unwrap();

    // Installing the authorizer also invalidates already prepared statements, so
    // caching a SUM statement cannot hide a repeated scan from this regression.
    state
        .conn
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Read {
                    table_name: "events",
                    column_name: "payload",
                }
            ) && context.accessor != Some("events_accounting_insert")
            {
                // INSERT bookkeeping reads NEW.payload only. A trigger read is
                // permitted; ordinary history SELECT/SUM remains forbidden.
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    assert!(
        state
            .conn
            .query_row("SELECT SUM(OCTET_LENGTH(payload)) FROM events", [], |row| {
                row.get::<_, i64>(0)
            })
            .is_err(),
        "the regression guard must reject history aggregation"
    );

    for _ in 0..512 {
        append(&mut state, Some("session"), &event)
            .expect("a warm append must not read any stored payload");
    }
    assert_eq!(event_count(&state.conn), 513);
    assert_eq!(state.accounting.session_bytes, payload_bytes(&event) * 513);
    assert_eq!(state.accounting.day_bytes, payload_bytes(&event) * 513);
}

#[test]
fn switching_sessions_and_sessionless_events_preserves_each_session_limit() {
    let mut state = fixture();
    let event = event_at(1, "delta");
    let bytes = payload_bytes(&event);
    state.config.hard_limits.max_session_bytes = bytes * 2;

    for session in [
        Some("first"),
        None,
        Some("second"),
        Some("second"),
        Some("first"),
    ] {
        append(&mut state, session, &event).unwrap();
    }
    for session in ["first", "second"] {
        assert_eq!(
            append(&mut state, Some(session), &event),
            Err(limit_error(LimitKind::SessionSize, bytes * 3, bytes * 2))
        );
    }
    append(&mut state, None, &event).unwrap();
    assert_eq!(event_count(&state.conn), 6);
    assert_eq!(state.accounting.day_bytes, bytes * 6);
}

#[test]
fn utc_day_switch_and_out_of_order_events_reseed_daily_accounting() {
    let mut state = fixture();
    let bytes = payload_bytes(&event_at(1, "delta"));
    state.config.hard_limits.max_daily_event_bytes = bytes * 4;

    // Preserve the existing daily policy: all events at or after this event's
    // UTC midnight count, including later timestamps for out-of-order appends.
    for (seconds, expected_total) in [(86_399, 1), (86_400, 1), (1, 3), (86_401, 2)] {
        append(&mut state, None, &event_at(seconds, "delta")).unwrap();
        assert_eq!(state.accounting.day_bytes, bytes * expected_total);
    }
    // The daily limit only warns, once per UTC day: the overrun is written and logged.
    let logs = capture_logs(|| {
        append(&mut state, None, &event_at(2, "delta")).unwrap();
        append(&mut state, None, &event_at(3, "delta")).unwrap();
    });
    assert_eq!(logs.matches(DAILY_WARNING).count(), 1, "{logs}");
    assert_eq!(state.accounting.day_bytes, bytes * 6);
    // The earlier-day overrun must not corrupt the current day's cache or warn for it.
    let logs = capture_logs(|| {
        append(&mut state, None, &event_at(86_402, "delta")).unwrap();
    });
    assert!(!logs.contains(DAILY_WARNING), "{logs}");
    assert_eq!(state.accounting.day_bytes, bytes * 3);
    assert_eq!(event_count(&state.conn), 7);
}

#[test]
fn evicted_session_reseeds_without_resetting_its_limit() {
    let mut state = fixture();
    let event = event_at(1, "delta");
    let bytes = payload_bytes(&event);
    state.config.hard_limits.max_session_bytes = bytes * 2;
    // More sessions than the bounded LRU can retain.
    for index in 0..65 {
        append(&mut state, Some(&format!("session-{index}")), &event).unwrap();
    }
    append(&mut state, Some("session-64"), &event).unwrap();
    append(&mut state, Some("session-0"), &event).unwrap();
    assert_eq!(
        append(&mut state, Some("session-0"), &event),
        Err(limit_error(LimitKind::SessionSize, bytes * 3, bytes * 2))
    );
    assert_eq!(event_count(&state.conn), 67);
}

#[test]
fn rejected_event_and_rolled_back_insert_leave_capacity_available() {
    let mut state = fixture();
    let event = event_at(1, "delta");
    let bytes = payload_bytes(&event);
    state.config.hard_limits.max_session_bytes = bytes * 2;
    append(&mut state, Some("session"), &event).unwrap();
    let accepted = state.accounting.clone();

    let oversized = event_at(2, &"x".repeat(bytes as usize * 2));
    assert!(matches!(
        append(&mut state, Some("session"), &oversized),
        Err(StorageError::LimitExceeded {
            limit: LimitKind::SessionSize,
            ..
        })
    ));
    assert_eq!(state.accounting, accepted);

    state
        .conn
        .execute_batch(
            "CREATE TRIGGER reject_event BEFORE INSERT ON events
             BEGIN SELECT RAISE(ABORT, 'forced insert failure'); END;",
        )
        .unwrap();
    assert!(append(&mut state, Some("session"), &event).is_err());
    assert_eq!(state.accounting, accepted);
    assert_eq!(event_count(&state.conn), 1);
    state
        .conn
        .execute_batch("DROP TRIGGER reject_event")
        .unwrap();

    append(&mut state, Some("session"), &event).unwrap();
    assert_eq!(event_count(&state.conn), 2);
    assert_eq!(state.accounting.session_bytes, bytes * 2);
    assert_eq!(state.accounting.day_bytes, bytes * 2);
}

#[test]
fn external_commits_and_reopen_reseed_session_and_daily_limits() {
    let event = event_at(1, "delta");
    let bytes = payload_bytes(&event);
    let dir = tempfile::tempdir().unwrap();
    let mut config = StorageConfig {
        db_path: dir.path().join("accounting.db"),
        ..Default::default()
    };
    config.hard_limits.max_session_bytes = bytes * 3;
    let mut first = writer_state(Database::open(&config).unwrap().conn, config.clone());
    let mut external = writer_state(Database::open(&config).unwrap().conn, config.clone());
    append(&mut first, Some("session"), &event).unwrap();
    append(&mut external, Some("session"), &event).unwrap();
    append(&mut first, Some("session"), &event).unwrap();
    let expected = Err(limit_error(LimitKind::SessionSize, bytes * 4, bytes * 3));
    assert_eq!(append(&mut first, Some("session"), &event), expected);
    assert_eq!(event_count(&first.conn), 3);
    drop(first);
    drop(external);

    let mut reopened = writer_state(Database::open(&config).unwrap().conn, config);
    assert_eq!(append(&mut reopened, Some("session"), &event), expected);
    assert_eq!(event_count(&reopened.conn), 3);
}

#[test]
fn stream_and_normal_commands_share_daily_totals_and_restore_session_limits() {
    let event = event_at(1, "delta");
    let bytes = payload_bytes(&event);
    let config = StorageConfig {
        hard_limits: HardLimits {
            max_session_bytes: bytes,
            max_daily_event_bytes: bytes * 3,
            ..Default::default()
        },
        ..Default::default()
    };
    let (tx, rx) = mpsc::channel();
    let queue = |stream: bool, session: &str| {
        let (reply, result) = mpsc::channel();
        let command = if stream {
            Command::AppendStreamEvent(session.into(), event.clone(), None, reply)
        } else {
            Command::AppendEvent(Some(session.into()), event.clone(), reply)
        };
        tx.send(command).unwrap();
        result
    };
    let first = queue(false, "session");
    let stream_first = queue(true, "gui");
    let session_overflow = queue(false, "session");
    let stream_second = queue(true, "gui");
    let normal_daily_overflow = queue(false, "another-session");
    let stream_daily_overflow = queue(true, "gui");
    tx.send(Command::Shutdown).unwrap();

    let logs = capture_logs(|| {
        run_writer(
            Database::open_in_memory().unwrap().conn,
            rx,
            config,
            false,
            false,
            false,
            false,
        );
    });
    assert_eq!(first.recv().unwrap(), Ok(()));
    assert_eq!(stream_first.recv().unwrap(), Ok(()));
    assert_eq!(
        session_overflow.recv().unwrap(),
        Err(limit_error(LimitKind::SessionSize, bytes * 2, bytes))
    );
    assert_eq!(stream_second.recv().unwrap(), Ok(()));
    // The daily limit only warns: both paths keep writing and share one warning.
    for result in [normal_daily_overflow, stream_daily_overflow] {
        assert_eq!(result.recv().unwrap(), Ok(()));
    }
    assert_eq!(logs.matches(DAILY_WARNING).count(), 1, "{logs}");
}

#[test]
fn invalidating_the_accounting_cache_does_not_repeat_the_daily_warning() {
    let mut state = fixture();
    let event = event_at(1, "delta");
    state.config.hard_limits.max_daily_event_bytes = payload_bytes(&event);
    let logs = capture_logs(|| {
        append(&mut state, None, &event).unwrap();
        append(&mut state, None, &event).unwrap();
    });
    assert_eq!(logs.matches(DAILY_WARNING).count(), 1, "{logs}");

    // Cleanup and retention drop the cache; the same UTC day must not warn again.
    state.accounting = state.accounting.invalidated();
    let logs = capture_logs(|| append(&mut state, None, &event).unwrap());
    assert!(!logs.contains(DAILY_WARNING), "{logs}");
    assert_eq!(event_count(&state.conn), 3);
}
