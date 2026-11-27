//! Real file-backed WAL pipeline; explicit ticks, no wall-clock correctness gate.
use super::super::*;
use crate::{
    Database,
    thread_retention::{BindingInput, NANOS_PER_DAY, tool_bodies::Request},
};
use event_bus::{Event, EventMeta, ToolEvent};
use std::time::{Duration, UNIX_EPOCH};
fn scalar(conn: &Connection, sql: &str) -> u64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}
fn noise(mut n: u64) -> String {
    let mut s = String::new();
    for _ in 0..4096 {
        n ^= n << 13;
        n ^= n >> 7;
        n ^= n << 17;
        s.push_str(&format!("{n:016x}"));
    }
    s
}
#[test]
fn tool_body_retention_wal_pipeline_bounds_gc_vacuum_and_allows_writes_between_ticks() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("pipeline.db"),
        // The warn-only lane is now on main: all stages also progress over limits.
        hard_limits: HardLimits {
            max_db_bytes: 1,
            max_wal_bytes: 1,
            max_daily_event_bytes: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let db = Database::open(&config).unwrap();
    let conn = &db.conn;
    assert_eq!(scalar(conn, "PRAGMA auto_vacuum"), 2);
    assert_eq!(
        conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    conn.execute_batch("INSERT INTO threads(thread_id,root_run_id,created_at_ns,updated_at_ns,source) VALUES('t','r',0,0,'host');INSERT INTO thread_runs VALUES('r','t','r',0);INSERT INTO retention_binding VALUES(1,'root','sidebar',0);").unwrap();
    let tx = immediate_transaction(conn).unwrap();
    let mut accounting = event::EventAccounting::default();
    for n in 1..=320 {
        let e = Event {
            meta: EventMeta {
                schema_version: event_bus::SCHEMA_VERSION,
                monotonic: Duration::ZERO,
                wall_clock: UNIX_EPOCH,
            },
            kind: ToolEvent::ToolCompleted {
                tool_name: "shell".into(),
                call_id: format!("c{n}"),
                is_error: false,
                output: Some(noise(n)),
                detail: None,
                run_id: Some("r".into()),
                parent_call_id: None,
            }
            .into(),
        };
        event::append_event_in_transaction(&tx, None, &e, &HardLimits::default(), &mut accounting)
            .unwrap();
    }
    tx.commit().unwrap();
    let mut request = Request {
        db_identity: db.retention_identity().unwrap(),
        binding: BindingInput {
            ownership_root: "root".into(),
            sidebar_path: "sidebar".into(),
        },
        expected: db.tool_body_canonical("t").unwrap().unwrap(),
        run_id: "r".into(),
        after_event: 0,
        now_ns: 8 * NANOS_PER_DAY,
        after_days: 7,
    };
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let file_before = std::fs::metadata(&config.db_path).unwrap().len();
    let now = Instant::now();
    let mut state = WriterState {
        conn: db.conn,
        accounting,
        statistics: StorageStatistics::default(),
        config: config.clone(),
        pending: HashMap::new(),
        writes_suspended: false,
        soft_warned: false,
        over_limit_warned: false,
        temp_warned: false,
        next_flush_at: now,
        next_checkpoint_at: now,
        diagnostic_cursor: None,
        reclaim_wal_pending: false,
        reclaim_after: None,
        index: crate::writer::index::IndexJob::inert(),
        task_progress_cursor: Default::default(),
        event_retention_cursor: Default::default(),
    };
    let other = Database::open(&config).unwrap();
    let mut changed = 0;
    for tick in 0..16 {
        let tx = immediate_transaction(&state.conn).unwrap();
        let p = crate::repo::event::tool_body_retention::apply(&tx, &request).unwrap();
        tx.commit().unwrap();
        request.after_event = p.after_event;
        changed += p.changed;
        assert!(
            p.changed <= 256
                && p.stored_bytes <= 2 * 1024 * 1024
                && p.decoded_bytes <= 2 * 1024 * 1024
                && p.old_refs <= 2000
                && p.new_refs <= 2000
        );
        let pages = scalar(&state.conn, "PRAGMA page_count");
        let blobs = scalar(&state.conn, "SELECT count(*) FROM content_blobs");
        let bytes = scalar(
            &state.conn,
            "SELECT coalesce(sum(octet_length(data)),0) FROM content_blobs",
        );
        maintenance(&mut state).unwrap();
        // incremental_vacuum budgets free-list pages, not pointer-map pages.
        // With 4096 usable bytes, a pointer-map page occurs at 2 + 820*k.
        // SQLite also truncates that housekeeping page when it crosses it.
        assert_eq!(scalar(&state.conn, "PRAGMA page_size"), 4096);
        let after = scalar(&state.conn, "PRAGMA page_count");
        let pointer_maps = |n: u64| if n < 2 { 0 } else { 1 + (n - 2) / 820 };
        let reclaimed = pages
            .saturating_sub(after)
            .saturating_sub(pointer_maps(pages).saturating_sub(pointer_maps(after)));
        assert!(
            reclaimed <= config.vacuum_page_budget_per_tick,
            "tick={tick} before={pages} after={after} reclaimed={reclaimed}"
        );
        assert!(
            blobs.saturating_sub(scalar(&state.conn, "SELECT count(*) FROM content_blobs")) <= 256
        );
        assert!(
            bytes.saturating_sub(scalar(
                &state.conn,
                "SELECT coalesce(sum(octet_length(data)),0) FROM content_blobs"
            )) <= 8 * 1024 * 1024
        );
        // Independent connection writes between every tick. No long transaction is
        // retained by the scan, GC or drained incremental_vacuum statement.
        other.conn.execute("INSERT INTO sessions(id,status,created_at_ns,updated_at_ns) VALUES(?1,'running',0,0)",[format!("foreground-{tick}")]).unwrap();
        event::append_event(
            &other.conn,
            None,
            &Event::new(event_bus::MessageEvent::MessageCompleted {
                run_id: "foreground".into(),
                text: format!("write-{tick}"),
            }),
            &config.hard_limits,
            &mut event::EventAccounting::default(),
        )
        .unwrap();
    }
    assert_eq!(changed, 320);
    assert_eq!(scalar(&state.conn, "SELECT count(*) FROM content_blobs"), 0);
    state
        .conn
        .execute_batch("PRAGMA wal_checkpoint(PASSIVE)")
        .unwrap();
    assert!(std::fs::metadata(&config.db_path).unwrap().len() < file_before);
    assert_eq!(
        state
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        scalar(&state.conn, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );
    let events = event::list_all_ordered(&state.conn).unwrap();
    assert!(events.iter().filter(|e|e.event.kind.primary_run_id()==Some("r")).all(|e|matches!(&e.event.kind,event_bus::EventKind::Tool(ToolEvent::ToolCompleted{output:Some(s),is_error:false,..}) if s==crate::thread_retention::tool_bodies::REMOVED)));
    // Close and reopen using the public DB reader, not a memory-only projection.
    drop(other);
    drop(state);
    let db = Database::open(&config).unwrap();
    assert_eq!(event::list_all_ordered(&db.conn).unwrap(), events);
}

#[test]
fn tool_body_retention_full_vacuum_recipe_on_incremental_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vacuum.db");
    let backup_path = dir.path().join("backup.db");
    let db = Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    db.conn.execute_batch("CREATE TABLE vacuum_fixture(id INTEGER PRIMARY KEY, body BLOB); INSERT INTO vacuum_fixture VALUES(1,'kept'); INSERT INTO vacuum_fixture VALUES(2,zeroblob(8388608)); DELETE FROM vacuum_fixture WHERE id=2; PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    assert_eq!(scalar(&db.conn, "PRAGMA auto_vacuum"), 2);
    assert!(scalar(&db.conn, "PRAGMA freelist_count") > 1024);
    let before = std::fs::metadata(&path).unwrap().len();
    assert!(!backup_path.exists());
    let backup = Connection::open(&backup_path).unwrap();
    // CLI .backup uses this same SQLite API. The sandbox has no sqlite3 CLI;
    // exercise the bundled library, not a different SQLite or VACUUM INTO.
    unsafe {
        // SAFETY: both owned connections outlive the backup handle; no statements
        // or other users borrow them until sqlite3_backup_finish releases it.
        let handle = rusqlite::ffi::sqlite3_backup_init(
            backup.handle(),
            c"main".as_ptr(),
            db.conn.handle(),
            c"main".as_ptr(),
        );
        assert!(!handle.is_null());
        let step = rusqlite::ffi::sqlite3_backup_step(handle, -1);
        let finish = rusqlite::ffi::sqlite3_backup_finish(handle);
        assert_eq!(step, rusqlite::ffi::SQLITE_DONE);
        assert_eq!(finish, rusqlite::ffi::SQLITE_OK);
    }
    assert_eq!(
        backup
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        std::fs::metadata(&backup_path).unwrap().len(),
        before,
        "backup preserves free pages"
    );
    drop(backup);
    db.conn.execute_batch("PRAGMA busy_timeout=0; PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    assert_eq!(
        db.conn
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(scalar(&db.conn, "PRAGMA auto_vacuum"), 2);
    assert_eq!(
        db.conn
            .query_row("SELECT body FROM vacuum_fixture WHERE id=1", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
        "kept"
    );
    let after = std::fs::metadata(&path).unwrap().len();
    assert!(after < before);
    println!(
        "INCREMENTAL full VACUUM recipe: before={before} after={after} backup={} integrity=ok auto_vacuum=2",
        std::fs::metadata(&backup_path).unwrap().len()
    );
}
