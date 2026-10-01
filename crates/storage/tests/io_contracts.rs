//! Deterministic, file-backed I/O contracts. No wall-clock performance assertions.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use event_bus::{Event, EventMeta, MessageEvent};
use rusqlite::{Connection, ffi, params};
use storage::{Database, Storage, StorageConfig, StorageHandle};

static VM_STEPS: AtomicU64 = AtomicU64::new(0);
const APPENDS: usize = 96;

// This binary has one test and no production tracing changes. SQLite invokes
// PROFILE while the statement pointer is valid; callbacks only touch static
// atomics and never unwind, close a connection, or re-enter SQLite execution.
unsafe extern "C" fn profile(
    mask: c_uint,
    _: *mut c_void,
    statement: *mut c_void,
    _: *mut c_void,
) -> c_int {
    if mask == ffi::SQLITE_TRACE_PROFILE as c_uint {
        // SAFETY: PROFILE supplies a live sqlite3_stmt. Reset avoids double
        // counting when a cached statement is executed repeatedly.
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
    // SAFETY: SQLite owns this live connection. The callback and its state have
    // static lifetime; registration does not take ownership of the connection.
    unsafe {
        ffi::sqlite3_trace_v2(
            connection,
            ffi::SQLITE_TRACE_PROFILE as c_uint,
            Some(profile),
            std::ptr::null_mut(),
        )
    }
}

struct Instrumentation;

impl Instrumentation {
    fn install() -> Self {
        // SAFETY: instrument only registers a callback; it opens/closes no DB.
        unsafe { rusqlite::auto_extension::register_auto_extension(instrument) }.unwrap();
        Self
    }
}

impl Drop for Instrumentation {
    fn drop(&mut self) {
        rusqlite::auto_extension::cancel_auto_extension(instrument);
    }
}

fn event() -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_secs(1),
            wall_clock: UNIX_EPOCH + Duration::from_secs(1),
        },
        kind: MessageEvent::MessageDelta {
            delta: "x".repeat(64),
            run_id: None,
        }
        .into(),
    }
}

fn append(handle: &StorageHandle, index: usize, event: &Event) {
    match index % 3 {
        0 => handle.append_event(Some("session-a"), event),
        1 => handle.append_event(Some("session-b"), event),
        _ => handle.append_stream_event("gui-stream", event, None),
    }
    .expect("every measured append must be accepted");
}

fn wal_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push("-wal");
    value.into()
}

fn wal_bytes(path: &Path) -> u64 {
    std::fs::metadata(wal_path(path)).map_or(0, |metadata| metadata.len())
}

struct Measurements {
    warm_steps: u64,
    scan_steps: u64,
    wal_frames: u64,
}

fn measure(history: usize, multiwriter: bool) -> Measurements {
    let directory = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: directory.path().join("io-contract.db"),
        // Tests explicitly trigger maintenance; elapsed time must not change work.
        checkpoint_interval: Duration::from_secs(3_600),
        flush_interval: Duration::from_secs(3_600),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).unwrap());
    let mut observer = Connection::open(&config.db_path).unwrap();
    let event = event();
    let payload = serde_json::to_string(&event.kind).unwrap();
    let transaction = observer.transaction().unwrap();
    transaction
        .execute_batch(
            "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns)
             VALUES ('session-a', 'running', 0, 0), ('session-b', 'running', 0, 0);",
        )
        .unwrap();
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO events
                 (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload)
                 VALUES (?1, ?2, 1000000000, 1000000000, 'Message', ?3)",
            )
            .unwrap();
        for index in 0..history {
            let session = ["session-a", "session-b", "gui-stream"][index % 3];
            insert
                .execute(params![session, event_bus::SCHEMA_VERSION, payload])
                .unwrap();
        }
    }
    transaction
        .execute_batch(
            "UPDATE sessions SET total_event_bytes =
             (SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events
              WHERE session_id = sessions.id);",
        )
        .unwrap();
    transaction.commit().unwrap();
    observer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let other_storage = multiwriter.then(|| Storage::open(config.clone()).unwrap());
    let other_handle = other_storage.as_ref().map(Storage::handle);
    for index in 0..3 {
        append(&handle, index, &event);
    }

    // A pinned snapshot prevents WAL restart/recycling during the measurement.
    // Therefore size growth is the actual appended frame volume, not net size
    // after a checkpoint. Do not checkpoint externally after warming: a WAL
    // reset can change the writer's data_version and intentionally reseed it.
    observer.execute_batch("BEGIN").unwrap();
    let initial_count: i64 = observer
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(initial_count, (history + 3) as i64);
    let before_wal = wal_bytes(&config.db_path);
    assert!(before_wal >= 32, "warm-up must have created a WAL header");
    VM_STEPS.store(0, Ordering::Relaxed);
    for index in 0..APPENDS {
        let writer = if index % 2 == 1 {
            other_handle.as_ref().unwrap_or(&handle)
        } else {
            &handle
        };
        append(writer, index, &event);
    }
    let warm_steps = VM_STEPS.load(Ordering::Relaxed);
    let after_wal = wal_bytes(&config.db_path);
    let page_size: u64 = observer
        .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0).map(u64::from))
        .unwrap();
    assert!(after_wal >= before_wal);
    assert_eq!((after_wal - before_wal) % (page_size + 24), 0);
    let wal_frames = (after_wal - before_wal) / (page_size + 24);

    // Repeated empty flush and maintenance requests must not dirty DB pages.
    for _ in 0..32 {
        handle.flush_usage_now().unwrap();
        handle.checkpoint_now().unwrap();
    }
    assert_eq!(
        wal_bytes(&config.db_path),
        after_wal,
        "idle ticks must add no WAL frames"
    );
    observer.execute_batch("COMMIT").unwrap();
    let count: i64 = observer
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, (history + 3 + APPENDS) as i64);

    // Positive control: prove the profiler detects an actual history scan.
    VM_STEPS.store(0, Ordering::Relaxed);
    let _: i64 = observer
        .query_row("SELECT SUM(OCTET_LENGTH(payload)) FROM events", [], |row| {
            row.get(0)
        })
        .unwrap();
    let scan_steps = VM_STEPS.load(Ordering::Relaxed);
    storage.close();
    if let Some(other) = other_storage {
        other.close();
    }
    Measurements {
        warm_steps,
        scan_steps,
        wal_frames,
    }
}

#[test]
fn warm_appends_and_idle_ticks_have_bounded_io() {
    let _instrumentation = Instrumentation::install();
    for multiwriter in [false, true] {
        let small = measure(128, multiwriter);
        let large = measure(8_192, multiwriter);
        println!(
            "multiwriter={multiwriter}; warm VM steps: {} / {}; WAL frames: {} / {} (128 / 8192 history rows)",
            small.warm_steps, large.warm_steps, small.wal_frames, large.wal_frames
        );
        assert!(
            small.warm_steps > 0,
            "SQLite profiler must observe writer work"
        );
        assert!(
            large.scan_steps > small.scan_steps * 16,
            "positive control must detect linear scans"
        );
        assert!(
            large.warm_steps <= small.warm_steps * 2,
            "64x history must not scale warm work: {} -> {} VM steps for {APPENDS} accepted appends",
            small.warm_steps,
            large.warm_steps
        );
        // Table + two indexes + session projection + two compact totals and
        // B-tree splits fit in the unchanged page budget. It deliberately measures durable work, not payload bytes.
        for (name, measurement) in [("small", &small), ("large", &large)] {
            assert!(
                measurement.wal_frames <= APPENDS as u64 * 6 + 16,
                "{name}: {} WAL frames exceed the append budget",
                measurement.wal_frames
            );
        }
    }
}
