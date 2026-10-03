//! Deterministic command interleaving; SQLite hooks signal batch boundaries.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event};
use rusqlite::{Connection, hooks::Action};

use super::{Command, Storage, StorageHandle, state::run_writer};
use crate::{Database, DiagnosticCleanupScope as Scope, RunContextRecord, StorageConfig};

fn config(dir: &tempfile::TempDir) -> StorageConfig {
    StorageConfig {
        db_path: dir.path().join("interleaving.db"),
        checkpoint_interval: Duration::from_secs(3_600),
        ..Default::default()
    }
}

fn start(conn: Connection, config: StorageConfig) -> Storage {
    let (tx, rx) = mpsc::sync_channel(128);
    let handle = StorageHandle(
        tx,
        config.hard_limits.max_event_bytes,
        Arc::new(config.clone()),
    );
    let writer =
        std::thread::spawn(move || run_writer(conn, rx, config, false, false, false, false));
    Storage(handle, Some(writer))
}

fn context() -> RunContextRecord {
    RunContextRecord {
        run_id: "active".into(),
        role: "worker".into(),
        name: "active".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: "[]".into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "running".into(),
        restorable: true,
        updated_at_ns: 1,
    }
}

#[test]
fn large_cleanup_serves_restore_writes_between_batches_and_preview_uses_another_connection() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    let conn = Database::open(&config).unwrap().conn;
    let diagnostic = Event::new(DiagnosticEvent {
        source: "lsp".into(),
        severity: DiagnosticSeverity::Info,
        code: "ordinary".into(),
        detail: "details".into(),
        run_id: None,
        thread_id: None,
        call_id: None,
    });
    let count = 10_000;
    conn.execute(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < ?1)
         INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,payload)
         SELECT 1,0,1,'Diagnostic',?2 FROM n",
        rusqlite::params![count, serde_json::to_string(&diagnostic.kind).unwrap()],
    )
    .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (observed_tx, observed_rx) = mpsc::channel();
    let mut deleted = 0;
    conn.update_hook(Some(move |action, _: &str, table: &str, _: i64| {
        if action == Action::SQLITE_DELETE && table == "events" {
            deleted += 1;
            if deleted == 64 {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
        }
        if action == Action::SQLITE_INSERT && table == "run_contexts" {
            observed_tx.send(deleted).unwrap();
        }
    }))
    .unwrap();
    let storage = start(conn, config.clone());
    let handle = storage.handle();
    let cleanup_handle = handle.clone();
    let cleanup = std::thread::spawn(move || cleanup_handle.cleanup_diagnostics(Scope::All));
    entered_rx.recv().unwrap();

    // The writer is held inside its first uncommitted delete batch. A preview
    // must finish on an independent read snapshot, retaining all original rows.
    let preview_result = handle.preview_diagnostic_cleanup(Scope::All);
    let (reply, result) = mpsc::channel();
    handle
        .0
        .send(Command::UpsertRunContext(context(), reply))
        .unwrap();
    let (append_reply, append_result) = mpsc::channel();
    handle
        .0
        .send(Command::AppendEvent(None, diagnostic.clone(), append_reply))
        .unwrap();
    release_tx.send(()).unwrap();
    assert_eq!(preview_result.unwrap().event_count, count as u64);
    result.recv().unwrap().unwrap();
    append_result.recv().unwrap().unwrap();
    assert_eq!(
        observed_rx.recv().unwrap(),
        64,
        "restore writes must run before the next deletion batch"
    );
    let deleted = cleanup.join().unwrap().unwrap();
    assert_eq!(deleted.event_count, count as u64);
    assert!(deleted.maintenance_error.is_none(), "{deleted:?}");
    let remaining = Database::open(&config)
        .unwrap()
        .events_all_ordered()
        .unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "new diagnostics beyond the initial sweep boundary survive"
    );
    assert_eq!(remaining[0].event, diagnostic);
    assert_eq!(
        Database::open(&config)
            .unwrap()
            .run_context("active")
            .unwrap(),
        Some(context())
    );
}

#[test]
fn manual_vacuum_yields_to_restore_writes_after_one_page_budget() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    let conn = Database::open(&config).unwrap().conn;
    conn.execute_batch(
        "CREATE TABLE filler(data BLOB); INSERT INTO filler VALUES(zeroblob(8388608));
         DROP TABLE filler; PRAGMA wal_checkpoint(TRUNCATE);",
    )
    .unwrap();
    let before: i64 = conn
        .pragma_query_value(None, "freelist_count", |row| row.get(0))
        .unwrap();
    assert!(before > 512);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (observed_tx, observed_rx) = mpsc::channel();
    let mut first = true;
    conn.commit_hook(Some(move || {
        if first {
            first = false;
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }
        false
    }))
    .unwrap();
    let reader = Connection::open(&config.db_path).unwrap();
    conn.update_hook(Some(move |action, _: &str, table: &str, _: i64| {
        if action == Action::SQLITE_INSERT && table == "run_contexts" {
            let remaining: i64 = reader
                .pragma_query_value(None, "freelist_count", |row| row.get(0))
                .unwrap();
            observed_tx.send(remaining).unwrap();
        }
    }))
    .unwrap();
    let storage = start(conn, config);
    let handle = storage.handle();
    let cleanup_handle = handle.clone();
    let cleanup = std::thread::spawn(move || cleanup_handle.cleanup_diagnostics(Scope::All));
    entered_rx.recv().unwrap();
    let (reply, result) = mpsc::channel();
    handle
        .0
        .send(Command::UpsertRunContext(context(), reply))
        .unwrap();
    release_tx.send(()).unwrap();
    result.recv().unwrap().unwrap();
    let remaining = observed_rx.recv().unwrap();
    assert_eq!(
        before - remaining,
        256,
        "restore write must run after one page budget, while most space remains to reclaim"
    );
    let result = cleanup.join().unwrap().unwrap();
    assert!(result.maintenance_error.is_none(), "{result:?}");
    assert!(result.reclaimed_bytes > 4_000_000, "{result:?}");
}
