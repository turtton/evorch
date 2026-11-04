//! IO contracts of the content blob collector and schema: what one maintenance tick
//! reads and writes is bounded by constants, not by how large the database is.
//!
//! Every figure here is a count (WAL frames, SQLite VM steps), never a duration.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, ffi};
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
const MIB: u64 = 1024 * 1024;
const STEP_BYTES: u64 = 8 * MIB;
const STEP_CANDIDATES: i64 = 64;

/// Fixed overhead of one collection step in WAL frames, on top of one frame per page of
/// the 8 MiB it may delete: the queue and blob table leaves and interior pages the 64
/// deletes touch, the hash index, the freelist trunk pages, and page 1. It does not
/// depend on the number of blobs or of references. First measured on 4 KiB pages with
/// `normal_step_stays_inside_its_frame_budget`: 17 frames for 64 blobs of 128 KiB (the
/// registration of an oversized blob took 5); the constant leaves headroom for one more
/// level of b-tree and a further freelist trunk page.
const C_GC: u64 = 24;

/// Frames the migrations may write: v39's two new tables and hash index, v40's message
/// table, reference tables, indexes and header columns (all on empty tables here), the
/// schema page, page 1 and the pointer-map pages that account for the new pages (6 frames
/// with 10 rows and 12 with 100,000 when v39 alone was first measured; 19 and 34 with
/// v40 added: its DROP COLUMN rewrites the table and each ADD COLUMN the schema), and
/// v41's lane tables and indexes plus the rebuilt `threads` table (53 and 86 frames with v41
/// added; the threads copy of this empty table costs no rows), and v42's provenance header
/// columns and message table (64 and 99 frames with v42 added), and v43's event body format (two
/// columns, the reference table and three accounting triggers on an empty `events` table;
/// 77 and 117 frames with v43 added), whatever the number of `thread_runs` rows it leaves
/// alone.
const MIGRATION_FRAMES: u64 = 128;

const REF_TABLE: &str = "
    CREATE TABLE io_owner (id TEXT PRIMARY KEY);
    CREATE TABLE content_blob_refs_io (
        owner_id TEXT NOT NULL,
        blob_id INTEGER NOT NULL REFERENCES content_blobs(id),
        PRIMARY KEY (owner_id, blob_id),
        FOREIGN KEY (owner_id) REFERENCES io_owner(id) ON DELETE CASCADE
    ) WITHOUT ROWID;
    CREATE INDEX idx_content_blob_refs_io_blob ON content_blob_refs_io(blob_id);
    CREATE TRIGGER content_blob_refs_io_queue_gc AFTER DELETE ON content_blob_refs_io
    BEGIN
        INSERT OR IGNORE INTO content_blob_gc_queue(blob_id) VALUES (OLD.blob_id);
    END;";

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

fn page_size(conn: &Connection) -> u64 {
    conn.pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0).map(u64::from))
        .unwrap()
}

/// A migrated database with the reference table installed, opened by an observer that
/// has foreign keys on.
fn fresh(dir: &tempfile::TempDir) -> (PathBuf, Connection) {
    let path = dir.path().join("io.db");
    drop(Database::open(&config(&path)).unwrap());
    let observer = Connection::open(&path).unwrap();
    observer.pragma_update(None, "foreign_keys", true).unwrap();
    observer.execute_batch(REF_TABLE).unwrap();
    (path, observer)
}

/// `count` queued blobs of `bytes` stored bytes each that nothing refers to.
fn queue_blobs(conn: &Connection, count: i64, bytes: i64, tag: &str) {
    conn.execute_batch(&format!(
        "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {count} - 1)
         INSERT INTO content_blobs(hash, byte_len, codec, data)
         SELECT randomblob(32), {bytes}, 0, zeroblob({bytes}) FROM n;
         INSERT INTO content_blob_gc_queue(blob_id)
         SELECT id FROM content_blobs WHERE id NOT IN (SELECT blob_id FROM content_blob_refs_io)
             AND id NOT IN (SELECT blob_id FROM content_blob_gc_queue);
         -- {tag}"
    ))
    .unwrap();
}

/// WAL frames one explicit maintenance tick adds. A snapshot pinned by the observer keeps
/// the WAL from restarting, so growth is the frame volume.
fn tick_frames(path: &Path, observer: &Connection, handle: &storage::StorageHandle) -> u64 {
    let page = page_size(observer);
    observer.execute_batch("BEGIN").unwrap();
    let _: i64 = observer
        .query_row("SELECT COUNT(*) FROM content_blobs", [], |row| row.get(0))
        .unwrap();
    let before = wal_bytes(path);
    handle.checkpoint_now().unwrap();
    let frames = (wal_bytes(path) - before) / (page + PAGE_OVERHEAD);
    observer.execute_batch("COMMIT").unwrap();
    frames
}

fn blob_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM content_blobs", [], |row| row.get(0))
        .unwrap()
}

/// With nothing queued, a tick adds no WAL frame at all.
fn idle_tick_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (path, observer) = fresh(&dir);
    observer
        .execute_batch(
            "INSERT INTO content_blobs(hash, byte_len, codec, data) VALUES (randomblob(32), 3, 0, 'abc');
             INSERT INTO io_owner VALUES ('o');
             INSERT INTO content_blob_refs_io SELECT 'o', id FROM content_blobs;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
    let storage = Storage::open(config(&path)).unwrap();
    let handle = storage.handle();
    // Settle whatever the first tick does for the other maintenance tasks.
    handle.checkpoint_now().unwrap();
    observer.execute_batch("BEGIN").unwrap();
    let _: i64 = observer
        .query_row("SELECT COUNT(*) FROM content_blobs", [], |row| row.get(0))
        .unwrap();
    let idle = wal_bytes(&path);
    for _ in 0..8 {
        handle.checkpoint_now().unwrap();
    }
    assert_eq!(wal_bytes(&path), idle, "an idle tick adds no WAL frames");
    observer.execute_batch("COMMIT").unwrap();
    storage.close();
}

/// One tick with 64 queued blobs totalling 8 MiB, with no retention job anywhere in the
/// database, deletes them all inside `ceil(8 MiB / page) + C_GC` frames.
fn normal_step_stays_inside_its_frame_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (path, observer) = fresh(&dir);
    queue_blobs(
        &observer,
        STEP_CANDIDATES,
        (STEP_BYTES as i64) / STEP_CANDIDATES,
        "full",
    );
    observer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let page = page_size(&observer);
    let storage = Storage::open(config(&path)).unwrap();
    let handle = storage.handle();
    let frames = tick_frames(&path, &observer, &handle);
    let budget = STEP_BYTES.div_ceil(page) + C_GC;
    println!("collection step: {frames} frames (budget {budget}, page size {page})");
    assert_eq!(blob_count(&observer), 0, "the tick collected all 64 blobs");
    assert!(frames > 0, "the tick must have written");
    assert!(
        frames <= budget,
        "{frames} WAL frames exceed the budget {budget}"
    );
    // The delete frees overflow pages without writing them: far below one frame per page.
    assert!(frames < STEP_BYTES.div_ceil(page) / 4, "{frames} frames");
    storage.close();
}

/// A blob above the single-blob limit is registered for the exclusive phase, not deleted,
/// and registering costs a handful of frames.
fn oversized_blob_is_registered_not_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let (path, observer) = fresh(&dir);
    queue_blobs(&observer, 1, 4 * MIB as i64 + 1, "big");
    queue_blobs(&observer, 3, 100, "small");
    observer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let storage = Storage::open(config(&path)).unwrap();
    let handle = storage.handle();
    let frames = tick_frames(&path, &observer, &handle);
    println!("oversized registration step: {frames} frames");
    assert!(frames <= C_GC, "{frames} frames");
    assert_eq!(blob_count(&observer), 1, "only the oversized blob is left");
    let registered: i64 = observer
        .query_row(
            "SELECT COUNT(*) FROM reclaim_oversized WHERE family_key = 'content-blobs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(registered, 1);
    let queued: i64 = observer
        .query_row("SELECT COUNT(*) FROM content_blob_gc_queue", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(queued, 0);
    storage.close();
}

/// VM steps of the first tick with 64 small queued blobs and `refs` references to an
/// unrelated blob in the reference table.
fn tick_steps(refs: i64) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let (path, observer) = fresh(&dir);
    observer
        .execute_batch(&format!(
            "INSERT INTO content_blobs(hash, byte_len, codec, data) VALUES (randomblob(32), 1, 0, 'x');
             WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {refs} - 1)
             INSERT INTO io_owner SELECT printf('o%07d', x) FROM n;
             INSERT INTO content_blob_refs_io SELECT id, (SELECT MIN(id) FROM content_blobs)
                 FROM io_owner;"
        ))
        .unwrap();
    queue_blobs(&observer, STEP_CANDIDATES, 100, "steps");
    observer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let storage = Storage::open(config(&path)).unwrap();
    let handle = storage.handle();
    VM_STEPS.store(0, Ordering::Relaxed);
    handle.checkpoint_now().unwrap();
    let steps = VM_STEPS.load(Ordering::Relaxed);
    assert_eq!(blob_count(&observer), 1, "the 64 blobs were collected");
    storage.close();
    steps
}

/// The probes into the reference table go through its `blob_id` index, so the work of a
/// tick does not follow the size of the reference table.
fn tick_work_ignores_the_size_of_the_reference_table() {
    let small = tick_steps(100);
    let large = tick_steps(100_000);
    println!("collection tick: {small} VM steps with 100 refs, {large} with 100,000");
    assert!(small > 0, "the profiler must observe the tick");
    assert!(
        large <= small * 2,
        "1000x references must not scale the tick: {small} -> {large} VM steps"
    );
}

/// WAL frames of applying migrations v39 to the latest (v43) to a database at v38 holding `rows` rows.
fn migration_cost(rows: i64) -> (u64, u64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("io.db");
    drop(Database::open(&config(&path)).unwrap());
    let observer = Connection::open(&path).unwrap();
    observer
        .execute_batch(&format!(
            "{undo_v43}
             {undo_v42}
             {undo_v40}
             DROP TABLE content_blob_gc_queue; DROP TABLE content_blobs;
             PRAGMA user_version = 38;
             WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < {rows} - 1)
             INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth)
             SELECT printf('r-%07d', x), 't', 'r-0000000', 0 FROM n;
             PRAGMA wal_checkpoint(TRUNCATE);",
            undo_v43 = include_str!("support/undo_v43.sql"),
            undo_v42 = include_str!("support/undo_v42.sql"),
            undo_v40 = include_str!("support/undo_v40.sql"),
        ))
        .unwrap();
    let page = page_size(&observer);
    observer.execute_batch("BEGIN").unwrap();
    let _: i64 = observer
        .query_row("SELECT COUNT(*) FROM thread_runs", [], |row| row.get(0))
        .unwrap();
    let before = wal_bytes(&path);
    VM_STEPS.store(0, Ordering::Relaxed);
    drop(Database::open_unchecked(&config(&path)).unwrap());
    let frames = (wal_bytes(&path) - before) / (page + PAGE_OVERHEAD);
    let scanned = VM_STEPS.load(Ordering::Relaxed);
    println!("migrations v39 to v43 with {rows} rows: {frames} frames, {scanned} VM steps");
    observer.execute_batch("COMMIT").unwrap();
    let version: i64 = observer
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert!(version >= 39);
    let tables: i64 = observer
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('content_blobs', 'content_blob_gc_queue')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 2);
    (frames, scanned)
}

/// The migration creates two tables and converts nothing: its writes do not follow the
/// number of rows already in the database.
fn migration_does_not_scale_with_existing_rows() {
    let (small, small_steps) = migration_cost(10);
    let (large, large_steps) = migration_cost(100_000);
    assert!(small > 0 && small <= MIGRATION_FRAMES, "{small} frames");
    assert!(
        large <= MIGRATION_FRAMES,
        "{large} frames with 100,000 rows"
    );
    assert!(small_steps > 0, "the profiler must observe the migration");
    assert!(
        large_steps <= small_steps * 2,
        "10 -> 100,000 rows changed the VM steps from {small_steps} to {large_steps}: \
         the migration must not read existing rows"
    );
}

// SQLite's profiler is registered for every connection of the process and adds to one
// counter, so the measurements share a single test and run one after another.
#[test]
fn content_blob_collection_is_bounded() {
    // SAFETY: instrument only registers a callback; it opens/closes no DB.
    unsafe { rusqlite::auto_extension::register_auto_extension(instrument) }.unwrap();
    idle_tick_writes_nothing();
    normal_step_stays_inside_its_frame_budget();
    oversized_blob_is_registered_not_deleted();
    tick_work_ignores_the_size_of_the_reference_table();
    migration_does_not_scale_with_existing_rows();
}
