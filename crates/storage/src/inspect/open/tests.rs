use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use tempfile::TempDir;

use super::*;
use crate::inspect::StorageSummaryOptions;
use crate::{Storage, StorageConfig};

const PROBE_DB: &str = "EVORCH_TEST_INSPECT_LOCK_PROBE_DB";
const INSERT_EVENT: &str = "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind)
                            VALUES ('s', 1, 0, 0, 'Raw', '{}', 'Raw')";
const INSERT_CONTEXT: &str = "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json,
                                  checkpoints_json, terminal_phase, restorable, updated_at_ns, host_inputs_json)
                              VALUES (lower(hex(randomblob(8))), 'worker', 'n', NULL, '{}', '[]', '[]', 'Done', 0, 0, '[]')";
const INSERT_INDEX_RUN: &str = "INSERT INTO thread_index_runs(run_id, state, source_revision, snapshot_revision,
                                    source_context_sha256, extractor_version, accounting_version,
                                    source_updated_at_ns, retention_at_ns)
                                VALUES (lower(hex(randomblob(8))), 'pending', 1, 1, 'sha', 1, 1, 0, 0)";

/// A database with 3 events, 1 run snapshot and 2 index runs; the returned connection
/// is the live writer, so the WAL and its index exist.
fn create(dir: &TempDir) -> (PathBuf, Database) {
    let path = dir.path().join("storage.db");
    let db = Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    for _ in 0..3 {
        db.conn.execute(INSERT_EVENT, []).unwrap();
    }
    db.conn.execute(INSERT_CONTEXT, []).unwrap();
    for _ in 0..2 {
        db.conn.execute(INSERT_INDEX_RUN, []).unwrap();
    }
    db.conn
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    (path.canonicalize().unwrap(), db)
}

fn summary(inspected: &InspectedDatabase, path: &Path) -> serde_json::Value {
    inspected
        .storage_summary(path, StorageSummaryOptions::default())
        .unwrap()
}

fn counts(summary: &serde_json::Value) -> [i64; 3] {
    [
        summary["events"]["rows"].as_i64().unwrap(),
        summary["run_contexts"]["rows"].as_i64().unwrap(),
        summary["conversation_index"]["runs"].as_i64().unwrap(),
    ]
}

fn copy_live(path: &Path, dir: &TempDir) -> PathBuf {
    let copy = dir.path().join("copy.db");
    std::fs::copy(path, &copy).unwrap();
    std::fs::copy(
        crate::db::suffixed_path(path, "-wal"),
        crate::db::suffixed_path(&copy, "-wal"),
    )
    .unwrap();
    copy
}

#[test]
fn every_table_of_a_scan_comes_from_one_snapshot_whatever_a_writer_commits_meanwhile() {
    let dir = TempDir::new().unwrap();
    let (path, writer) = create(&dir);
    writer.conn.busy_timeout(std::time::Duration::ZERO).unwrap();

    let inspected = Database::open_for_inspection(&path).unwrap();
    let scanned = inspected
        .storage_summary_with(&path, StorageSummaryOptions::default(), &mut |phase| {
            // Events are done; the snapshots and the index are still to be read.
            if phase == "events" {
                writer
                    .conn
                    .execute_batch(&format!(
                        "BEGIN; {INSERT_EVENT}; {INSERT_CONTEXT}; {INSERT_INDEX_RUN}; COMMIT;"
                    ))
                    .unwrap();
                writer
                    .conn
                    .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
                    .unwrap();
            }
            Ok(())
        })
        .unwrap();

    assert_eq!(counts(&scanned), [3, 1, 2]);
    drop(inspected);

    let later = Database::open_for_inspection(&path).unwrap();
    assert_eq!(counts(&summary(&later, &path)), [4, 2, 3]);
}

#[test]
fn inspection_never_merges_a_nonempty_wal_into_the_database_file() {
    let dir = TempDir::new().unwrap();
    let (path, writer) = create(&dir);
    let originals = TempDir::new().unwrap();
    let copy = copy_live(&path, &originals);
    let database = std::fs::read(&copy).unwrap();
    let wal = std::fs::read(crate::db::suffixed_path(&copy, "-wal")).unwrap();
    assert!(!wal.is_empty());

    let inspected = Database::open_for_inspection(&copy).unwrap();
    assert_eq!(counts(&summary(&inspected, &copy)), [3, 1, 2]);
    assert!(
        inspected
            .conn
            .db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE)
            .unwrap()
    );
    drop(inspected);

    assert!(std::fs::read(&copy).unwrap() == database);
    assert!(std::fs::read(crate::db::suffixed_path(&copy, "-wal")).unwrap() == wal);

    // Control: an ordinary read-write open merges the same WAL when it closes, so the
    // checks above can tell a checkpoint from none.
    let control = TempDir::new().unwrap();
    let merged = copy_live(&path, &control);
    let merging = Connection::open(&merged).unwrap();
    merging
        .query_row("SELECT count(*) FROM events", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    drop(merging);
    assert!(std::fs::read(&merged).unwrap() != database);
    drop(writer);
}

/// Whether another process can take the database file exclusively right now.
fn exclusive_lock_is_free(path: &Path) -> bool {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inspect::open::tests::lock_probe_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE_DB, path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The harness prints "test <name> ... " before the first line.
    match stdout.split_once("ACK ").map(|(_, rest)| rest.trim_start()) {
        Some(rest) if rest.starts_with("free") => true,
        Some(rest) if rest.starts_with("busy") => false,
        _ => panic!("probe gave no answer: {stdout}"),
    }
}

#[test]
fn lock_probe_child() {
    let Ok(path) = std::env::var(PROBE_DB) else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let free =
        rustix::fs::fcntl_lock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok();
    println!("ACK {}", if free { "free" } else { "busy" });
}

#[test]
fn inspecting_leaves_the_locks_other_connections_hold_in_this_process_alone() {
    let dir = TempDir::new().unwrap();
    let (path, writer) = create(&dir);
    // An open read transaction holds the connection's shared lock on the file.
    let reading = writer.conn.unchecked_transaction().unwrap();
    reading
        .query_row("SELECT count(*) FROM events", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    assert!(!exclusive_lock_is_free(&path));

    let inspected = Database::open_for_inspection(&path).unwrap();
    assert_eq!(counts(&summary(&inspected, &path)), [3, 1, 2]);
    drop(inspected);
    assert!(!exclusive_lock_is_free(&path), "inspection released a lock");

    // Control: closing a descriptor of our own is what would have released it.
    drop(std::fs::File::open(&path).unwrap());
    assert!(exclusive_lock_is_free(&path));
    drop(reading);
}

#[test]
fn inspection_shares_the_lease_with_writers_and_yields_to_maintenance() {
    let dir = TempDir::new().unwrap();
    let (path, writer) = create(&dir);
    drop(writer);

    let running = crate::maintenance::offline::writer_lease(&path).unwrap();
    let inspected = Database::open_for_inspection(&path).unwrap();
    drop(running);

    // A writer can start while an inspection runs, and maintenance cannot.
    let later = crate::maintenance::offline::writer_lease(&path).unwrap();
    let maintenance = lease_file(&path).unwrap();
    assert!(maintenance.try_lock().is_err());
    drop(later);
    Storage::open(StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap()
    .close();
    drop(inspected);

    maintenance.try_lock().unwrap();
    let error = Database::open_for_inspection(&path)
        .unwrap_err()
        .to_string();
    assert!(error.contains("offline maintenance"), "{error}");
}
