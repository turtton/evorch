//! The writer and the collector as two processes on one database file.
//!
//! The child is this test binary run again on `child_entry`; it takes commands on stdin
//! and answers each with one `@@ ` line, so the order of the two processes' steps is
//! fixed by the test and never by timing. A child is killed (not stopped) to leave a
//! transaction open when the process dies.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde_json::json;
use tempfile::TempDir;

use super::test_owner::{OWNERS, count, install, load, store, text};
use crate::body_ref::BodyToken;
use crate::repo::content_blobs::{
    BodyScope, check_bodies, delete_unreferenced, gc_step, ref_tables,
};
use crate::{Database, StorageConfig};

const CHILD_DB: &str = "EVORCH_BLOB_TEST_CHILD_DB";

fn connect(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(60))
        .unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn
}

fn say(text: &str) {
    println!("@@ {text}");
    std::io::stdout().flush().unwrap();
}

/// Not a test of its own: the body of the child process. Without the variable it does nothing.
#[test]
fn child_entry() {
    let Ok(path) = std::env::var(CHILD_DB) else {
        return;
    };
    let conn = connect(Path::new(&path));
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut held: Option<Transaction<'_>> = None;
    while let Some(Ok(line)) = lines.next() {
        let words: Vec<&str> = line.split(' ').collect();
        match words.as_slice() {
            ["gc"] => {
                let progress = gc_step(&conn).unwrap();
                say(&format!(
                    "gc deleted={} retained={} stale={}",
                    progress.deleted, progress.retained, progress.stale
                ));
            }
            ["put", id, tag, len] => {
                let body = json!([text(tag, len.parse().unwrap())]);
                match store(&conn, id, &body) {
                    Ok(()) => say("put ok"),
                    Err(error) => say(&format!("put err {error}")),
                }
            }
            ["delete", id] => {
                conn.execute("DELETE FROM test_owner WHERE id = ?1", [id])
                    .unwrap();
                say("delete ok");
            }
            ["hold_put", id, tag, len] => {
                let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
                let body = json!([text(tag, len.parse().unwrap())]);
                super::test_owner::put(&tx, id, None, &body).unwrap();
                held = Some(tx);
                say("held");
            }
            ["hold_gc"] => {
                let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
                let tables = ref_tables(&tx).unwrap();
                let queued: Vec<i64> = tx
                    .prepare("SELECT blob_id FROM content_blob_gc_queue")
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                for id in queued {
                    delete_unreferenced(&tx, &tables, id).unwrap();
                    tx.execute("DELETE FROM content_blob_gc_queue WHERE blob_id = ?1", [id])
                        .unwrap();
                }
                held = Some(tx);
                say("held");
            }
            ["commit"] => {
                held.take().unwrap().commit().unwrap();
                say("committed");
            }
            ["exit"] => break,
            other => panic!("unknown command {other:?}"),
        }
    }
}

struct Peer {
    process: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Peer {
    fn spawn(path: &Path) -> Self {
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "body_store::process_tests::child_entry",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_DB, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = process.stdin.take().unwrap();
        let stdout = BufReader::new(process.stdout.take().unwrap());
        Self {
            process,
            stdin,
            stdout,
        }
    }

    /// Sends one command and returns the child's answer, blocking until it arrives.
    fn call(&mut self, command: &str) -> String {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).unwrap();
            assert!(read > 0, "the child exited before answering {command:?}");
            // The harness has printed "test ... " on this line before the test ran.
            if let Some((_, answer)) = line.split_once("@@ ") {
                return answer.trim_end().to_owned();
            }
        }
    }

    fn kill(mut self) {
        self.process.kill().unwrap();
        self.process.wait().unwrap();
    }

    fn finish(mut self) {
        let _ = writeln!(self.stdin, "exit");
        self.process.wait().unwrap();
    }
}

fn database(dir: &TempDir) -> (std::path::PathBuf, Connection) {
    let path = dir.path().join("blobs.db");
    let database = Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    install(&database.conn);
    drop(database);
    let conn = connect(&path);
    (path, conn)
}

fn assert_consistent(conn: &Connection) {
    let report = check_bodies(conn, OWNERS, BodyScope::All, true).unwrap();
    assert_eq!(report.problems, vec![], "the database must stay consistent");
}

fn blob_exists(conn: &Connection, body: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM content_blobs WHERE hash = ?1)",
        [&BodyToken::of(body).hash[..]],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn a_writer_that_saw_the_blob_before_the_collector_deleted_it_stores_it_again() {
    // Given: a blob that is queued (its owner was deleted), and a writer connection
    // that has just seen the blob exist.
    let dir = tempfile::tempdir().unwrap();
    let (path, conn) = database(&dir);
    let body = text("stale", 3_000);
    store(&conn, "o1", &json!([body.clone()])).unwrap();
    conn.execute("DELETE FROM test_owner WHERE id = 'o1'", [])
        .unwrap();
    assert!(blob_exists(&conn, &body));

    // When: another process collects it, and only then the writer stores the same text.
    let mut collector = Peer::spawn(&path);
    assert_eq!(collector.call("gc"), "gc deleted=1 retained=0 stale=0");
    assert!(!blob_exists(&conn, &body));
    store(&conn, "o2", &json!([body.clone()])).unwrap();

    // Then: the blob exists again, is referenced, and resolves; nothing dangles.
    assert_eq!(load(&conn, "o2").unwrap(), json!([body]));
    assert_eq!(collector.call("gc"), "gc deleted=0 retained=0 stale=0");
    assert_consistent(&conn);
    collector.finish();
}

#[test]
fn a_collector_that_runs_after_the_writer_committed_keeps_the_blob() {
    // Given: a queued blob.
    let dir = tempfile::tempdir().unwrap();
    let (path, conn) = database(&dir);
    let body = text("late", 3_000);
    store(&conn, "o1", &json!([body.clone()])).unwrap();
    conn.execute("DELETE FROM test_owner WHERE id = 'o1'", [])
        .unwrap();

    // When: another process re-references it before the collector's step.
    let mut writer = Peer::spawn(&path);
    assert_eq!(writer.call("put o2 late 3000"), "put ok");
    let progress = gc_step(&conn).unwrap();

    // Then: the step keeps the blob, clears the queue, and the row resolves.
    assert_eq!((progress.deleted, progress.retained), (0, 1));
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM content_blob_gc_queue"),
        0
    );
    assert_eq!(load(&conn, "o2").unwrap(), json!([body]));
    assert_consistent(&conn);
    writer.finish();
}

#[test]
fn alternating_deletes_collections_and_stores_never_lose_a_referenced_blob() {
    // Given: two processes working on the same few bodies in a fixed alternation.
    let dir = tempfile::tempdir().unwrap();
    let (path, conn) = database(&dir);
    let mut peer = Peer::spawn(&path);
    let tags = ["t0", "t1", "t2"];
    let mut live: Vec<Option<&str>> = vec![None; 4];

    // When: 60 steps alternate between this process and the child.
    for step in 0..60_usize {
        let slot = step % live.len();
        let tag = tags[(step / 2) % tags.len()];
        let id = format!("o{slot}");
        match step % 5 {
            0 => {
                store(&conn, &id, &json!([text(tag, 2_500)])).unwrap();
                live[slot] = Some(tag);
            }
            1 => {
                assert_eq!(peer.call(&format!("put {id} {tag} 2500")), "put ok");
                live[slot] = Some(tag);
            }
            2 => {
                conn.execute("DELETE FROM test_owner WHERE id = ?1", [&id])
                    .unwrap();
                live[slot] = None;
            }
            3 => {
                peer.call("gc");
            }
            _ => {
                gc_step(&conn).unwrap();
            }
        }

        // Then: after every step each live row resolves to its text.
        for (slot, tag) in live.iter().enumerate() {
            if let Some(tag) = tag {
                assert_eq!(
                    load(&conn, &format!("o{slot}")).unwrap(),
                    json!([text(tag, 2_500)]),
                    "step {step}"
                );
            }
        }
    }
    while gc_step(&conn).unwrap().candidates > 0 {}
    assert_consistent(&conn);
    peer.finish();
}

#[test]
fn a_writer_killed_inside_its_transaction_leaves_no_blob_owner_or_reference() {
    // Given: a child that stored a row and a blob but has not committed.
    let dir = tempfile::tempdir().unwrap();
    let (path, conn) = database(&dir);
    store(&conn, "keep", &json!([text("keep", 3_000)])).unwrap();
    let mut writer = Peer::spawn(&path);
    assert_eq!(writer.call("hold_put dead dead 3000"), "held");

    // When: it dies while holding the write lock, and its lock is released.
    conn.busy_timeout(std::time::Duration::ZERO).unwrap();
    let busy = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate);
    assert!(busy.is_err(), "the open transaction holds the write lock");
    drop(busy);
    writer.kill();
    conn.busy_timeout(std::time::Duration::from_secs(60))
        .unwrap();

    // Then: none of its writes survive, and the database is consistent and usable.
    assert!(!blob_exists(&conn, &text("dead", 3_000)));
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM test_owner WHERE id = 'dead'"),
        0
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM content_blobs"), 1);
    store(&conn, "next", &json!([text("next", 3_000)])).unwrap();
    assert_consistent(&conn);
}

#[test]
fn a_collector_killed_before_commit_deletes_nothing_and_the_next_step_finishes_the_job() {
    // Given: three queued blobs and a child that deleted them and the queue entries
    // but has not committed.
    let dir = tempfile::tempdir().unwrap();
    let (path, conn) = database(&dir);
    for id in ["a", "b", "c"] {
        store(&conn, id, &json!([text(id, 3_000)])).unwrap();
        conn.execute("DELETE FROM test_owner WHERE id = ?1", [id])
            .unwrap();
    }
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM content_blob_gc_queue"),
        3
    );
    let mut collector = Peer::spawn(&path);
    assert_eq!(collector.call("hold_gc"), "held");

    // When: it dies.
    collector.kill();

    // Then: the blobs and their queue entries are all still there, and a normal step
    // collects them.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM content_blobs"), 3);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM content_blob_gc_queue"),
        3
    );
    assert_eq!(gc_step(&conn).unwrap().deleted, 3);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM content_blobs"), 0);
    assert_consistent(&conn);
}
