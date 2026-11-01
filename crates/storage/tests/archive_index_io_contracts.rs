//! IO contracts of the archived-thread index exclusion (PR-2): the archive
//! mirror hook follows the size of the thread, not of the database, and one
//! purge or backfill transaction stays inside a WAL frame budget.

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

/// The archive hook follows the thread's runs, not the size of the database.
fn archive_hook_scales_with_the_threads_runs() {
    let (small, small_evicted) = archive_steps(1_000);
    let (large, large_evicted) = archive_steps(50_000);
    assert_eq!((small_evicted, large_evicted), (20, 20));
    assert!(small > 0, "the profiler must observe the hook");
    assert!(
        large <= small * 2,
        "50x unrelated runs must not scale the hook: {small} -> {large} VM steps"
    );
}

/// One purge transaction stays inside its WAL frame budget and an empty tick writes nothing.
fn purge_transaction_is_bounded_and_an_empty_tick_writes_nothing() {
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

const BIG_RUNS: i64 = 3_000;
const TICK_BUDGET: i64 = 1_000;

/// An archived thread with `BIG_RUNS` indexed runs (far above one tick's budget) next to
/// `unrelated` live threads of one run each, which sort before it. The rows are written
/// directly, as a database from before the mirror hook would hold them.
fn seed_backfill(conn: &Connection, unrelated: i64) {
    conn.execute_batch(&format!(
        "INSERT INTO threads(thread_id, project_root, root_run_id, archived, created_at_ns,
             updated_at_ns, source) VALUES ('t-big', '/p', 'r-000000', 1, 1, 1, 'host');
         WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {BIG_RUNS} - 1)
         INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth)
         SELECT printf('r-%06d', x), 't-big', 'r-000000', x > 0 FROM n;
         WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {unrelated} - 1)
         INSERT INTO threads(thread_id, project_root, root_run_id, archived, created_at_ns,
             updated_at_ns, source)
         SELECT printf('a-%07d', x), '/p', printf('u-%07d', x), 0, 1, 1, 'host' FROM n;
         WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {unrelated} - 1)
         INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth)
         SELECT printf('u-%07d', x), printf('a-%07d', x), printf('u-%07d', x), 0 FROM n;
         {INDEX_ROW}
         SELECT run_id, 'indexed', NULL, 1, 1, '', 1, 1, 1, 1, 1, 10 FROM thread_runs;
         UPDATE thread_index_state SET archive_backfill_done = 0;
         PRAGMA wal_checkpoint(TRUNCATE);"
    ))
    .unwrap();
}

/// (VM steps, WAL frames, runs evicted) of the first maintenance tick.
fn backfill_tick(unrelated: i64) -> (u64, u64, i64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("io.db");
    drop(Database::open(&config(&path)).unwrap());
    let observer = Connection::open(&path).unwrap();
    seed_backfill(&observer, unrelated);
    let page_size: u64 = observer
        .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0).map(u64::from))
        .unwrap();
    let storage = Storage::open(config(&path)).unwrap();
    let handle = storage.handle();
    observer.execute_batch("BEGIN").unwrap();
    let _: i64 = observer
        .query_row("SELECT COUNT(*) FROM thread_runs", [], |row| row.get(0))
        .unwrap();
    let before = wal_bytes(&path);
    VM_STEPS.store(0, Ordering::Relaxed);
    handle.checkpoint_now().unwrap();
    let steps = VM_STEPS.load(Ordering::Relaxed);
    let frames = (wal_bytes(&path) - before) / (page_size + PAGE_OVERHEAD);
    observer.execute_batch("COMMIT").unwrap();
    let evicted = observer
        .query_row(
            "SELECT COUNT(*) FROM thread_index_runs WHERE evicted_reason = 'archived'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    storage.close();
    (steps, frames, evicted)
}

/// One backfill tick reads and updates at most its budget of rows, inside a WAL frame
/// budget, however large the thread is and however many unrelated threads exist.
fn backfill_tick_is_bounded() {
    let (small, frames, evicted) = backfill_tick(100);
    let (large, ..) = backfill_tick(50_000);
    // The tick's own purge resets the charges of the (up to four) runs it just evicted.
    assert!(
        (TICK_BUDGET - 1..=TICK_BUDGET).contains(&evicted),
        "one tick evicts one budget of the {BIG_RUNS}-run thread, not {evicted}"
    );
    let budget = 6 * (evicted as u64 + 4) + 64;
    println!("backfill: {frames} frames (budget {budget}), {small} / {large} VM steps");
    assert!(
        frames > 0 && frames <= budget,
        "{frames} WAL frames exceed {budget}"
    );
    assert!(
        large <= small * 2,
        "500x unrelated threads must not scale the tick: {small} -> {large} VM steps"
    );
}

/// `registered` runs handed to the exclusive phase, and one ordinary run sorted after
/// all of them when `with_work`. VM steps of the first maintenance tick, and whether
/// the ordinary run was purged by it.
fn purge_tick(registered: i64, with_work: bool) -> (u64, bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("io.db");
    drop(Database::open(&config(&path)).unwrap());
    let observer = Connection::open(&path).unwrap();
    observer
        .execute_batch(&format!(
            "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {registered} - 1)
             {INDEX_ROW}
             SELECT printf('d-%07d', x), 'evicted', 'archived', 1, 1, '', 1, 1, 1, 1, 1, 10 FROM n;
             UPDATE thread_index_runs SET purge_deferred = 1;
             INSERT INTO reclaim_oversized(family_key, tbl, key, stored_bytes)
             SELECT 'archive-index', 'conversation_blocks', run_id, 4000000 FROM thread_index_runs;
             PRAGMA wal_checkpoint(TRUNCATE);"
        ))
        .unwrap();
    if with_work {
        observer
            .execute_batch(&format!(
                "{INDEX_ROW}
                 VALUES ('z-normal', 'evicted', 'archived', 1, 1, '', 1, 1, 1, 1, 1, 100);
                 INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index,
                     role, kind, truncated, hash, text)
                 VALUES ('z-normal', 'message', '0:0', 0, 0, 'user', 'text', 0, 'h', 'alpha');"
            ))
            .unwrap();
    }
    let storage = Storage::open(config(&path)).unwrap();
    let handle = storage.handle();
    VM_STEPS.store(0, Ordering::Relaxed);
    handle.checkpoint_now().unwrap();
    let steps = VM_STEPS.load(Ordering::Relaxed);
    let purged: bool = observer
        .query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM conversation_blocks WHERE run_id = 'z-normal')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    storage.close();
    (steps, purged)
}

/// The purge tick never reads the runs waiting for the exclusive phase, so its work
/// does not depend on how many there are, nor does the ordinary run behind them wait.
fn purge_tick_ignores_deferred_runs() {
    for with_work in [true, false] {
        let (small, small_purged) = purge_tick(1_000, with_work);
        let (large, large_purged) = purge_tick(50_000, with_work);
        // With work behind the deferred runs, the tick still purges it.
        assert!(small_purged && large_purged);
        assert!(small > 0);
        assert!(
            large <= small * 2,
            "50x deferred runs must not scale the tick (work: {with_work}): {small} -> {large} VM steps"
        );
    }
}

// SQLite's profiler is registered for every connection of the process and adds to
// one counter, so the measurements share a single test and run one after another.
#[test]
fn archived_thread_index_work_is_bounded() {
    // SAFETY: instrument only registers a callback; it opens/closes no DB.
    unsafe { rusqlite::auto_extension::register_auto_extension(instrument) }.unwrap();
    archive_hook_scales_with_the_threads_runs();
    purge_transaction_is_bounded_and_an_empty_tick_writes_nothing();
    backfill_tick_is_bounded();
    purge_tick_ignores_deferred_runs();
}
