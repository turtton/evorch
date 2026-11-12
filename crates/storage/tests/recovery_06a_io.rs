//! v60 -> v61 migration I/O contract. Counts, never wall-clock durations.
//! One test process owns SQLite's profiler, as in content_blob_io_contracts.
use rusqlite::{Connection, ffi};
use std::{
    ffi::{c_char, c_int, c_uint, c_void},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use storage::{Database, StorageConfig};
static VM_STEPS: AtomicU64 = AtomicU64::new(0);
unsafe extern "C" fn profile(
    mask: c_uint,
    _: *mut c_void,
    statement: *mut c_void,
    _: *mut c_void,
) -> c_int {
    if mask == ffi::SQLITE_TRACE_PROFILE as c_uint {
        // SAFETY: PROFILE supplies a live statement; reset avoids double counting.
        let steps = unsafe {
            ffi::sqlite3_stmt_status(statement.cast(), ffi::SQLITE_STMTSTATUS_VM_STEP, 1)
        };
        VM_STEPS.fetch_add(steps as u64, Ordering::Relaxed);
    }
    0
}
unsafe extern "C" fn instrument(
    connection: *mut ffi::sqlite3,
    _: *mut *mut c_char,
    _: *const ffi::sqlite3_api_routines,
) -> c_int {
    // SAFETY: SQLite owns the live connection and keeps the static callback.
    unsafe {
        ffi::sqlite3_trace_v2(
            connection,
            ffi::SQLITE_TRACE_PROFILE as c_uint,
            Some(profile),
            std::ptr::null_mut(),
        )
    }
}
fn bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
fn migration_cost(rows: i64) -> (u64, u64) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("io.db"),
        ..Default::default()
    };
    drop(Database::open(&config).unwrap());
    let sql = Connection::open(&config.db_path).unwrap();
    // Populate before removing the graph schema, so its free root-page slots
    // are identical. The full-chain probe separately covers root-page relocation.
    sql.execute_batch(&format!("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x < {rows}-1) INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) SELECT printf('run-%07d',x),'t','run-0000000',0 FROM n; WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x < {rows}-1) INSERT INTO resume_intents(run_id,incarnation,payload) SELECT printf('run-%07d',x),'old','{{}}' FROM n;")).unwrap();
    sql.execute_batch(include_str!("../undo/undo_v61.sql"))
        .unwrap();
    sql.pragma_update(None, "user_version", 60).unwrap();
    sql.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); BEGIN")
        .unwrap();
    let _: i64 = sql
        .query_row("SELECT COUNT(*) FROM thread_runs", [], |r| r.get(0))
        .unwrap();
    let page: u64 = sql
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .unwrap();
    let wal = dir.path().join("io.db-wal");
    let before = bytes(&wal);
    VM_STEPS.store(0, Ordering::Relaxed);
    drop(Database::open(&config).unwrap());
    let steps = VM_STEPS.load(Ordering::Relaxed);
    let frames = (bytes(&wal) - before) / (page + 24);
    sql.execute_batch("COMMIT").unwrap();
    let untouched: i64 = sql.query_row("SELECT COUNT(*) FROM resume_intents WHERE revision=0 AND incarnation='old' AND payload='{}'",[],|r|r.get(0)).unwrap();
    assert_eq!(untouched, rows);
    (frames, steps)
}
#[test]
fn v60_to_v61_writes_schema_only_without_scanning_saved_intents() {
    // SAFETY: instrument only registers a callback; no DB is opened by it.
    unsafe { rusqlite::auto_extension::register_auto_extension(instrument) }.unwrap();
    let small = migration_cost(10);
    let large = migration_cost(100_000);
    println!("v61 frames/VM steps: 10 rows={small:?}, 100000 rows={large:?}");
    assert!(
        small.0 > 0 && small.1 > 0,
        "probe must observe migration work"
    );
    assert_eq!(
        large, small,
        "schema migration must not scan or rewrite saved rows"
    );
}
