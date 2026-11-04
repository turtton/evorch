//! IO contracts of saving a run's conversation as message rows: what a save writes and
//! reads is bounded by what changed, not by how long the conversation is.
//!
//! Every figure is a count (WAL frames, statements, SQLite VM steps), never a duration.
//! The measurements share one test because SQLite's trace hooks are registered for every
//! connection of the process.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, ffi};
use serde_json::{Value, json};
use storage::thread_index::{
    BlockOrigin, BlockProvenance, MessageProvenance, PROVENANCE_FORMAT_VERSION, ProvenanceEvidence,
    ProvenanceMap, ProvenanceStatus, SnapshotWrite,
};
use storage::{RunContextRecord, Storage, StorageConfig};

static VM_STEPS: AtomicU64 = AtomicU64::new(0);
static STATEMENTS: AtomicU64 = AtomicU64::new(0);
static BODY_SELECTS: AtomicU64 = AtomicU64::new(0);

// SQLite invokes the trace while the statement pointer and SQL text are valid; the
// callback only reads them and bumps static atomics, and never re-enters SQLite.
unsafe extern "C" fn trace(
    mask: c_uint,
    _: *mut c_void,
    statement: *mut c_void,
    sql: *mut c_void,
) -> c_int {
    if mask == ffi::SQLITE_TRACE_PROFILE as c_uint {
        // SAFETY: PROFILE supplies a live sqlite3_stmt; the reset avoids double counting.
        let steps = unsafe {
            ffi::sqlite3_stmt_status(statement.cast(), ffi::SQLITE_STMTSTATUS_VM_STEP, 1)
        };
        VM_STEPS.fetch_add(steps as u64, Ordering::Relaxed);
    } else if mask == ffi::SQLITE_TRACE_STMT as c_uint && !sql.is_null() {
        // SAFETY: STMT supplies a NUL-terminated SQL string (or a trigger comment).
        let sql = unsafe { CStr::from_ptr(sql.cast::<c_char>()) }.to_string_lossy();
        if sql.starts_with("--") {
            return 0;
        }
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
        // A read of a body: a message skeleton or a blob's data.
        if sql.trim_start().starts_with("SELECT")
            && (sql.contains("skeleton") || sql.contains("data FROM content_blobs"))
        {
            BODY_SELECTS.fetch_add(1, Ordering::Relaxed);
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
            (ffi::SQLITE_TRACE_STMT | ffi::SQLITE_TRACE_PROFILE) as c_uint,
            Some(trace),
            std::ptr::null_mut(),
        )
    }
}

const PAGE_OVERHEAD: u64 = 24;

/// Frames the same append may differ by between a short and a long conversation: one
/// level of the message table's b-tree (and of its two indexes) more or less. 11 frames
/// for both when first measured.
const FRAME_DIFFERENCE: u64 = 4;

/// What saving a conversation whose messages did not change may write: the header page,
/// the sidecar, the index bookkeeping and page 1. It does not depend on the length (5
/// frames for 100 and for 10,000 messages when first measured).
const UNCHANGED_SAVE_FRAMES: u64 = 12;

/// VM steps of a save outside the scan of the existing messages' metadata (header,
/// sidecar, index state, the new message's blob, row and references): about 900 when
/// first measured.
const SAVE_BASE_STEPS: u64 = 2_000;

/// VM steps per existing message: the metadata scan (`id`, `ordinal`, `msg_sha256`) and
/// the comparison. The scan reads no skeleton and no blob (7 when first measured: 1,596
/// steps for 100 messages, 70,896 for 10,000).
const STEPS_PER_EXISTING_MESSAGE: u64 = 16;

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

fn long_text(tag: &str, index: usize) -> String {
    let mut text = format!("{tag} {index}: ");
    while text.len() < 3_000 {
        text.push_str(&format!(
            "distinct line {tag} {index} {}\n",
            text.len() * 31 + index
        ));
    }
    text
}

/// `count` messages; every hundredth one carries a blob-sized text.
fn conversation(count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| {
            let text = if index % 100 == 0 {
                long_text("base", index)
            } else {
                format!("message {index}")
            };
            let role = if index % 2 == 0 { "user" } else { "assistant" };
            json!({"role": role, "content": [{"type": "text", "text": text}]})
        })
        .collect()
}

fn record(messages: &[Value], at: i64) -> RunContextRecord {
    RunContextRecord {
        run_id: "run-a".into(),
        role: "worker".into(),
        name: "io".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: serde_json::to_string(messages).unwrap(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: true,
        updated_at_ns: at,
        host_inputs_json: "[]".into(),
    }
}

fn with_map(record: RunContextRecord, count: usize, expected: Option<u64>) -> SnapshotWrite {
    SnapshotWrite {
        record,
        expected_snapshot_revision: expected,
        binding: None,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
        map: ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: count,
            checkpoint_count: 0,
            messages: (0..count)
                .map(|index| MessageProvenance {
                    raw_index: index,
                    ordinal: index,
                    blocks: vec![BlockProvenance {
                        block_index: 0,
                        origin: BlockOrigin::Native,
                        excluded_reason: None,
                    }],
                })
                .collect(),
        },
    }
}

#[derive(Debug, Clone, Copy)]
struct Cost {
    frames: u64,
    statements: u64,
    body_selects: u64,
    vm_steps: u64,
}

/// What one operation costs. A snapshot pinned by the observer keeps the WAL from
/// restarting, so its growth is the frame volume.
fn measure(path: &Path, observer: &Connection, operation: impl FnOnce()) -> Cost {
    let page = observer
        .pragma_query_value(None, "page_size", |row| row.get::<_, u32>(0).map(u64::from))
        .unwrap();
    observer.execute_batch("BEGIN").unwrap();
    let _: i64 = observer
        .query_row("SELECT COUNT(*) FROM run_messages", [], |row| row.get(0))
        .unwrap();
    let before = (
        wal_bytes(path),
        STATEMENTS.load(Ordering::Relaxed),
        BODY_SELECTS.load(Ordering::Relaxed),
        VM_STEPS.load(Ordering::Relaxed),
    );
    operation();
    let cost = Cost {
        frames: (wal_bytes(path) - before.0) / (page + PAGE_OVERHEAD),
        statements: STATEMENTS.load(Ordering::Relaxed) - before.1,
        body_selects: BODY_SELECTS.load(Ordering::Relaxed) - before.2,
        vm_steps: VM_STEPS.load(Ordering::Relaxed) - before.3,
    };
    observer.execute_batch("COMMIT").unwrap();
    cost
}

struct Run {
    cost_append: Cost,
    cost_unchanged: Cost,
    cost_bundle: Cost,
}

fn run(messages: usize) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("io.db");
    let storage = Storage::open(config(&path)).unwrap();
    let handle = storage.handle();
    let mut history = conversation(messages);
    let first = handle.upsert_run_context(&record(&history, 1));
    first.unwrap();
    let observer = Connection::open(&path).unwrap();

    // The same conversation saved again changes the header only.
    let cost_unchanged = measure(&path, &observer, || {
        handle.upsert_run_context(&record(&history, 2)).unwrap();
    });

    // One message with a blob-sized text appended.
    history.push(json!({"role": "user", "content": [{"type": "text", "text": long_text("added", messages)}]}));
    let cost_append = measure(&path, &observer, || {
        handle.upsert_run_context(&record(&history, 3)).unwrap();
    });

    // The runtime's own save also writes the provenance map, which holds one entry per
    // message: its cost is reported apart because it still grows with the length.
    history.push(json!({"role": "assistant", "content": [{"type": "text", "text": long_text("bundle", messages)}]}));
    let write = with_map(record(&history, 4), history.len(), None);
    let cost_bundle = measure(&path, &observer, || {
        handle.save_snapshot_bundle(&write).unwrap();
    });
    drop(observer);
    storage.close();
    Run {
        cost_append,
        cost_unchanged,
        cost_bundle,
    }
}

#[test]
fn saving_costs_what_changed_not_the_length_of_the_conversation() {
    // SAFETY: instrument only registers a callback; it opens/closes no DB.
    unsafe { rusqlite::auto_extension::register_auto_extension(instrument) }.unwrap();
    let short = run(100);
    let long = run(10_000);
    for (name, run) in [("100", &short), ("10,000", &long)] {
        println!(
            "{name} messages: append {:?}; unchanged {:?}; bundle with provenance map {:?}",
            run.cost_append, run.cost_unchanged, run.cost_bundle
        );
    }

    // Appending one message writes the same number of frames (up to one b-tree level
    // more) and issues the same statements, however long the conversation is.
    assert!(
        long.cost_append.frames.abs_diff(short.cost_append.frames) <= FRAME_DIFFERENCE,
        "frames of one append: {} for 100 messages, {} for 10,000",
        short.cost_append.frames,
        long.cost_append.frames
    );
    assert_eq!(
        long.cost_append.statements, short.cost_append.statements,
        "statements of one append"
    );
    assert!(short.cost_append.frames > 0, "the trace must see the save");

    // Saving what did not change writes a constant number of frames.
    for run in [&short, &long] {
        assert!(
            run.cost_unchanged.frames <= UNCHANGED_SAVE_FRAMES,
            "an unchanged save wrote {} frames",
            run.cost_unchanged.frames
        );
    }
    assert!(
        long.cost_unchanged
            .frames
            .abs_diff(short.cost_unchanged.frames)
            <= FRAME_DIFFERENCE,
        "frames of an unchanged save: {} for 100 messages, {} for 10,000",
        short.cost_unchanged.frames,
        long.cost_unchanged.frames
    );

    // No save reads a message skeleton or a blob's data: the existing messages are
    // compared by the hash their row already holds.
    for run in [&short, &long] {
        assert_eq!(run.cost_append.body_selects, 0);
        assert_eq!(run.cost_unchanged.body_selects, 0);
        assert_eq!(run.cost_bundle.body_selects, 0);
    }

    // VM steps: a constant plus the metadata scan of the existing messages.
    for (messages, run) in [(100_u64, &short), (10_000, &long)] {
        for (name, cost) in [
            ("append", run.cost_append),
            ("unchanged", run.cost_unchanged),
        ] {
            let bound = SAVE_BASE_STEPS + STEPS_PER_EXISTING_MESSAGE * messages;
            assert!(
                cost.vm_steps <= bound,
                "{name} save of {messages} messages: {} VM steps, bound {bound}",
                cost.vm_steps
            );
        }
    }
}
