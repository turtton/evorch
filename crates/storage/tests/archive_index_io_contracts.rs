//! IO contracts of the archived-thread index exclusion (PR-2): the archive
//! mirror hook follows the size of the thread, not of the database, and one
//! purge transaction stays inside a WAL frame budget.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, ffi};
use storage::threads::ThreadUpsert;
use storage::{Database, Storage, StorageConfig};

static VM_STEPS: AtomicU64 = AtomicU64::new(0);

// SQLite invokes PROFILE while the statement pointer is valid; the callback only
// touches a static atomic and never unwinds or re-enters SQLite.
unsafe extern "C" fn profile(
    mask: c_uint,
    _: *mut c_void,
    statement: *mut c_void,
    _: *mut c_void,
) -> c_int {
    if mask == ffi::SQLITE_TRACE_PROFILE as c_uint {
        // SAFETY: PROFILE supplies a live sqlite3_stmt; reset avoids double counting.
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
    // SAFETY: SQLite owns this live connection; registration takes no ownership.
    unsafe {
        ffi::sqlite3_trace_v2(
            connection,
            ffi::SQLITE_TRACE_PROFILE as c_uint,
            Some(profile),
            std::ptr::null_mut(),
        )
    }
}

const PAGE_OVERHEAD: u64 = 24;

fn config(path: &Path) -> StorageConfig {
    StorageConfig {
        db_path: path.to_path_buf(),
        // Maintenance is triggered explicitly; elapsed time must not change work.
        checkpoint_interval: std::time::Duration::from_secs(3_600),
        flush_interval: std::time::Duration::from_secs(3_600),
        ..StorageConfig::default()
    }
}

fn wal_bytes(path: &Path) -> u64 {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(PathBuf::from(wal)).map_or(0, |meta| meta.len())
}

const INDEX_ROW: &str = "INSERT INTO thread_index_runs(run_id, state, evicted_reason, \
     source_revision, snapshot_revision, source_context_sha256, extractor_version, \
     accounting_version, source_updated_at_ns, retention_at_ns, work_generation, charged_bytes)";

/// A live thread `A` with `runs` indexed runs, next to `others` runs of another thread.
fn seed(conn: &Connection, runs: i64, others: i64) {
    conn.execute_batch(&format!(
        "INSERT INTO threads(thread_id, project_root, root_run_id, archived, created_at_ns,
             updated_at_ns, source) VALUES ('A', '/p', 'a-000000', 0, 1, 1, 'host'),
             ('B', '/p', 'b-0000000', 0, 1, 1, 'host');
         WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {runs} - 1)
         INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth)
         SELECT printf('a-%06d', x), 'A', 'a-000000', x > 0 FROM n;
         WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {runs} - 1)
         {INDEX_ROW}
         SELECT printf('a-%06d', x), 'indexed', NULL, 1, 1, '', 1, 1, 1, 1, 1, 10 FROM n;
         WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {others} - 1)
         INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth)
         SELECT printf('b-%07d', x), 'B', 'b-0000000', x > 0 FROM n;
         WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {others} - 1)
         {INDEX_ROW}
         SELECT printf('b-%07d', x), 'indexed', NULL, 1, 1, '', 1, 1, 1, 1, 1, 10 FROM n;"
    ))
    .unwrap();
}

fn host(archived: bool) -> ThreadUpsert {
    ThreadUpsert {
        thread_id: "A".into(),
        project_root: Some("/p".into()),
        root_run_id: Some("a-000000".into()),
        archived: Some(archived),
        created_at_ns: 1,
        updated_at_ns: 2,
        ..ThreadUpsert::default()
    }
}

/// VM steps of one archive mirror write, and how many runs it evicted.
fn archive_steps(others: i64) -> (u64, i64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("io.db");
    drop(Database::open(&config(&path)).unwrap());
    let conn = Connection::open(&path).unwrap();
    seed(&conn, 20, others);
    let storage = Storage::open(config(&path)).unwrap();
    let handle = storage.handle();
    handle.upsert_threads_host(&[host(false)]).unwrap();
    VM_STEPS.store(0, Ordering::Relaxed);
    handle.upsert_threads_host(&[host(true)]).unwrap();
    let steps = VM_STEPS.load(Ordering::Relaxed);
    let evicted = conn
        .query_row(
            "SELECT COUNT(*) FROM thread_index_runs WHERE evicted_reason = 'archived'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    storage.close();
    (steps, evicted)
}

#[test]
fn the_archive_hook_scales_with_the_threads_runs_not_with_the_database() {
    // SAFETY: instrument only registers a callback; it opens/closes no DB.
    unsafe { rusqlite::auto_extension::register_auto_extension(instrument) }.unwrap();
    let (small, small_evicted) = archive_steps(1_000);
    let (large, large_evicted) = archive_steps(50_000);
    assert_eq!((small_evicted, large_evicted), (20, 20));
    assert!(small > 0, "the profiler must observe the hook");
    assert!(
        large <= small * 2,
        "50x unrelated runs must not scale the hook: {small} -> {large} VM steps"
    );
}

#[test]
fn one_purge_transaction_stays_inside_its_wal_frame_budget_and_an_empty_tick_writes_nothing() {
    const ROWS: u64 = 200;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("io.db");
    drop(Database::open(&config(&path)).unwrap());
    let observer = Connection::open(&path).unwrap();
    seed(&observer, 1, 1);
    observer
        .execute_batch(&format!(
            "UPDATE thread_index_runs SET state = 'evicted', evicted_reason = 'archived',
                 charged_bytes = 1000000 WHERE run_id = 'a-000000';
             WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {ROWS} - 1)
             INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index,
                 role, kind, truncated, hash, text)
             SELECT 'a-000000', 'message', x || ':0', x, 0, 'user', 'text', 0, printf('h%d', x),
                 printf('block %d ', x) || hex(randomblob(150)) FROM n;
             PRAGMA wal_checkpoint(TRUNCATE);"
        ))
        .unwrap();
    let text_bytes: u64 = observer
        .query_row(
            "SELECT SUM(length(CAST(text AS BLOB))) FROM conversation_blocks",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let page_size: u64 = observer
        .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0).map(u64::from))
        .unwrap();
    let storage = Storage::open(config(&path)).unwrap();
    let handle = storage.handle();
    // A pinned snapshot keeps the WAL from restarting, so growth is the frame volume.
    observer.execute_batch("BEGIN").unwrap();
    let _: i64 = observer
        .query_row("SELECT COUNT(*) FROM conversation_blocks", [], |row| {
            row.get(0)
        })
        .unwrap();
    let before = wal_bytes(&path);
    handle.checkpoint_now().unwrap();
    let frames = (wal_bytes(&path) - before) / (page_size + PAGE_OVERHEAD);
    let budget = 6 * ROWS + 2 * text_bytes.div_ceil(page_size) + 64;
    let remaining: i64 = observer
        .query_row("SELECT COUNT(*) FROM conversation_blocks", [], |row| {
            row.get(0)
        })
        .unwrap();
    println!(
        "purge frames: {frames} (budget {budget}), blocks left in the pinned view: {remaining}"
    );
    assert!(frames > 0, "the tick must have purged");
    assert!(
        frames <= budget,
        "{frames} WAL frames exceed the budget {budget}"
    );
    observer.execute_batch("COMMIT").unwrap();
    let left: i64 = observer
        .query_row("SELECT COUNT(*) FROM conversation_blocks", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(left, 0);
    let charged: i64 = observer
        .query_row(
            "SELECT charged_bytes FROM thread_index_runs WHERE run_id = 'a-000000'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(charged, 0);

    // With nothing left the tick only reads.
    observer.execute_batch("BEGIN").unwrap();
    let _: i64 = observer
        .query_row("SELECT COUNT(*) FROM thread_runs", [], |row| row.get(0))
        .unwrap();
    let idle = wal_bytes(&path);
    for _ in 0..8 {
        handle.checkpoint_now().unwrap();
    }
    assert_eq!(wal_bytes(&path), idle, "an empty tick adds no WAL frames");
    observer.execute_batch("COMMIT").unwrap();
    storage.close();
}
