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
            FinalizeBudget::proportional(100_000),
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
                    FinalizeBudget::proportional(22),
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

    // Todo snapshots of other threads are scanned a bounded number of rows per step,
    // however long the history is, and the cursor advances although nothing matches.
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
        let (_, steps, progress) = one_step(&db, &storage);
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
        steps
    };
    let short = todo_step(3_000);
    let long = todo_step(12_000);
    assert!(
        long <= short + short / 10,
        "todo scan: {short} steps over 3k rows, {long} over 12k"
    );
    assert!(
        short <= 2_000 * 100,
        "todo scan takes {short} steps for 2000 rows"
    );

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
