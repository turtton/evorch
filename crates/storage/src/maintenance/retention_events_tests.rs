use std::time::Duration;

use rusqlite::{Connection, params};

use super::*;
use crate::Database;

const DAY: i64 = NANOS_PER_DAY;
const NOW: i64 = 1_000 * DAY;
const RETENTION_DAYS: u32 = 90;

fn open() -> Connection {
    let conn = Database::open_in_memory().unwrap().conn;
    conn.execute(
        "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) VALUES ('s', 'running', 0, 0)",
        [],
    )
    .unwrap();
    conn
}

/// Inserts a row shaped like the real payload (`{"kind": category, "payload":
/// {"kind": tag, ...}}`). `converted == false` stores it the way a database
/// from before the subkind column was filled holds it.
fn insert(conn: &Connection, kind: &str, tag: &str, converted: bool, wall_clock_ns: i64) -> i64 {
    let payload = serde_json::json!({
        "kind": kind,
        "payload": {"kind": tag, "payload": {"padding": "x".repeat(100)}},
    })
    .to_string();
    conn.execute(
        "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind) \
         VALUES ('s', 1, 1, ?1, ?2, ?3, ?4)",
        params![wall_clock_ns, kind, payload, converted.then_some(tag)],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    conn.execute(
        "UPDATE sessions SET total_event_bytes = total_event_bytes + OCTET_LENGTH(?1) WHERE id = 's'",
        [payload],
    )
    .unwrap();
    id
}

fn ids(conn: &Connection) -> Vec<i64> {
    let mut statement = conn.prepare("SELECT id FROM events ORDER BY id").unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap()
}

fn run(conn: &Connection, cursor: &mut Cursor, days: u32) -> Summary {
    run_at(conn, cursor, NOW, days)
}

fn run_at(conn: &Connection, cursor: &mut Cursor, now_ns: i64, days: u32) -> Summary {
    tick(conn, cursor, now_ns, days, Duration::from_secs(3_600)).unwrap()
}

fn old() -> i64 {
    NOW - (i64::from(RETENTION_DAYS) + 1) * DAY
}

fn recent() -> i64 {
    NOW - DAY
}

const PROVIDER: [&str; 3] = [
    "RequestCompleted",
    "FirstTokenObserved",
    "CacheReuseObserved",
];

#[test]
fn removes_leftover_fault_and_progress_rows_but_nothing_else() {
    for converted in [true, false] {
        // Given: removable rows (converted or not) among rows that must stay
        let conn = open();
        let lagged = insert(&conn, "Fault", "SubscriberLagged", converted, recent());
        let progress = insert(&conn, "Lifecycle", "RunProgress", converted, recent());
        let keep = [
            insert(&conn, "Fault", "SkillDiagnostic", converted, recent()),
            insert(&conn, "Lifecycle", "Started", converted, recent()),
            insert(&conn, "Provider", "RequestStarted", converted, old()),
            insert(&conn, "Provider", "RequestFailed", converted, old()),
            insert(&conn, "Provider", "ProviderFallback", converted, old()),
            insert(&conn, "Message", "MessageDelta", converted, old()),
            insert(&conn, "Usage", "Usage", converted, old()),
        ];

        // When: one tick runs
        let summary = run(&conn, &mut Cursor::default(), RETENTION_DAYS);

        // Then: exactly the two removable rows are gone
        assert_eq!(summary.event_count, 2, "converted={converted}");
        assert!(summary.payload_bytes > 0);
        let left = ids(&conn);
        assert!(!left.contains(&lagged) && !left.contains(&progress));
        assert_eq!(left, keep.to_vec(), "converted={converted}");
    }
}

#[test]
fn provider_observations_expire_only_outside_the_window() {
    for converted in [true, false] {
        let conn = open();
        let mut expired = Vec::new();
        let mut kept = Vec::new();
        for tag in PROVIDER {
            expired.push(insert(&conn, "Provider", tag, converted, old()));
            // A row exactly at the cutoff is inside the window.
            kept.push(insert(
                &conn,
                "Provider",
                tag,
                converted,
                NOW - i64::from(RETENTION_DAYS) * DAY,
            ));
            kept.push(insert(&conn, "Provider", tag, converted, recent()));
        }

        let summary = run(&conn, &mut Cursor::default(), RETENTION_DAYS);

        assert_eq!(summary.event_count, expired.len() as u64);
        assert_eq!(ids(&conn), kept, "converted={converted}");
    }
}

#[test]
fn zero_days_keeps_provider_observations_but_still_removes_leftover_noise() {
    let conn = open();
    for tag in PROVIDER {
        insert(&conn, "Provider", tag, true, old());
        insert(&conn, "Provider", tag, false, old());
    }
    insert(&conn, "Fault", "SubscriberLagged", true, old());

    let mut cursor = Cursor::default();
    let first = run(&conn, &mut cursor, 0);
    let second = run(&conn, &mut cursor, 0);

    assert_eq!(first.event_count, 1);
    assert_eq!(second.event_count, 0);
    assert_eq!(count(&conn), 6);
}

#[test]
fn a_tick_never_deletes_more_than_its_budget_and_later_ticks_finish() {
    let conn = open();
    let total = MAX_DELETIONS_PER_TICK + 700;
    for index in 0..total {
        // Spread over every target: converted/unconverted, fault/progress/provider.
        match index % 4 {
            0 => insert(&conn, "Fault", "SubscriberLagged", true, recent()),
            1 => insert(&conn, "Lifecycle", "RunProgress", false, recent()),
            2 => insert(&conn, "Provider", "FirstTokenObserved", true, old()),
            _ => insert(&conn, "Provider", "RequestCompleted", false, old()),
        };
    }
    let survivor = insert(&conn, "Lifecycle", "Started", true, recent());
    let mut cursor = Cursor::default();

    let first = run(&conn, &mut cursor, RETENTION_DAYS);
    assert!(first.event_count > 0 && first.event_count <= MAX_DELETIONS_PER_TICK as u64);
    assert_eq!(count(&conn), (total as u64 - first.event_count + 1) as i64);

    // Each target only scans a bounded window, so draining takes a few ticks.
    let mut removed = first.event_count;
    for _ in 0..20 {
        let summary = run(&conn, &mut cursor, RETENTION_DAYS);
        assert!(summary.event_count <= MAX_DELETIONS_PER_TICK as u64);
        removed += summary.event_count;
    }
    assert_eq!(removed, total as u64);
    assert_eq!(ids(&conn), vec![survivor]);
}

#[test]
fn filtering_targets_examine_a_bounded_window_and_resume_where_they_stopped() {
    for converted in [true, false] {
        // Given: more in-window rows than one scan covers, then an expired one
        let conn = open();
        for _ in 0..MAX_SCANNED_PER_TARGET + 500 {
            insert(&conn, "Provider", "RequestCompleted", converted, recent());
        }
        let expired = insert(&conn, "Provider", "RequestCompleted", converted, old());
        let mut cursor = Cursor::default();

        // When/Then: the first tick stops short of it, a following one reaches it
        assert_eq!(run(&conn, &mut cursor, RETENTION_DAYS).event_count, 0);
        assert!(ids(&conn).contains(&expired));
        assert_eq!(run(&conn, &mut cursor, RETENTION_DAYS).event_count, 1);
        assert!(!ids(&conn).contains(&expired));
        assert_eq!(count(&conn), (MAX_SCANNED_PER_TARGET + 500) as i64);
    }
}

#[test]
fn a_budget_that_runs_out_inside_a_target_resumes_at_the_next_row() {
    let conn = open();
    for _ in 0..MAX_DELETIONS_PER_TICK - 500 {
        insert(&conn, "Fault", "SubscriberLagged", true, recent());
    }
    let progress: Vec<i64> = (0..MAX_SCANNED_PER_TARGET)
        .map(|_| insert(&conn, "Lifecycle", "RunProgress", false, recent()))
        .collect();
    let mut cursor = Cursor::default();

    let first = run(&conn, &mut cursor, RETENTION_DAYS);

    // The ordered target stops after the 500th row, exactly at the budget.
    assert_eq!(first.event_count, MAX_DELETIONS_PER_TICK as u64);
    assert_eq!(ids(&conn), progress[500..].to_vec());
    let second = run(&conn, &mut cursor, RETENTION_DAYS);
    assert_eq!(second.event_count, 500);
    assert_eq!(count(&conn), 0);
}

#[test]
fn an_unconditional_target_larger_than_the_budget_finishes_over_two_ticks() {
    let conn = open();
    for _ in 0..MAX_DELETIONS_PER_TICK + 300 {
        insert(&conn, "Fault", "SubscriberLagged", true, recent());
    }
    let mut cursor = Cursor::default();

    assert_eq!(
        run(&conn, &mut cursor, RETENTION_DAYS).event_count,
        MAX_DELETIONS_PER_TICK as u64
    );
    assert_eq!(count(&conn), 300);
    assert_eq!(run(&conn, &mut cursor, RETENTION_DAYS).event_count, 300);
    assert_eq!(count(&conn), 0);
}

#[test]
fn a_row_that_expires_behind_the_cursor_is_only_found_after_the_pass_wraps() {
    let conn = open();
    // Inside the window now, but its low id is already behind the cursor
    // once the first tick has scanned past it.
    let aging = insert(
        &conn,
        "Provider",
        "RequestCompleted",
        true,
        NOW - (i64::from(RETENTION_DAYS) - 1) * DAY,
    );
    for _ in 0..MAX_SCANNED_PER_TARGET + 500 {
        insert(&conn, "Provider", "RequestCompleted", true, recent());
    }
    let mut cursor = Cursor::default();
    let later = NOW + 2 * DAY;

    assert_eq!(
        run_at(&conn, &mut cursor, NOW, RETENTION_DAYS).event_count,
        0
    );
    // The row is expired now, but the pass has moved on and finishes first.
    assert_eq!(
        run_at(&conn, &mut cursor, later, RETENTION_DAYS).event_count,
        0
    );
    assert!(ids(&conn).contains(&aging));
    // The finished pass starts over and reaches it.
    assert_eq!(
        run_at(&conn, &mut cursor, later, RETENTION_DAYS).event_count,
        1
    );
    assert!(!ids(&conn).contains(&aging));
}

#[test]
fn a_finished_pass_restarts_and_finds_rows_that_arrived_behind_the_cursor() {
    let conn = open();
    insert(&conn, "Provider", "CacheReuseObserved", true, recent());
    let mut cursor = Cursor::default();
    assert_eq!(run(&conn, &mut cursor, RETENTION_DAYS).event_count, 0);

    // A backdated arrival gets a higher id, but the cursor already wrapped.
    let late = insert(&conn, "Provider", "CacheReuseObserved", true, old());
    assert_eq!(run(&conn, &mut cursor, RETENTION_DAYS).event_count, 1);
    assert!(!ids(&conn).contains(&late));
}

#[test]
fn unreadable_payloads_without_a_subkind_are_left_alone() {
    let conn = open();
    for payload in ["not json", "{\"kind\":\"Fault\"}", "[]"] {
        conn.execute(
            "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
             VALUES ('s', 1, 1, ?1, 'Fault', ?2)",
            params![old(), payload],
        )
        .unwrap();
    }

    let summary = run(&conn, &mut Cursor::default(), RETENTION_DAYS);

    assert_eq!(summary.event_count, 0);
    assert_eq!(count(&conn), 3);
}

#[test]
fn accounting_aggregates_and_session_totals_shrink_with_the_deleted_rows() {
    let conn = open();
    let keep = insert(&conn, "Lifecycle", "Started", true, recent());
    for converted in [true, false] {
        insert(&conn, "Provider", "RequestCompleted", converted, old());
        insert(&conn, "Lifecycle", "RunProgress", converted, old());
    }
    let day_bytes = |conn: &Connection| -> i64 {
        conn.query_row(
            "SELECT COALESCE(SUM(payload_bytes), 0) FROM event_day_bytes",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let session_bytes = |conn: &Connection| -> i64 {
        conn.query_row(
            "SELECT COALESCE(SUM(payload_bytes), 0) FROM event_session_bytes",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let total_event_bytes = |conn: &Connection| -> i64 {
        conn.query_row(
            "SELECT total_event_bytes FROM sessions WHERE id = 's'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let kept_bytes: i64 = conn
        .query_row(
            "SELECT OCTET_LENGTH(payload) FROM events WHERE id = ?1",
            [keep],
            |row| row.get(0),
        )
        .unwrap();
    let before = day_bytes(&conn);
    assert!(before > kept_bytes);

    let summary = run(&conn, &mut Cursor::default(), RETENTION_DAYS);

    assert_eq!(summary.event_count, 4);
    assert_eq!(day_bytes(&conn), kept_bytes);
    assert_eq!(before - day_bytes(&conn), summary.payload_bytes as i64);
    assert_eq!(session_bytes(&conn), kept_bytes);
    assert_eq!(total_event_bytes(&conn), kept_bytes);
}

#[test]
fn candidate_lookup_seeks_the_kind_subkind_index() {
    let conn = open();
    for unconverted in [false, true] {
        let sql = format!("EXPLAIN QUERY PLAN {}", candidate_sql(unconverted));
        let mut statement = conn.prepare(&sql).unwrap();
        let read = |row: &rusqlite::Row<'_>| row.get::<_, String>(3);
        let plan: Vec<String> = if unconverted {
            statement.query_map(params!["Fault", 0_i64, 10_i64], read)
        } else {
            statement.query_map(params!["Fault", 0_i64, 10_i64, "SubscriberLagged"], read)
        }
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
        assert!(
            plan.iter().any(|line| line.contains("SEARCH")
                && line.contains("idx_events_kind_subkind_id")
                && line.contains("kind=?")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }
}

#[test]
fn a_spent_time_budget_still_makes_progress_and_flags_a_backlog() {
    let conn = open();
    let total = TIME_CHECK_ROWS as usize * 3 + 5;
    for _ in 0..total {
        insert(&conn, "Fault", "SubscriberLagged", true, recent());
    }
    let mut cursor = Cursor::default();

    let first = tick(&conn, &mut cursor, NOW, RETENTION_DAYS, Duration::ZERO).unwrap();

    // The clock is only read after TIME_CHECK_ROWS deletions.
    assert_eq!(first.event_count, TIME_CHECK_ROWS);
    assert!(cursor.has_backlog());
    let mut removed = first.event_count;
    while count(&conn) > 0 {
        removed += tick(&conn, &mut cursor, NOW, RETENTION_DAYS, Duration::ZERO)
            .unwrap()
            .event_count;
    }
    assert_eq!(removed, total as u64);
    assert!(!cursor.has_backlog());
}

#[test]
fn reaching_the_deletion_cap_flags_a_backlog_and_finishing_clears_it() {
    let conn = open();
    for _ in 0..MAX_DELETIONS_PER_TICK + 1 {
        insert(&conn, "Fault", "SubscriberLagged", true, recent());
    }
    let mut cursor = Cursor::default();

    run(&conn, &mut cursor, RETENTION_DAYS);
    assert!(cursor.has_backlog());
    run(&conn, &mut cursor, RETENTION_DAYS);
    assert!(!cursor.has_backlog());
}
