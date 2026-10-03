use std::collections::HashMap;
use std::time::{Instant, UNIX_EPOCH};

use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventMeta, MessageEvent};

use super::*;
use crate::{Database, StorageConfig};

fn fixture() -> WriterState {
    WriterState {
        conn: Database::open_in_memory().unwrap().conn,
        accounting: event::EventAccounting::default(),
        statistics: Default::default(),
        config: StorageConfig::default(),
        pending: HashMap::new(),
        writes_suspended: false,
        soft_warned: false,
        suspend_logged: false,
        temp_warned: false,
        next_flush_at: Instant::now(),
        next_checkpoint_at: Instant::now(),
        diagnostic_cursor: None,
        reclaim_wal_pending: false,
    }
}

fn event(ns: u64, audit: bool) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::ZERO,
            wall_clock: UNIX_EPOCH + Duration::from_nanos(ns),
        },
        kind: DiagnosticEvent {
            source: "sandbox".into(),
            severity: DiagnosticSeverity::Info,
            code: if audit {
                "escalation_review"
            } else {
                "ordinary"
            }
            .into(),
            detail: "details".into(),
            run_id: None,
            thread_id: None,
            call_id: None,
        }
        .into(),
    }
}

fn append(state: &mut WriterState, event: &Event) -> Result<(), StorageError> {
    super::super::append_event_to_conn(state, &Some("session".into()), event)
}

#[test]
fn retention_bounds_scanned_audits_observes_exclusive_cutoff_and_reseeds_quota() {
    let mut state = fixture();
    // A full batch of audits precedes the disposable rows, sharing a timestamp.
    // The first pass must stop at its scan budget and the second must progress.
    for _ in 0..diagnostic::SCAN_BATCH_SIZE {
        append(&mut state, &event(1, true)).unwrap();
    }
    let day_ns = 86_400_000_000_000;
    let expired = event(day_ns - 1, false);
    let boundary = event(day_ns, false);
    let future = event(day_ns + 1, false);
    let message = Event {
        kind: MessageEvent::MessageDelta {
            delta: "keep".into(),
            run_id: None,
        }
        .into(),
        ..expired.clone()
    };
    for event in [&expired, &expired, &boundary, &future, &message] {
        append(&mut state, event).unwrap();
    }
    state.config.hard_limits.max_session_bytes = state.accounting.session_bytes;
    assert!(append(&mut state, &expired).is_err());
    let now = UNIX_EPOCH + Duration::from_secs(31 * 86_400);

    assert_eq!(retain_recent(&mut state, now).unwrap(), 0);
    assert!(state.diagnostic_cursor.is_some());
    assert_eq!(retain_recent(&mut state, now).unwrap(), 2);
    assert!(state.diagnostic_cursor.is_none());
    let remaining = event::list_all_ordered(&state.conn).unwrap();
    assert_eq!(remaining.len(), diagnostic::SCAN_BATCH_SIZE + 3);
    for event in [&boundary, &future, &message] {
        assert!(remaining.iter().any(|row| &row.event == event));
    }

    // Reuse exactly the freed quota without reconnecting the warm writer.
    append(&mut state, &expired).unwrap();
    append(&mut state, &expired).unwrap();
    assert!(append(&mut state, &expired).is_err());
    // A new pass revisits late/backdated rows instead of losing them behind a cursor.
    assert_eq!(retain_recent(&mut state, now).unwrap(), 0);
    assert_eq!(retain_recent(&mut state, now).unwrap(), 2);
}

#[test]
fn bounded_retention_seeks_past_large_non_diagnostic_history() {
    let mut state = fixture();
    state
        .conn
        .execute_batch(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 10000)
         INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload)
         SELECT 1, 0, x, 'Message', '{}' FROM n;",
        )
        .unwrap();
    append(&mut state, &event(1, false)).unwrap();
    // A VM instruction guard, independent of machine speed, detects a regression
    // to scanning all event rows during an automatic maintenance tick.
    let instructions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = instructions.clone();
    state
        .conn
        .progress_handler(
            100,
            Some(move || observed.fetch_add(100, std::sync::atomic::Ordering::Relaxed) > 5_000),
        )
        .unwrap();
    assert_eq!(
        retain_recent(&mut state, UNIX_EPOCH + Duration::from_secs(31 * 86_400)).unwrap(),
        1
    );
    state
        .conn
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    assert_eq!(
        state
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        10_000
    );
}

#[test]
fn retention_cursor_seeks_past_protected_rows_with_identical_timestamps() {
    let mut state = fixture();
    let audit_payload = serde_json::to_string(&event(1, true).kind).unwrap();
    state
        .conn
        .execute(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 10000)
         INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload)
         SELECT 1, 0, 1, 'Diagnostic', ?1 FROM n;",
            [audit_payload],
        )
        .unwrap();
    state.diagnostic_cursor = Some((1, 9500));
    let instructions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = instructions.clone();
    state
        .conn
        .progress_handler(
            100,
            Some(move || observed.fetch_add(100, std::sync::atomic::Ordering::Relaxed) > 5_000),
        )
        .unwrap();
    assert_eq!(
        retain_recent(&mut state, UNIX_EPOCH + Duration::from_secs(31 * 86_400)).unwrap(),
        0
    );
    state
        .conn
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    assert_eq!(
        state.diagnostic_cursor,
        Some((1, 9500 + diagnostic::SCAN_BATCH_SIZE as i64))
    );
}
