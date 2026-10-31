use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child as Process, ChildStdin, ChildStdout, Command, Stdio};

use rusqlite::Connection;
use tempfile::TempDir;

use super::*;
use crate::inspect::StorageSummaryOptions;
use crate::{Storage, StorageConfig};

const CHILD_DB: &str = "EVORCH_TEST_INSPECT_WRITER_DB";
const CHILD_LEASE: &str = "EVORCH_TEST_INSPECT_WRITER_LEASE";
const INSERT: &str = "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind)
                      VALUES ('s', 1, 0, 0, 'Raw', '{}', 'Raw')";

fn create(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("storage.db");
    let db = Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    for _ in 0..3 {
        db.conn.execute(INSERT, []).unwrap();
    }
    drop(db);
    path.canonicalize().unwrap()
}

fn names(dir: &TempDir) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn exists(path: &Path, suffix: &str) -> bool {
    crate::db::suffixed_path(path, suffix).exists()
}

fn event_rows(inspected: &InspectedDatabase, path: &Path) -> i64 {
    inspected
        .storage_summary(path, StorageSummaryOptions::default())
        .unwrap()["events"]["rows"]
        .as_i64()
        .unwrap()
}

/// The writer of another process, driven command by command over its stdin. Each
/// command returns once the writer has done it, which is the only synchronization the
/// tests use. It is this test binary re-run on [`writer_child`].
struct Writer {
    process: Process,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Writer {
    fn spawn(path: &Path, lease: bool) -> Self {
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "inspect::open::tests::writer_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_DB, path)
            .env(CHILD_LEASE, if lease { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = process.stdin.take().unwrap();
        let stdout = BufReader::new(process.stdout.take().unwrap());
        let mut writer = Self {
            process,
            stdin,
            stdout,
        };
        writer.wait_ack();
        writer
    }

    fn wait_ack(&mut self) -> String {
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "writer exited early"
            );
            // The harness prints "test <name> ... " before the first line.
            if let Some((_, ack)) = line.trim().split_once("ACK ") {
                return ack.to_owned();
            }
        }
    }

    fn send(&mut self, command: &str) -> String {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
        self.wait_ack()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "exit");
        let _ = self.process.wait();
    }
}

#[test]
fn writer_child() {
    let Ok(path) = std::env::var(CHILD_DB) else {
        return;
    };
    let path = PathBuf::from(path);
    let mut lease = (std::env::var(CHILD_LEASE).as_deref() == Ok("1"))
        .then(|| crate::maintenance::offline::writer_lease(&path).unwrap());
    let mut conn: Option<Connection> = None;
    println!("ACK ready");
    for line in std::io::stdin().lines() {
        let reply = match line.unwrap().trim() {
            "open" => {
                let opened = Connection::open(&path).unwrap();
                opened.busy_timeout(std::time::Duration::ZERO).unwrap();
                opened.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
                conn = Some(opened);
                "opened".to_owned()
            }
            "insert" => {
                conn.as_ref().unwrap().execute(INSERT, []).unwrap();
                "inserted".to_owned()
            }
            "checkpoint" => {
                let busy: i64 = conn
                    .as_ref()
                    .unwrap()
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
                    .unwrap();
                format!("checkpoint busy={busy}")
            }
            "close" => {
                conn = None;
                "closed".to_owned()
            }
            "release" => {
                lease = None;
                "released".to_owned()
            }
            "exit" => break,
            other => panic!("unknown command {other}"),
        };
        println!("ACK {reply}");
    }
    drop((conn, lease));
}

#[test]
fn a_live_writer_may_commit_checkpoint_and_exit_during_a_scan_without_tearing_it() {
    let dir = TempDir::new().unwrap();
    let path = create(&dir);
    let mut writer = Writer::spawn(&path, true);
    writer.send("open");
    writer.send("insert");
    assert!(exists(&path, "-wal") && exists(&path, "-shm"));

    let inspected = Database::open_for_inspection(&path).unwrap();
    assert_eq!(inspected.mode(), InspectionMode::LiveWal);
    let summary = inspected
        .storage_summary_with(&path, StorageSummaryOptions::default(), &mut |phase| {
            match phase {
                "events" => {
                    writer.send("insert");
                    writer.send("checkpoint");
                }
                "run_contexts" => {
                    writer.send("insert");
                    writer.send("close");
                    writer.send("release");
                }
                _ => {}
            }
            Ok(())
        })
        .unwrap();
    inspected.finish().unwrap();

    // One committed snapshot: the 3 seeded rows and the writer's first insert, whatever
    // was committed or checkpointed while the scan ran.
    assert_eq!(summary["events"]["rows"], 4);
    let table = summary["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["table"] == "events")
        .unwrap();
    assert_eq!(table["rows"], 4);
    let attributed: i64 = ["active", "archived", "orphan", "unattributed"]
        .iter()
        .map(|bucket| {
            summary["attribution"]["events"][bucket]["rows"]
                .as_i64()
                .unwrap()
        })
        .sum();
    assert_eq!(attributed, 4);
    drop(inspected);

    // What the writer committed during the scan is there for the next one, whether its
    // sidecars were left behind or merged by the exit.
    let later = Database::open_for_inspection(&path).unwrap();
    assert_eq!(event_rows(&later, &path), 6);
}

#[test]
fn the_file_lock_keeps_an_exiting_writer_from_deleting_sidecars_before_the_open() {
    // Control: the last connection closing merges and deletes the sidecars.
    let dir = TempDir::new().unwrap();
    let path = create(&dir);
    let mut writer = Writer::spawn(&path, true);
    writer.send("open");
    writer.send("insert");
    assert!(exists(&path, "-wal") && exists(&path, "-shm"));
    writer.send("close");
    assert!(!exists(&path, "-wal") && !exists(&path, "-shm"));
    drop(writer);

    // With the lock held, the same exit leaves them for the open to find.
    let dir = TempDir::new().unwrap();
    let path = create(&dir);
    let mut writer = Writer::spawn(&path, true);
    writer.send("open");
    writer.send("insert");
    let pinned = pin(&path).unwrap();
    writer.send("close");
    assert!(exists(&path, "-wal") && exists(&path, "-shm"));
    drop(pinned);
}

#[test]
fn without_a_wal_current_writers_fail_fast_until_the_inspection_ends() {
    let dir = TempDir::new().unwrap();
    let path = create(&dir);
    assert_eq!(names(&dir), ["storage.db"]);

    let inspected = Database::open_for_inspection(&path).unwrap();
    assert_eq!(inspected.mode(), InspectionMode::NoWal);
    assert_eq!(event_rows(&inspected, &path), 3);
    assert_eq!(names(&dir), ["storage.db", "storage.db.maintenance-lock"]);

    let blocked = crate::maintenance::offline::writer_lease(&path).unwrap_err();
    assert!(
        blocked.to_string().contains("offline maintenance"),
        "{blocked}"
    );
    let refused = Storage::open(StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    });
    assert!(refused.is_err());
    assert_eq!(names(&dir), ["storage.db", "storage.db.maintenance-lock"]);
    inspected.finish().unwrap();
    drop(inspected);

    Storage::open(StorageConfig {
        db_path: path,
        ..Default::default()
    })
    .unwrap()
    .close();
}

#[test]
fn a_writer_that_may_be_opening_or_closing_is_refused_without_touching_anything() {
    let dir = TempDir::new().unwrap();
    let path = create(&dir);
    let _writer = crate::maintenance::offline::writer_lease(&path).unwrap();

    let error = Database::open_for_inspection(&path)
        .unwrap_err()
        .to_string();

    assert!(error.contains("retry shortly"), "{error}");
    assert_eq!(names(&dir), ["storage.db", "storage.db.maintenance-lock"]);
}

#[test]
fn offline_maintenance_refuses_inspection_with_or_without_a_wal() {
    let dir = TempDir::new().unwrap();
    let path = create(&dir);
    let maintenance = lease_file(&path).unwrap();
    maintenance.try_lock().unwrap();

    let error = Database::open_for_inspection(&path)
        .unwrap_err()
        .to_string();
    assert!(error.contains("offline maintenance"), "{error}");
    assert_eq!(names(&dir), ["storage.db", "storage.db.maintenance-lock"]);

    let live = Connection::open(&path).unwrap();
    live.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    live.execute(INSERT, []).unwrap();
    let error = Database::open_for_inspection(&path)
        .unwrap_err()
        .to_string();
    assert!(error.contains("offline maintenance"), "{error}");
}

#[test]
fn a_writer_that_ignores_the_lease_is_detected_and_nothing_is_reported() {
    let dir = TempDir::new().unwrap();
    let path = create(&dir);
    let mut writer = Writer::spawn(&path, false);

    let inspected = Database::open_for_inspection(&path).unwrap();
    assert_eq!(inspected.mode(), InspectionMode::NoWal);
    inspected.finish().unwrap();
    inspected
        .storage_summary_with(&path, StorageSummaryOptions::default(), &mut |phase| {
            if phase == "events" {
                writer.send("open");
                writer.send("insert");
            }
            Ok(())
        })
        .unwrap();

    let error = inspected.finish().unwrap_err().to_string();
    assert!(error.contains("changed during inspection"), "{error}");
}
