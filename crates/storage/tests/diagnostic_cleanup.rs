//! Public cleanup semantics and recovery from the storage limit.

use std::time::{Duration, UNIX_EPOCH};

use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventMeta, MessageEvent};
use rusqlite::Connection;
use storage::{
    Database, DiagnosticCleanupScope as Scope, LimitKind, RunContextRecord, Storage, StorageConfig,
    StorageError,
};

fn config(path: std::path::PathBuf) -> StorageConfig {
    StorageConfig {
        db_path: path,
        checkpoint_interval: Duration::from_secs(3_600),
        ..StorageConfig::default()
    }
}

fn diagnostic(ns: u64, source: &str, code: &str, detail: &str) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::ZERO,
            wall_clock: UNIX_EPOCH + Duration::from_nanos(ns),
        },
        kind: DiagnosticEvent {
            source: source.into(),
            severity: DiagnosticSeverity::Info,
            code: code.into(),
            detail: detail.into(),
            run_id: Some("run".into()),
            thread_id: None,
            call_id: None,
        }
        .into(),
    }
}

fn context() -> RunContextRecord {
    RunContextRecord {
        run_id: "run".into(),
        role: "worker".into(),
        name: "restorable".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: r#"[{"content":"retain this conversation"}]"#.into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "completed".into(),
        restorable: true,
        updated_at_ns: 10,
    }
}

fn bytes(event: &Event) -> u64 {
    serde_json::to_vec(&event.kind).unwrap().len() as u64
}

#[test]
fn preview_and_cleanup_preserve_audits_conversation_goal_and_restore_context() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path().join("cleanup.db"));
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let old = diagnostic(99, "lsp", "progress", "古い診断");
    let boundary = diagnostic(100, "sandbox", "other", "boundary");
    let audit = diagnostic(1, "sandbox", "escalation_review", "approved");
    let other_source = diagnostic(99, "lsp", "escalation_review", "ordinary");
    let message = Event {
        kind: MessageEvent::MessageDelta {
            delta: "conversation".into(),
            run_id: None,
        }
        .into(),
        ..old.clone()
    };
    let goal = Event {
        kind: event_bus::OrchestratorEvent::GoalCreated {
            goal_id: "goal".into(),
            session_id: "session".into(),
            project_id: "project".into(),
            thread_id: "thread".into(),
            goal: "preserve goal".into(),
            references: vec![],
            constraints: vec![],
            repo: "owner/repo".into(),
            base_ref: "main".into(),
            root_run_id: "run".into(),
        }
        .into(),
        ..old.clone()
    };
    for event in [&old, &boundary, &audit, &other_source, &message, &goal] {
        handle.append_event(Some("session"), event).unwrap();
    }
    handle.upsert_run_context(&context()).unwrap();
    handle.reconcile().unwrap();
    let db = Database::open(&config).unwrap();
    let before = db.events_all_ordered().unwrap();
    let preview = handle
        .preview_diagnostic_cleanup(Scope::Before(100))
        .unwrap();
    assert_eq!(preview.event_count, 2);
    assert_eq!(preview.payload_bytes, bytes(&old) + bytes(&other_source));
    assert_eq!(db.events_all_ordered().unwrap(), before);

    let deleted = handle.cleanup_diagnostics(Scope::Before(100)).unwrap();
    assert_eq!(deleted.event_count, preview.event_count);
    assert_eq!(deleted.payload_bytes, preview.payload_bytes);
    assert!(deleted.maintenance_error.is_none());
    assert!(!deleted.requires_full_vacuum);
    assert_eq!(
        db.events_all_ordered()
            .unwrap()
            .into_iter()
            .map(|row| row.event)
            .collect::<Vec<_>>(),
        [&boundary, &audit, &message, &goal]
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
    );
    let remaining_bytes = [&boundary, &audit, &message, &goal]
        .into_iter()
        .map(bytes)
        .sum::<u64>();
    assert_eq!(
        db.session("session").unwrap().unwrap().total_event_bytes,
        remaining_bytes
    );
    assert_eq!(
        handle.cleanup_diagnostics(Scope::All).unwrap().event_count,
        1
    );
    assert_eq!(
        handle.cleanup_diagnostics(Scope::All).unwrap().event_count,
        0
    );
    storage.close();
    let db = Database::open(&config).unwrap();
    assert_eq!(db.run_context("run").unwrap(), Some(context()));
    assert_eq!(
        db.events_all_ordered()
            .unwrap()
            .into_iter()
            .map(|row| row.event)
            .collect::<Vec<_>>(),
        vec![audit, message, goal]
    );
    assert_eq!(
        db.restore_sessions().unwrap()[0].pending_message,
        "conversation"
    );
}

#[test]
fn cleanup_reseeds_warm_session_and_daily_quotas() {
    for session_limit in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let event = diagnostic(1, "lsp", "code", "日本語");
        let limit = bytes(&event);
        let mut config = config(dir.path().join("accounting.db"));
        if session_limit {
            config.hard_limits.max_session_bytes = limit;
        } else {
            config.hard_limits.max_daily_event_bytes = limit;
        }
        let storage = Storage::open(config.clone()).unwrap();
        let handle = storage.handle();
        let session = session_limit.then_some("session");
        handle.append_event(session, &event).unwrap();
        let error = handle.append_event(session, &event).unwrap_err();
        assert!(
            matches!(error, StorageError::LimitExceeded { limit: actual, .. }
            if actual == if session_limit { LimitKind::SessionSize } else { LimitKind::DailyBytes })
        );
        handle.cleanup_diagnostics(Scope::All).unwrap();
        handle
            .append_event(session, &event)
            .expect("quota capacity must be reusable immediately");
        assert!(handle.append_event(session, &event).is_err());
        let conn = Connection::open(&config.db_path).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT SUM(payload_bytes) FROM event_day_bytes",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            limit as i64
        );
        if session_limit {
            assert_eq!(
                conn.query_row(
                    "SELECT payload_bytes FROM event_session_bytes WHERE session_id = 'session'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                limit as i64
            );
        }
    }
}

#[test]
fn suspended_writer_cleanup_reclaims_space_and_resumes_context_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path().join("suspended.db"));
    let storage = Storage::open(config.clone()).unwrap();
    let event = diagnostic(1, "lsp", "large", &"x".repeat(32_768));
    for _ in 0..32 {
        storage.handle().append_event(None, &event).unwrap();
    }
    storage.close();
    config.hard_limits.max_db_bytes = 800_000;
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    assert!(handle.upsert_run_context(&context()).is_err());
    assert!(
        handle
            .preview_diagnostic_cleanup(Scope::All)
            .unwrap()
            .writes_suspended
    );
    let result = handle.cleanup_diagnostics(Scope::All).unwrap();
    assert_eq!(result.event_count, 32);
    assert!(result.reclaimed_bytes > 500_000, "{result:?}");
    assert!(!result.writes_suspended, "{result:?}");
    assert!(result.maintenance_error.is_none(), "{result:?}");
    handle
        .upsert_run_context(&context())
        .expect("restore writes resume in the same process");
    assert_eq!(
        Database::open(&config).unwrap().run_context("run").unwrap(),
        Some(context())
    );
}

#[test]
fn maintenance_cleans_expired_diagnostics_and_recovers_suspended_writer() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path().join("automatic.db"));
    let storage = Storage::open(config.clone()).unwrap();
    let expired = diagnostic(1, "lsp", "large", &"x".repeat(32_768));
    for _ in 0..32 {
        storage.handle().append_event(None, &expired).unwrap();
    }
    storage.close();
    config.hard_limits.max_db_bytes = 800_000;
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    assert!(handle.upsert_run_context(&context()).is_err());
    // Drive and await the same maintenance operation used by periodic ticks.
    handle.checkpoint_now().unwrap();
    assert_eq!(
        handle
            .preview_diagnostic_cleanup(Scope::All)
            .unwrap()
            .event_count,
        0
    );
    handle
        .upsert_run_context(&context())
        .expect("maintenance must reevaluate limits after reclamation");
}

#[test]
fn legacy_database_reports_deletion_without_claiming_file_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path().join("legacy.db"));
    {
        let conn = Connection::open(&config.db_path).unwrap();
        conn.execute_batch("CREATE TABLE legacy(x);").unwrap();
    }
    let storage = Storage::open(config.clone()).unwrap();
    let event = diagnostic(1, "lsp", "large", &"x".repeat(32_768));
    for _ in 0..32 {
        storage.handle().append_event(None, &event).unwrap();
    }
    storage.close();
    config.hard_limits.max_db_bytes = 800_000;
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    assert!(
        handle
            .preview_diagnostic_cleanup(Scope::All)
            .unwrap()
            .requires_full_vacuum
    );
    let result = handle.cleanup_diagnostics(Scope::All).unwrap();
    assert_eq!(result.event_count, 32);
    assert!(result.requires_full_vacuum);
    assert!(result.writes_suspended);
    assert!(handle.upsert_run_context(&context()).is_err());
    assert!(
        Database::open(&config)
            .unwrap()
            .events_all_ordered()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn malformed_or_mismatched_diagnostic_payloads_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path().join("unknown.db"));
    let storage = Storage::open(config.clone()).unwrap();
    let conn = Connection::open(&config.db_path).unwrap();
    for payload in [
        "not json",
        r#"{"kind":"Diagnostic","payload":{"source":"sandbox","code":"escalation_review"}}"#,
        r#"{"kind":"Message","payload":{"kind":"MessageDelta","payload":{"delta":"retain","run_id":null}}}"#,
    ] {
        conn.execute("INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload) VALUES(1,0,0,'Diagnostic',?1)", [payload]).unwrap();
    }
    let handle = storage.handle();
    assert_eq!(
        handle
            .preview_diagnostic_cleanup(Scope::All)
            .unwrap()
            .event_count,
        0
    );
    assert_eq!(
        handle.cleanup_diagnostics(Scope::All).unwrap().event_count,
        0
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
}

#[test]
fn active_reader_defers_wal_reclamation_and_cleanup_still_commits() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path().join("reader.db"));
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    handle
        .append_event(None, &diagnostic(1, "lsp", "code", "text"))
        .unwrap();
    let reader = Connection::open(&config.db_path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT * FROM events;")
        .unwrap();
    let result = handle.cleanup_diagnostics(Scope::All).unwrap();
    assert_eq!(result.event_count, 1);
    assert!(result.maintenance_error.is_some());
    reader.execute_batch("COMMIT").unwrap();
    // Retry reclamation even when there are no more disposable rows and the
    // pinned WAL was smaller than the normal WAL hard limit.
    handle.checkpoint_now().unwrap();
    let mut wal_path = config.db_path.as_os_str().to_owned();
    wal_path.push("-wal");
    assert_eq!(std::fs::metadata(wal_path).unwrap().len(), 0);
    let retry = handle.cleanup_diagnostics(Scope::All).unwrap();
    assert_eq!(retry.event_count, 0);
    assert!(retry.maintenance_error.is_none());
}

#[test]
fn failed_cleanup_rolls_back_rows_and_accounting_before_replying() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path().join("rollback.db"));
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let event = diagnostic(1, "lsp", "code", "text");
    for _ in 0..2 {
        handle.append_event(Some("session"), &event).unwrap();
    }
    let conn = Connection::open(&config.db_path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_cleanup BEFORE DELETE ON events WHEN OLD.id = 2
         BEGIN SELECT RAISE(ABORT, 'injected delete failure'); END;",
    )
    .unwrap();
    assert!(handle.cleanup_diagnostics(Scope::All).is_err());
    assert_eq!(
        handle
            .preview_diagnostic_cleanup(Scope::All)
            .unwrap()
            .event_count,
        2
    );
    assert_eq!(
        conn.query_row(
            "SELECT payload_bytes FROM event_session_bytes WHERE session_id = 'session'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2 * bytes(&event) as i64
    );
    conn.execute_batch("DROP TRIGGER reject_cleanup;").unwrap();
    assert_eq!(
        handle.cleanup_diagnostics(Scope::All).unwrap().event_count,
        2
    );
}

#[test]
fn failed_later_batch_reports_exact_committed_deletions_and_remaining_work() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path().join("partial.db"));
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let event = diagnostic(1, "lsp", "code", "text");
    for _ in 0..130 {
        handle.append_event(Some("session"), &event).unwrap();
    }
    let conn = Connection::open(&config.db_path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_later_batch BEFORE DELETE ON events WHEN OLD.id = 65
         BEGIN SELECT RAISE(ABORT, 'injected later-batch failure'); END;",
    )
    .unwrap();
    let result = handle.cleanup_diagnostics(Scope::All).unwrap();
    assert_eq!(result.event_count, 64);
    assert_eq!(result.payload_bytes, 64 * bytes(&event));
    assert!(result.maintenance_error.is_some());
    assert_eq!(
        handle
            .preview_diagnostic_cleanup(Scope::All)
            .unwrap()
            .event_count,
        66
    );
    assert_eq!(
        conn.query_row(
            "SELECT payload_bytes FROM event_session_bytes WHERE session_id = 'session'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        66 * bytes(&event) as i64
    );
    conn.execute_batch("DROP TRIGGER reject_later_batch;")
        .unwrap();
    let retry = handle.cleanup_diagnostics(Scope::All).unwrap();
    assert_eq!(retry.event_count, 66);
    assert!(retry.maintenance_error.is_none());
}
