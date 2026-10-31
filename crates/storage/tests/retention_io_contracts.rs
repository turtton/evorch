//! Deterministic I/O contracts of the retention foundation: bounded WAL frames and
//! SQLite VM steps, never wall-clock time. SQLite's profiler callback feeds a
//! process-global counter, so everything runs in one test of this binary.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, ffi};
use storage::thread_retention::{
    BindingInput, ExploreBudget, FamilyState, FinalizeBudget, FinalizeOutcome, ReclaimBudget,
    RetentionInput, SidebarThread, ThreadRetentionPolicy,
};
use storage::{Database, Storage, StorageConfig};

static VM_STEPS: AtomicU64 = AtomicU64::new(0);
static FULL_SCANS: AtomicU64 = AtomicU64::new(0);
static SORTS: AtomicU64 = AtomicU64::new(0);
static AUTO_INDEXES: AtomicU64 = AtomicU64::new(0);

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
        // SAFETY: as above; each counter is read and reset once per statement.
        let (full_scans, sorts, auto_indexes) = unsafe {
            (
                ffi::sqlite3_stmt_status(statement.cast(), ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP, 1),
                ffi::sqlite3_stmt_status(statement.cast(), ffi::SQLITE_STMTSTATUS_SORT, 1),
                ffi::sqlite3_stmt_status(statement.cast(), ffi::SQLITE_STMTSTATUS_AUTOINDEX, 1),
            )
        };
        FULL_SCANS.fetch_add(full_scans as u64, Ordering::Relaxed);
        SORTS.fetch_add(sorts as u64, Ordering::Relaxed);
        AUTO_INDEXES.fetch_add(auto_indexes as u64, Ordering::Relaxed);
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
const NOW_NS: i64 = 100 * 86_400 * 1_000_000_000;

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

struct Db {
    _dir: tempfile::TempDir,
    path: PathBuf,
    conn: Connection,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("io.db");
        drop(Database::open(&config(&path)).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        Self {
            _dir: dir,
            path,
            conn,
        }
    }

    fn sql(&self, sql: &str) {
        self.conn.execute_batch(sql).unwrap();
    }

    fn int(&self, sql: &str) -> i64 {
        self.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn page_size(&self) -> u64 {
        self.int("PRAGMA page_size") as u64
    }

    /// Bulk-insert `count` runs (cheap rows) that nothing refers to.
    fn unrelated_runs(&self, count: i64) {
        self.sql(&format!(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < {count})
             INSERT INTO run_contexts(run_id, role, name, config_json, messages_json, checkpoints_json,
                 terminal_phase, restorable, updated_at_ns)
             SELECT printf('other-%07d', x), 'Worker', 'other', '{{}}', '[]', '[]', 'Done', 1, 1 FROM n;"
        ));
    }

    /// Thread `A` with a root run `ra` and `children` delegated runs.
    fn family(&self, children: i64) {
        self.sql(
            "INSERT INTO threads(thread_id, project_root, root_run_id, archived, created_at_ns,
                 updated_at_ns, source) VALUES ('A', '/p', 'ra', 1, 1, 1, 'host');
             INSERT INTO run_contexts(run_id, role, name, config_json, messages_json, checkpoints_json,
                 terminal_phase, restorable, updated_at_ns)
             VALUES ('ra', 'Worker', 'chat:Worker:A', '{}', '[]', '[]', 'Done', 1, 1);
             INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth) VALUES ('ra', 'A', 'ra', 0);",
        );
        self.sql(&format!(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < {children})
             INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json,
                 checkpoints_json, terminal_phase, restorable, updated_at_ns)
             SELECT printf('child-%05d', x), 'Worker', 'child', 'ra', '{{}}', '[]', '[]', 'Done', 1, 1 FROM n;"
        ));
    }
}

fn input() -> RetentionInput {
    RetentionInput {
        threads: vec![SidebarThread {
            thread_id: "A".into(),
            archived: true,
            archived_at_s: None,
            pinned: false,
            updated_at_s: 1,
            created_at_s: 1,
            archive_revision: None,
            pin_revision: None,
        }],
        sidebar_path: "/s".into(),
        ownership_root: "/r".into(),
        now_ns: NOW_NS,
    }
}

fn steps_of(action: impl FnOnce()) -> u64 {
    VM_STEPS.store(0, Ordering::Relaxed);
    action();
    VM_STEPS.load(Ordering::Relaxed)
}

/// VM steps, full-table-scan steps, sorts and automatic indexes of `action`.
fn scans_of(action: impl FnOnce()) -> (u64, u64, u64, u64) {
    for counter in [&VM_STEPS, &FULL_SCANS, &SORTS, &AUTO_INDEXES] {
        counter.store(0, Ordering::Relaxed);
    }
    action();
    (
        VM_STEPS.load(Ordering::Relaxed),
        FULL_SCANS.load(Ordering::Relaxed),
        SORTS.load(Ordering::Relaxed),
        AUTO_INDEXES.load(Ordering::Relaxed),
    )
}

fn explore_all(database: &Database, budget: u64) -> u64 {
    let policy = ThreadRetentionPolicy::default();
    let mut total = 0;
    loop {
        let mut settled = false;
        total += steps_of(|| {
            let states = database
                .retention_explore(&input(), &policy, ExploreBudget { max_edges: budget })
                .unwrap();
            settled = states
                .iter()
                .all(|(_, state)| !matches!(state, FamilyState::Exploring { .. }));
        });
        if settled {
            return total;
        }
    }
}

/// Confirms the family `A` so that reclaim jobs exist.
fn confirm(db: &Db) {
    let database = Database::open(&config(&db.path)).unwrap();
    explore_all(&database, 1_000_000);
    let outcome = database
        .retention_finalize(
            "A",
            &input(),
            &ThreadRetentionPolicy::default(),
            &std::collections::BTreeMap::new(),
            &BindingInput {
                ownership_root: "/r".into(),
                sidebar_path: "/s".into(),
            },
            FinalizeBudget {
                max_edges: 1_000_000,
            },
        )
        .unwrap();
    assert!(
        matches!(outcome, FinalizeOutcome::Confirmed(_)),
        "{outcome:?}"
    );
}

/// WAL frames and VM steps one reclaim transaction costs, with a reader pinning
/// the log so that frames are counted rather than recycled.
fn one_step(db: &Db, storage: &Storage) -> (u64, u64, storage::thread_retention::ReclaimProgress) {
    let observer = Connection::open(&db.path).unwrap();
    observer.execute_batch("BEGIN").unwrap();
    let _: i64 = observer
        .query_row("SELECT COUNT(*) FROM reclaim_jobs", [], |row| row.get(0))
        .unwrap();
    let before = wal_bytes(&db.path);
    let mut progress = None;
    let steps = steps_of(|| {
        progress = Some(
            storage
                .handle()
                .reclaim_step(ReclaimBudget {
                    max_transactions: 1,
                })
                .unwrap(),
        );
    });
    let after = wal_bytes(&db.path);
    observer.execute_batch("COMMIT").unwrap();
    assert_eq!((after - before) % (db.page_size() + PAGE_OVERHEAD), 0);
    (
        (after - before) / (db.page_size() + PAGE_OVERHEAD),
        steps,
        progress.unwrap(),
    )
}

/// Like [`one_step`], reporting scan counters instead of a single step count.
fn one_step_scans(
    storage: &Storage,
) -> (
    (u64, u64, u64, u64),
    storage::thread_retention::ReclaimProgress,
) {
    let mut progress = None;
    let counters = scans_of(|| {
        progress = Some(
            storage
                .handle()
                .reclaim_step(ReclaimBudget {
                    max_transactions: 1,
                })
                .unwrap(),
        );
    });
    (counters, progress.unwrap())
}

fn set_cursor(db: &Db, cursor: &str) {
    db.sql(&format!("UPDATE reclaim_jobs SET cursor_json = '{cursor}'"));
    db.sql("PRAGMA wal_checkpoint(TRUNCATE)");
}

fn pages(bytes: u64, page: u64) -> u64 {
    bytes.div_ceil(page)
}

#[test]
fn retention_work_is_bounded_in_wal_frames_and_vm_steps() {
    // SAFETY: instrument only registers a callback; it opens/closes no DB.
    unsafe { rusqlite::auto_extension::register_auto_extension(instrument) }.unwrap();

    // Candidate listing follows the archived count, not the number of threads.
    let schedule_steps = |threads: i64| {
        let db = Db::new();
        db.sql(&format!(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < {threads})
             INSERT INTO threads(thread_id, project_root, archived, created_at_ns, updated_at_ns, source)
             SELECT printf('t-%05d', x), '/p', x <= 16, 1, 1, 'host' FROM n;"
        ));
        let database = Database::open(&config(&db.path)).unwrap();
        let mut sidebar = input();
        sidebar.threads = (1..=threads)
            .map(|x| SidebarThread {
                thread_id: format!("t-{x:05}"),
                archived: x <= 16,
                archived_at_s: Some(1),
                pinned: false,
                updated_at_s: 1,
                created_at_s: 1,
                archive_revision: None,
                pin_revision: None,
            })
            .collect();
        steps_of(|| {
            let statuses = database
                .retention_schedule(
                    &sidebar,
                    &ThreadRetentionPolicy::default(),
                    &std::collections::BTreeMap::new(),
                )
                .unwrap();
            assert_eq!(statuses.len() as i64, threads);
        })
    };
    let small = schedule_steps(128);
    let large = schedule_steps(8_192);
    assert!(
        large <= small * 2,
        "schedule: {small} steps for 128 threads, {large} for 8192"
    );

    // Exploring a family reads rows in proportion to the family, not the database.
    let explore_steps = |unrelated: i64| {
        let db = Db::new();
        db.family(20);
        db.unrelated_runs(unrelated);
        explore_all(&Database::open(&config(&db.path)).unwrap(), 1_000_000)
    };
    let few = explore_steps(1_000);
    let many = explore_steps(50_000);
    assert!(
        many <= few + few / 4,
        "explore: {few} steps with 1k unrelated runs, {many} with 50k"
    );
    assert!(
        few <= 5_000 * 22,
        "explore reads {few} steps for 22 members"
    );

    // The fixed-point check is bounded by the family size and fails past its budget.
    let finalize_steps = |unrelated: i64| {
        let db = Db::new();
        db.family(20);
        db.unrelated_runs(unrelated);
        let database = Database::open(&config(&db.path)).unwrap();
        explore_all(&database, 1_000_000);
        let binding = BindingInput {
            ownership_root: "/r".into(),
            sidebar_path: "/s".into(),
        };
        let policy = ThreadRetentionPolicy::default();
        let none = std::collections::BTreeMap::new();
        let tiny = database
            .retention_finalize(
                "A",
                &input(),
                &policy,
                &none,
                &binding,
                FinalizeBudget { max_edges: 10 },
            )
            .unwrap();
        assert!(
            matches!(tiny, FinalizeOutcome::OverBudget { .. }),
            "{tiny:?}"
        );
        steps_of(|| {
            let outcome = database
                .retention_finalize(
                    "A",
                    &input(),
                    &policy,
                    &none,
                    &binding,
                    FinalizeBudget {
                        max_edges: 1_000_000,
                    },
                )
                .unwrap();
            assert!(
                matches!(outcome, FinalizeOutcome::Confirmed(_)),
                "{outcome:?}"
            );
        })
    };
    let few = finalize_steps(1_000);
    let many = finalize_steps(50_000);
    assert!(
        many <= few + few / 4,
        "finalize: {few} steps with 1k unrelated runs, {many} with 50k"
    );
    assert!(
        few <= 5_000 * 22,
        "finalize takes {few} steps for 22 members"
    );

    // An idle reclaim step writes nothing; a step over events is bounded by what it deletes.
    let db = Db::new();
    db.family(1);
    db.sql("INSERT INTO sessions(id, status, created_at_ns, updated_at_ns) VALUES ('s', 'running', 0, 0);");
    db.sql(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 700)
         INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind, run_id)
         SELECT 's', 1, 1, 1, 'Message', printf('%02000d', x), 'MessageCompleted', 'ra' FROM n;",
    );
    db.sql("UPDATE sessions SET total_event_bytes = (SELECT SUM(OCTET_LENGTH(payload)) FROM events) WHERE id = 's'");
    let storage = Storage::open(config(&db.path)).unwrap();
    let (frames, _, idle) = one_step(&db, &storage);
    assert_eq!((frames, idle.transactions), (0, 0), "no job: no write");
    storage.close();
    confirm(&db);
    let storage = Storage::open(config(&db.path)).unwrap();
    let page = db.page_size();
    // Skip the (empty) phases before events: the first real transaction deletes events.
    let (frames, _, progress) = one_step(&db, &storage);
    assert_eq!(progress.rows, 500, "{progress:?}");
    let bound = 2 * pages(progress.stored_bytes, page) + 4 * progress.rows + 64;
    assert!(
        frames <= bound,
        "events step: {frames} frames for {} bytes / {} rows (bound {bound})",
        progress.stored_bytes,
        progress.rows
    );
    storage.close();

    // Todo snapshots and queue ledger rows are scanned a bounded number of rows per
    // step, however long the history is; the cursor advances although nothing matches,
    // and no scan, sort or automatic index grows with the history.
    let todo_step = |rows: i64| {
        let db = Db::new();
        db.family(1);
        db.sql(&format!(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < {rows})
             INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind)
             SELECT 1, 1, 1, 'Orchestrator',
                    json_object('kind', 'Orchestrator', 'payload', json_object('kind', 'ThreadTodoUpdated',
                      'payload', json_object('snapshot', json_object('thread_id', 'other')))),
                    'ThreadTodoUpdated' FROM n;"
        ));
        confirm(&db);
        set_cursor(&db, r#"{"phase":"events_thread"}"#);
        let storage = Storage::open(config(&db.path)).unwrap();
        let (counters, progress) = one_step_scans(&storage);
        storage.close();
        assert_eq!(progress.rows, 0);
        let cursor: String = db
            .conn
            .query_row("SELECT cursor_json FROM reclaim_jobs", [], |r| r.get(0))
            .unwrap();
        assert!(cursor.contains("\"scan_after\""), "{cursor}");
        assert!(
            !cursor.contains("\"scan_after\":0"),
            "the cursor must advance: {cursor}"
        );
        counters
    };
    let short = todo_step(3_000);
    let long = todo_step(12_000);
    assert!(
        long.0 <= short.0 + short.0 / 10,
        "todo scan: {short:?} over 3k rows, {long:?} over 12k"
    );
    assert!(
        short.0 <= 2_000 * 100,
        "todo scan takes {} steps for 2000 rows",
        short.0
    );
    assert!(
        long.1 <= short.1,
        "full scans grew with the history: {short:?} -> {long:?}"
    );
    assert_eq!(
        (short.2, short.3, long.2, long.3),
        (0, 0, 0, 0),
        "no sort or automatic index"
    );

    let ledger_step = |rows: i64| {
        let db = Db::new();
        db.family(1);
        db.sql(&format!(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < {rows})
             INSERT INTO task_queue_ledger(operation, payload)
             SELECT 'enqueue', json_object('id', 'other-' || x) FROM n;"
        ));
        confirm(&db);
        set_cursor(&db, r#"{"phase":"tasks"}"#);
        let storage = Storage::open(config(&db.path)).unwrap();
        let (counters, progress) = one_step_scans(&storage);
        storage.close();
        assert_eq!(progress.rows, 0);
        let cursor: String = db
            .conn
            .query_row("SELECT cursor_json FROM reclaim_jobs", [], |r| r.get(0))
            .unwrap();
        assert!(
            !cursor.contains("\"scan_after\":0"),
            "the cursor must advance: {cursor}"
        );
        counters
    };
    let short = ledger_step(3_000);
    let long = ledger_step(12_000);
    assert!(
        long.0 <= short.0 + short.0 / 10,
        "ledger scan: {short:?} over 3k rows, {long:?} over 12k"
    );
    assert!(
        long.1 <= short.1,
        "full scans grew with the ledger: {short:?} -> {long:?}"
    );
    assert_eq!(
        (short.2, short.3, long.2, long.3),
        (0, 0, 0, 0),
        "no sort or automatic index"
    );

    // The scan steps probe the family's tombstones row by row: a family with many
    // threads or tasks costs the same per step as a small one.
    let scan_with_tombstones = |tombstones: i64, phase: &str| {
        let db = Db::new();
        db.family(1);
        db.sql(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 2000)
             INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind)
             SELECT 1, 1, 1, 'Orchestrator',
                    json_object('kind', 'Orchestrator', 'payload', json_object('kind', 'ThreadTodoUpdated',
                      'payload', json_object('snapshot', json_object('thread_id', 'other')))),
                    'ThreadTodoUpdated' FROM n;
             WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 2000)
             INSERT INTO task_queue_ledger(operation, payload)
             SELECT 'enqueue', json_object('id', 'other-' || x) FROM n;",
        );
        confirm(&db);
        db.sql(&format!(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < {tombstones})
             INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns)
             SELECT 'dead-' || x, 'A', 1 FROM n;
             WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < {tombstones})
             INSERT INTO task_tombstones(task_id, family_key) SELECT 'dead-task-' || x, 'A' FROM n;"
        ));
        set_cursor(&db, &format!(r#"{{"phase":"{phase}"}}"#));
        let storage = Storage::open(config(&db.path)).unwrap();
        let (counters, _) = one_step_scans(&storage);
        storage.close();
        counters
    };
    for phase in ["events_thread", "tasks"] {
        let few = scan_with_tombstones(10, phase);
        let many = scan_with_tombstones(20_000, phase);
        assert!(
            many.0 <= few.0 + few.0 / 10,
            "{phase}: {few:?} with 10 tombstones, {many:?} with 20000"
        );
        assert_eq!(
            (few.2, few.3, many.2, many.3),
            (0, 0, 0, 0),
            "{phase}: sort or automatic index"
        );
    }

    // Rows at the step budget are deleted in one transaction; one byte above, they wait.
    for (event_bytes, expect_deleted) in [(4_194_304_u64, true), (4_194_305, false)] {
        let db = Db::new();
        db.family(1);
        db.sql("INSERT INTO sessions(id, status, created_at_ns, updated_at_ns) VALUES ('s', 'running', 0, 0);");
        db.sql(&format!(
            "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind, run_id)
             VALUES ('s', 1, 1, 1, 'Message', printf('%0{event_bytes}d', 1), 'MessageCompleted', 'ra');"
        ));
        confirm(&db);
        set_cursor(&db, r#"{"phase":"events_run","run":"ra"}"#);
        let storage = Storage::open(config(&db.path)).unwrap();
        let (frames, _, progress) = one_step(&db, &storage);
        storage.close();
        let page = db.page_size();
        if expect_deleted {
            assert_eq!(progress.rows, 1);
            assert!(
                frames <= 2 * pages(event_bytes, page) + 4 + 64,
                "{frames} frames for {event_bytes} bytes"
            );
        } else {
            assert_eq!(
                progress.rows, 0,
                "a row above the budget is not deleted by a bounded step"
            );
            assert!(
                frames <= 64,
                "registering a large row costs {frames} frames"
            );
            assert_eq!(
                db.int("SELECT COUNT(*) FROM reclaim_oversized WHERE tbl = 'events'"),
                1
            );
        }
    }
    for (stored_bytes, expect_deleted) in [(4_194_304_u64, true), (4_194_305, false)] {
        let db = Db::new();
        db.family(1);
        db.sql(&format!(
            "UPDATE run_contexts SET messages_json = zeroblob({}) WHERE run_id = 'ra'",
            stored_bytes - 2
        ));
        confirm(&db);
        set_cursor(&db, r#"{"phase":"run_contexts","run":"ra"}"#);
        let storage = Storage::open(config(&db.path)).unwrap();
        let (_, _, progress) = one_step(&db, &storage);
        storage.close();
        assert_eq!(progress.rows, u64::from(expect_deleted), "{stored_bytes}");
        assert_eq!(
            db.int("SELECT COUNT(*) FROM reclaim_oversized WHERE tbl = 'run_contexts'"),
            i64::from(!expect_deleted)
        );
    }

    // One run context is one transaction, bounded by its stored size.
    let db = Db::new();
    db.family(1);
    db.sql("UPDATE run_contexts SET messages_json = zeroblob(1500000) WHERE run_id = 'ra'");
    confirm(&db);
    set_cursor(&db, r#"{"phase":"run_contexts","run":"ra"}"#);
    let storage = Storage::open(config(&db.path)).unwrap();
    let (frames, _, progress) = one_step(&db, &storage);
    storage.close();
    assert_eq!(progress.rows, 1);
    let bound = 2 * pages(progress.stored_bytes, db.page_size()) + 64;
    assert!(
        frames <= bound,
        "run context: {frames} frames for {} bytes (bound {bound})",
        progress.stored_bytes
    );

    // Index blocks are purged in bounded transactions.
    let db = Db::new();
    db.family(1);
    db.sql(
        "INSERT INTO thread_index_runs(run_id, state, source_revision, snapshot_revision, source_context_sha256,
             extractor_version, accounting_version, source_updated_at_ns, retention_at_ns)
         VALUES ('ra', 'indexed', 1, 1, 'x', 1, 0, 1, 1);
         WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 400)
         INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index, role, kind,
             truncated, hash, text)
         SELECT 'ra', 'message', x || ':0', x, 0, 'user', 'text', 0, 'h', printf('%0500d', x) FROM n;",
    );
    confirm(&db);
    set_cursor(&db, r#"{"phase":"index","run":"ra"}"#);
    let storage = Storage::open(config(&db.path)).unwrap();
    let (frames, _, progress) = one_step(&db, &storage);
    storage.close();
    assert_eq!(progress.rows, 200, "{progress:?}");
    let bound = 6 * progress.rows + 2 * pages(progress.stored_bytes, db.page_size()) + 64;
    assert!(
        frames <= bound,
        "index purge: {frames} frames for {} rows (bound {bound})",
        progress.rows
    );

    // A large index block is registered by its own size: the work does not follow how
    // many small blocks the run has, in the bounded phase or the exclusive one.
    let index_work = |small_blocks: i64| {
        let db = Db::new();
        db.family(1);
        db.sql(&format!(
            "INSERT INTO thread_index_runs(run_id, state, source_revision, snapshot_revision,
                 source_context_sha256, extractor_version, accounting_version, source_updated_at_ns,
                 retention_at_ns)
             VALUES ('ra', 'indexed', 1, 1, 'x', 1, 0, 1, 1);
             INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index, role,
                 kind, truncated, hash, text)
             VALUES ('ra', 'message', '0:0', 0, 0, 'user', 'text', 0, 'h', printf('%03145728d', 1));
             WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < {small_blocks})
             INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index, role,
                 kind, truncated, hash, text)
             SELECT 'ra', 'message', x || ':0', x, 0, 'user', 'text', 0, 'h', 'small' FROM n;"
        ));
        confirm(&db);
        set_cursor(&db, r#"{"phase":"index","run":"ra"}"#);
        let storage = Storage::open(config(&db.path)).unwrap();
        let (bounded, _) = one_step_scans(&storage);
        storage.close();
        // The huge block comes first, so the step registers it and deletes no block.
        assert_eq!(db.int("SELECT COUNT(*) FROM reclaim_oversized"), 1);
        assert_eq!(
            db.int("SELECT COUNT(*) FROM conversation_blocks WHERE run_id = 'ra'"),
            small_blocks + 1
        );
        assert!(db.int("SELECT stored_bytes FROM reclaim_oversized") >= 3_145_728);
        let database = Database::open(&config(&db.path)).unwrap();
        // The first exclusive call takes the huge block alone; the next takes a batch.
        let alone = database.reclaim_oversized_step(0).unwrap();
        assert_eq!(alone.rows, 1, "{alone:?}");
        let mut batch = None;
        let exclusive = steps_of(|| batch = Some(database.reclaim_oversized_step(0).unwrap()));
        let batch = batch.unwrap();
        assert_eq!(batch.rows, 200, "{batch:?}");
        (bounded, exclusive)
    };
    let (short, short_exclusive) = index_work(400);
    let (long, long_exclusive) = index_work(6_000);
    assert!(
        long.0 <= short.0 + short.0 / 10,
        "registering a large block: {short:?} over 400 small blocks, {long:?} over 6000"
    );
    assert_eq!(
        (short.2, short.3, long.2, long.3),
        (0, 0, 0, 0),
        "no sort or automatic index"
    );
    assert!(
        long_exclusive <= short_exclusive + short_exclusive / 10,
        "exclusive index batch: {short_exclusive} steps over 400 small blocks, {long_exclusive} over 6000"
    );

    // A capture with a large file and thousands of empty ones: one step reads and deletes at
    // most a row-count batch, and the large file is registered without reading the others.
    let capture_work = |empty_files: i64, huge: bool| {
        let db = Db::new();
        db.family(1);
        db.sql(&format!(
            "INSERT INTO thread_start_states(thread_id, root_run_id, project_root, repo_prefix, head_commit,
                 diff, captured_at_ns, replayable, reason)
             VALUES ('A', 'ra', '/p', '', 'abc', zeroblob(64), 1, 1, NULL);
             WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < {empty_files})
             INSERT INTO thread_start_untracked(thread_id, path, kind, executable, content)
             SELECT 'A', printf('z-%06d', x), 'file', 0, zeroblob(0) FROM n;"
        ));
        if huge {
            db.sql(
                "INSERT INTO thread_start_untracked(thread_id, path, kind, executable, content)
                 VALUES ('A', 'a-huge', 'file', 0, zeroblob(5242880));",
            );
        }
        confirm(&db);
        set_cursor(&db, r#"{"phase":"thread_tables","thread":"A"}"#);
        let storage = Storage::open(config(&db.path)).unwrap();
        let (bounded, progress) = one_step_scans(&storage);
        storage.close();
        let database = Database::open(&config(&db.path)).unwrap();
        let mut exclusive = None;
        let exclusive_steps =
            steps_of(|| exclusive = Some(database.reclaim_oversized_step(0).unwrap()));
        (
            bounded,
            progress,
            exclusive_steps,
            exclusive.unwrap(),
            db.int("SELECT COUNT(*) FROM reclaim_oversized"),
        )
    };
    let (short, short_progress, _, _, registered) = capture_work(1_000, false);
    let (long, long_progress, _, _, _) = capture_work(9_000, false);
    assert_eq!((short_progress.rows, long_progress.rows), (500, 500));
    assert_eq!(registered, 0);
    assert!(
        long.0 <= short.0 + short.0 / 10,
        "capture step: {short:?} over 1k files, {long:?} over 9k"
    );
    assert_eq!(
        (short.2, short.3, long.2, long.3),
        (0, 0, 0, 0),
        "no sort or automatic index"
    );
    let (short, short_progress, short_exclusive, short_batch, registered) =
        capture_work(1_000, true);
    let (long, long_progress, long_exclusive, long_batch, _) = capture_work(9_000, true);
    assert_eq!(
        (short_progress.rows, long_progress.rows, registered),
        (0, 0, 1)
    );
    assert!(
        long.0 <= short.0 + short.0 / 10,
        "registering a large file: {short:?} over 1k files, {long:?} over 9k"
    );
    assert_eq!(
        (short_batch.rows, long_batch.rows),
        (1, 1),
        "the large file goes alone"
    );
    assert!(short_exclusive > 0 && long_exclusive <= short_exclusive + short_exclusive / 10);
    // The next exclusive call deletes a bounded batch of the empty files.

    // The attribution backfill rewrites whole rows, so its frames follow the bytes it touches.
    for payload in [200_usize, 20_000] {
        let db = Db::new();
        db.sql(&format!(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 100)
             INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind)
             SELECT 1, 1, 1, 'Orchestrator',
                    json_object('kind', 'Orchestrator', 'payload', json_object('kind', 'TaskProgressed',
                      'payload', json_object('task_id', 't', 'run_id', 'run-' || x, 'progress',
                        json_object('pad', printf('%0{payload}d', x)), 'reason', 'r'))),
                    'TaskProgressed' FROM n;"
        ));
        db.sql("PRAGMA wal_checkpoint(TRUNCATE)");
        let observer = Connection::open(&db.path).unwrap();
        observer.execute_batch("BEGIN").unwrap();
        let _: i64 = observer
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        let before = wal_bytes(&db.path);
        let storage = Storage::open(config(&db.path)).unwrap();
        let progress = storage.handle().retention_backfill_step(100).unwrap();
        let after = wal_bytes(&db.path);
        storage.close();
        observer.execute_batch("COMMIT").unwrap();
        assert_eq!(progress.reattributed, 100);
        let frames = (after - before) / (db.page_size() + PAGE_OVERHEAD);
        let bound = 2 * pages(100 * (payload as u64 + 200), db.page_size()) + 4 * 100 + 64;
        assert!(
            frames <= bound,
            "backfill: {frames} frames for {payload}-byte payloads (bound {bound})"
        );
    }

    rusqlite::auto_extension::cancel_auto_extension(instrument);
}
