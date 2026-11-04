//! IO contracts of appending an event whose bodies are tokens: when the bodies are stored
//! already, an append writes the same whatever their size.
//!
//! Every figure is a count (WAL frames, statements), never a duration. One test measures
//! everything because SQLite's trace hooks are registered for every connection of the
//! process.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use event_bus::{Event, EventKind, EventMeta, ToolEvent};
use rusqlite::{Connection, ffi};
use serde_json::json;
use storage::{HardLimits, Storage, StorageConfig};

static STATEMENTS: AtomicU64 = AtomicU64::new(0);
static BLOB_DATA_SELECTS: AtomicU64 = AtomicU64::new(0);
static BLOB_INSERTS: AtomicU64 = AtomicU64::new(0);

// SQLite invokes the trace while the SQL text is valid; the callback only reads it and
// bumps static atomics, and never re-enters SQLite.
unsafe extern "C" fn trace(_: c_uint, _: *mut c_void, _: *mut c_void, sql: *mut c_void) -> c_int {
    if !sql.is_null() {
        // SAFETY: STMT supplies a NUL-terminated SQL string (or a trigger comment).
        let sql = unsafe { CStr::from_ptr(sql.cast::<c_char>()) }.to_string_lossy();
        if sql.starts_with("--") {
            return 0;
        }
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
        let sql = sql.trim_start();
        if sql.starts_with("SELECT") && sql.contains("data FROM content_blobs") {
            BLOB_DATA_SELECTS.fetch_add(1, Ordering::Relaxed);
        }
        if sql.starts_with("INSERT INTO content_blobs") {
            BLOB_INSERTS.fetch_add(1, Ordering::Relaxed);
        }
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
            ffi::SQLITE_TRACE_STMT as c_uint,
            Some(trace),
            std::ptr::null_mut(),
        )
    }
}

const PAGE_OVERHEAD: u64 = 24;
const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// Frames the same append may differ by between a 2 KiB and a 4 MiB body: one level of a
/// b-tree more or less. The token containers are equally long, so they take the same
/// number of pages.
const FRAME_DIFFERENCE: u64 = 4;

fn config(path: &Path) -> StorageConfig {
    StorageConfig {
        db_path: path.to_path_buf(),
        hard_limits: HardLimits {
            max_event_bytes: 16 * MIB as u64,
            ..HardLimits::default()
        },
        // Maintenance is triggered explicitly; elapsed time must not change work.
        checkpoint_interval: Duration::from_secs(3_600),
        flush_interval: Duration::from_secs(3_600),
        ..StorageConfig::default()
    }
}

fn wal_bytes(path: &Path) -> u64 {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(PathBuf::from(wal)).map_or(0, |meta| meta.len())
}

/// Text that does not compress, so that a blob's stored size follows its length.
fn noise(seed: u64, bytes: usize) -> String {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut text = String::with_capacity(bytes + 16);
    while text.len() < bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        text.push_str(&format!("{state:016x}"));
    }
    text.truncate(bytes);
    text
}

/// Two bodies of `size` bytes in three places: the output and, twice, the detail. One of
/// the bodies occurs twice in the event, so the event has three token positions and two
/// reference rows.
fn event(size: usize, seed: u64) -> Event {
    let (first, second) = (noise(seed, size), noise(seed + 1, size));
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_secs(1),
            wall_clock: UNIX_EPOCH + Duration::from_secs(1),
        },
        kind: EventKind::Tool(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "call".into(),
            is_error: false,
            output: Some(first.clone()),
            detail: Some(json!({"again": first, "other": second})),
            run_id: Some("run-a".into()),
            parent_call_id: None,
        }),
    }
}

#[derive(Debug, Clone, Copy)]
struct Cost {
    frames: u64,
    statements: u64,
    blob_data_selects: u64,
    blob_inserts: u64,
}

/// What one append costs. A snapshot pinned by the observer keeps the WAL from restarting,
/// so its growth is the frame volume.
fn measure(path: &Path, observer: &Connection, operation: impl FnOnce()) -> Cost {
    let page = observer
        .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0).map(u64::from))
        .unwrap();
    observer.execute_batch("BEGIN").unwrap();
    let _: i64 = observer
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    let before = (
        wal_bytes(path),
        STATEMENTS.load(Ordering::Relaxed),
        BLOB_DATA_SELECTS.load(Ordering::Relaxed),
        BLOB_INSERTS.load(Ordering::Relaxed),
    );
    operation();
    let cost = Cost {
        frames: (wal_bytes(path) - before.0) / (page + PAGE_OVERHEAD),
        statements: STATEMENTS.load(Ordering::Relaxed) - before.1,
        blob_data_selects: BLOB_DATA_SELECTS.load(Ordering::Relaxed) - before.2,
        blob_inserts: BLOB_INSERTS.load(Ordering::Relaxed) - before.3,
    };
    observer.execute_batch("COMMIT").unwrap();
    cost
}

struct Run {
    existing: Cost,
    creating: Cost,
    references: i64,
}

fn run(size: usize) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("io.db");
    let storage = Storage::open(config(&path)).unwrap();
    let handle = storage.handle();
    let observer = Connection::open(&path).unwrap();
    let first = event(size, 7);

    // The first append stores the bodies; its cost follows their size.
    let creating = measure(&path, &observer, || {
        handle.append_event(Some("s"), &first).unwrap();
    });
    // The same event again finds every body stored.
    let existing = measure(&path, &observer, || {
        handle.append_event(Some("s"), &first).unwrap();
    });
    let references: i64 = observer
        .query_row(
            "SELECT COUNT(*) FROM content_blob_refs_event WHERE event_id = 2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(observer);
    storage.close();
    Run {
        existing,
        creating,
        references,
    }
}

#[test]
fn appending_with_stored_bodies_writes_the_same_whatever_their_size() {
    // SAFETY: instrument only registers a callback; it opens/closes no DB.
    unsafe { rusqlite::auto_extension::register_auto_extension(instrument) }.unwrap();
    let small = run(2 * KIB);
    let large = run(4 * MIB);
    for (name, run) in [("2 KiB", &small), ("4 MiB", &large)] {
        println!(
            "{name}: existing bodies {:?}; creating bodies {:?}; references {}",
            run.existing, run.creating, run.references
        );
    }

    // With the bodies stored, a 4 MiB event costs what a 2 KiB one does.
    assert!(small.existing.frames > 0, "the trace must see the append");
    assert!(
        large.existing.frames.abs_diff(small.existing.frames) <= FRAME_DIFFERENCE,
        "frames of an append with stored bodies: {} for 2 KiB, {} for 4 MiB",
        small.existing.frames,
        large.existing.frames
    );
    assert_eq!(large.existing.statements, small.existing.statements);
    for run in [&small, &large] {
        assert_eq!(run.existing.blob_data_selects, 0, "no blob data is read");
        assert_eq!(run.existing.blob_inserts, 0, "no blob is created");
        // The body that occurs twice is one reference row, the other body a second one.
        assert_eq!(run.references, 2);
    }

    // Creating the bodies is what costs by size: it writes their stored bytes.
    assert!(large.creating.frames > small.creating.frames + 1_000);
    for run in [&small, &large] {
        assert_eq!(run.creating.blob_inserts, 2);
        assert_eq!(run.creating.blob_data_selects, 0);
    }
}
