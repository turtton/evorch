use std::fs;

use event_bus::{Event, OrchestratorEvent};
use rusqlite::{Connection, OpenFlags};

use super::*;
use crate::durable_tasks::DurableTasksModel;
use crate::{Database, Storage, StorageConfig};

fn event(input: Option<&str>, status: &str, artifact: Option<&str>) -> Event {
    Event::new(OrchestratorEvent::TaskProgressed {
        task_id: "task".into(),
        run_id: "run".into(),
        progress: serde_json::json!({ "status": status, "input": input, "last_artifact": artifact, "attempts": 1, "resume_cursor": "large old history".repeat(4096) }),
        reason: "boundary".into(),
    })
}

fn seed(path: &std::path::Path, events: &[Event]) -> StorageConfig {
    let config = StorageConfig {
        db_path: path.into(),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .append_event(
            Some("session"),
            &Event::new(event_bus::LifecycleEvent::Started {
                session_id: "session".into(),
            }),
        )
        .unwrap();
    for event in events {
        storage
            .handle()
            .append_event(Some("session"), event)
            .unwrap();
    }
    storage.close();
    config
}

fn snapshot(config: &StorageConfig) -> (crate::entity::TaskRecord, DurableTasksModel) {
    let db = Database::open(config).unwrap();
    // Snapshot the event-derived state explicitly. Seeding must exercise normal
    // append only, without accidentally supplying retention a tasks projection.
    crate::projection::reconcile(&db.conn).unwrap();
    let mut gui = DurableTasksModel::default();
    for stored in db.orchestrator_events_ordered().unwrap() {
        gui.apply_event(&stored.event);
    }
    (db.task("task").unwrap().unwrap(), gui)
}

#[test]
fn cursor_dry_run_changes_no_files_and_apply_backs_up_is_idempotent_and_preserves_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let config = seed(
        &path,
        &[
            event(Some("Title"), "running", None),
            event(None, "running", Some("result")),
            event(None, "completed", Some("result")),
        ],
    );
    let before = snapshot(&config);
    let bytes = fs::read(&path).unwrap();
    let names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let preview = strip_task_progress_cursor(&path, &StripOptions::default()).unwrap();
    assert!(preview.dry_run);
    assert_eq!(preview.target_rows, 3);
    assert!(preview.resume_cursor_bytes > 100_000);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let after_names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, after_names);
    let backup = dir.path().join("backup.db");
    let options = StripOptions {
        apply: true,
        backup: Some(backup.clone()),
        batch_size: 1,
        ..Default::default()
    };
    let applied = strip_task_progress_cursor(&path, &options).unwrap();
    assert_eq!(applied.rewritten_rows, 3);
    assert!(applied.vacuum && applied.checkpoint_truncated);
    assert!(applied.database_bytes_after < applied.database_bytes_before);
    assert_eq!(snapshot(&config), before);
    let (backup_conn, files) = offline::read_only_preview_connection(&backup).unwrap();
    assert_eq!(
        backup_conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE json_type(payload,'$.payload.payload.progress.resume_cursor') IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        3
    );
    files.verify().unwrap();
    drop(backup_conn);
    let repeated = strip_task_progress_cursor(
        &path,
        &StripOptions {
            apply: true,
            no_backup: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(repeated.rewritten_rows, 0);
    let conn = Connection::open(&path).unwrap();
    let tracked: i64 = conn
        .query_row(
            "SELECT payload_bytes FROM event_session_bytes WHERE session_id='session'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let actual: i64 = conn
        .query_row(
            "SELECT SUM(OCTET_LENGTH(payload)) FROM events WHERE session_id='session'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tracked, actual);
    assert_eq!(
        conn.query_row(
            "SELECT total_event_bytes FROM sessions WHERE id='session'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        actual
    );
}

#[test]
fn writer_lease_rejects_even_idle_writer_and_sqlite_rejects_legacy_reader() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let config = seed(&path, &[event(Some("Title"), "completed", None)]);
    let writer = Storage::open(config).unwrap();
    let options = StripOptions {
        apply: true,
        no_backup: true,
        ..Default::default()
    };
    assert!(strip_task_progress_cursor(&path, &options).is_err());
    assert!(strip_task_progress_cursor(&path, &StripOptions::default()).is_err());
    writer.close();
    let legacy = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    legacy
        .execute_batch("BEGIN; SELECT * FROM events;")
        .unwrap();
    assert!(strip_task_progress_cursor(&path, &options).is_err());
    legacy.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn interrupted_batch_can_resume_and_only_cursor_path_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    seed(
        &path,
        &[
            event(Some("Title"), "running", None),
            event(None, "completed", None),
        ],
    );
    let conn = Connection::open(&path).unwrap();
    let originals: Vec<(i64, String)> = conn
        .prepare("SELECT id,payload FROM events ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(offline::strip_batch(&conn, i64::MAX, 1).unwrap(), 1);
    drop(conn);
    let result = strip_task_progress_cursor(
        &path,
        &StripOptions {
            apply: true,
            no_backup: true,
            no_vacuum: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.rewritten_rows, 1);
    let conn = Connection::open(&path).unwrap();
    for (id, original) in originals {
        let mut expected: serde_json::Value = serde_json::from_str(&original).unwrap();
        if let Some(progress) = expected
            .pointer_mut("/payload/payload/progress")
            .and_then(serde_json::Value::as_object_mut)
        {
            progress.remove("resume_cursor");
        }
        let actual: String = conn
            .query_row("SELECT payload FROM events WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&actual).unwrap(),
            expected
        );
    }
}

#[test]
fn retention_compares_shared_gui_model_and_excludes_artifact_regression() {
    for (last_artifact, eligible) in [(None, false), (Some("result"), true)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let config = seed(
            &path,
            &[
                event(Some("Title"), "running", None),
                event(None, "running", Some("result")),
                event(None, "cancelled", last_artifact),
            ],
        );
        let before = snapshot(&config);
        let preview = preview_task_progress_retention(&path).unwrap();
        assert_eq!(preview.tasks_eligible, u64::from(eligible));
        assert_eq!(preview.tasks_excluded, u64::from(!eligible));
        let storage = Storage::open(config.clone()).unwrap();
        storage.handle().checkpoint_now().unwrap();
        storage.handle().reconcile().unwrap();
        storage.close();
        assert_eq!(before, snapshot(&config));
        let db = Database::open(&config).unwrap();
        assert_eq!(
            db.orchestrator_events_ordered().unwrap().len(),
            if eligible { 2 } else { 3 }
        );
    }
}

#[test]
fn retention_preserves_nonterminal_tasks_checkpoints_and_can_be_disabled() {
    for status in ["stopped", "failed", "running", "completed"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let mut config = seed(
            &path,
            &[
                event(Some("Title"), "running", None),
                event(None, "running", None),
                event(None, status, None),
            ],
        );
        config.retain_terminal_task_progress = status != "completed";
        let storage = Storage::open(config.clone()).unwrap();
        storage.handle().checkpoint_now().unwrap();
        storage.close();
        assert_eq!(
            Database::open(&config)
                .unwrap()
                .orchestrator_events_ordered()
                .unwrap()
                .len(),
            3
        );
    }
}

#[test]
fn retention_excludes_changed_input_attempts_generations_and_late_progress() {
    for case in [
        "input",
        "attempts",
        "generation",
        "late",
        "heartbeat",
        "invalid",
        "oversize",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let mut events = vec![
            event(Some("Title"), "running", None),
            event(None, "running", None),
            event(None, "completed", None),
        ];
        if let event_bus::EventKind::Orchestrator(OrchestratorEvent::TaskProgressed {
            progress,
            run_id,
            reason,
            ..
        }) = &mut events[1].kind
        {
            match case {
                "input" => progress["input"] = "Changed input".into(),
                "attempts" => progress["attempts"] = 9.into(),
                "generation" => *run_id = "another-run".into(),
                "heartbeat" => {
                    *reason = "task heartbeat".into();
                    progress["heartbeat_at_ns"] = 42.into();
                }
                "invalid" => progress["status"] = "unknown-state".into(),
                _ => {}
            }
        }
        if case == "late" {
            events.push(event(None, "running", None));
        }
        if case == "oversize" {
            events.splice(1..1, (0..256).map(|_| event(None, "running", None)));
        }
        let config = seed(&path, &events);
        // A different run or invalid intermediate record can be safely removed
        // if both production replays are truly identical. The other cases cannot.
        let before = snapshot(&config);
        let preview = preview_task_progress_retention(&path).unwrap();
        if matches!(case, "input" | "attempts" | "late" | "oversize") {
            assert_eq!(preview.tasks_eligible, 0, "{case}");
        }
        let storage = Storage::open(config.clone()).unwrap();
        storage.handle().checkpoint_now().unwrap();
        storage.handle().reconcile().unwrap();
        storage.close();
        assert_eq!(snapshot(&config), before, "{case}");
    }
}

#[test]
fn checkpoint_and_multiple_writer_arrivals_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let checkpoint = Event::new(OrchestratorEvent::TaskCheckpoint {
        task_id: "task".into(),
        run_id: "run".into(),
        tool_call_count: 2,
        cumulative_input_tokens: 3,
        cumulative_output_tokens: 4,
        elapsed_ms: 5,
    });
    let config = seed(
        &path,
        &[
            event(Some("Title"), "running", None),
            event(None, "running", None),
            event(None, "completed", None),
            checkpoint.clone(),
        ],
    );
    let before = snapshot(&config);
    let storage = Storage::open(config.clone()).unwrap();
    let other = Storage::open(config.clone()).unwrap();
    storage.handle().checkpoint_now().unwrap();
    other
        .handle()
        .append_event(None, &event(None, "running", Some("late")))
        .unwrap();
    storage.handle().checkpoint_now().unwrap();
    storage.handle().reconcile().unwrap();
    other.close();
    storage.close();
    let db = Database::open(&config).unwrap();
    assert!(
        db.orchestrator_events_ordered()
            .unwrap()
            .iter()
            .any(|stored| stored.event == checkpoint)
    );
    assert_eq!(db.task("task").unwrap().unwrap(), before.0);
}

#[test]
fn backup_collision_and_insufficient_space_fail_before_payload_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    seed(&path, &[event(Some("Title"), "completed", None)]);
    let backup = dir.path().join("backup.db");
    fs::write(&backup, b"do not overwrite").unwrap();
    let before = fs::read(&path).unwrap();
    assert!(
        strip_task_progress_cursor(
            &path,
            &StripOptions {
                apply: true,
                backup: Some(backup.clone()),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(backup).unwrap(), b"do not overwrite");
    assert!(offline::require_space(dir.path(), u64::MAX).is_err());
}

#[test]
fn cursor_null_unicode_and_unrelated_events_remain_semantically_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("space #? 日本語.db");
    let mut cursor_null = event(Some("タイトル"), "completed", Some("結果"));
    if let event_bus::EventKind::Orchestrator(OrchestratorEvent::TaskProgressed {
        progress, ..
    }) = &mut cursor_null.kind
    {
        progress["resume_cursor"] = serde_json::Value::Null;
        progress["nested"] = serde_json::json!({"resume_cursor":"not the obsolete field"});
    }
    let checkpoint = Event::new(OrchestratorEvent::TaskCheckpoint {
        task_id: "task".into(),
        run_id: "run".into(),
        tool_call_count: 1,
        cumulative_input_tokens: 1,
        cumulative_output_tokens: 1,
        elapsed_ms: 1,
    });
    let config = seed(&path, &[cursor_null, checkpoint.clone()]);
    assert_eq!(
        strip_task_progress_cursor(&path, &StripOptions::default())
            .unwrap()
            .target_rows,
        1
    );
    strip_task_progress_cursor(
        &path,
        &StripOptions {
            apply: true,
            no_backup: true,
            ..Default::default()
        },
    )
    .unwrap();
    let events = Database::open(&config)
        .unwrap()
        .orchestrator_events_ordered()
        .unwrap();
    assert_eq!(events[1].event, checkpoint);
    let event_bus::EventKind::Orchestrator(OrchestratorEvent::TaskProgressed { progress, .. }) =
        &events[0].event.kind
    else {
        panic!()
    };
    assert_eq!(progress["input"], "タイトル");
    assert_eq!(
        progress["nested"]["resume_cursor"],
        "not the obsolete field"
    );
    assert!(progress.get("resume_cursor").is_none());
}

#[test]
fn retention_uses_indexed_ranges_and_skips_oversized_history_before_loading_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let config = seed(
        &path,
        &[
            event(Some("Title"), "running", None),
            event(None, "running", None),
            event(None, "completed", None),
        ],
    );
    let db = Database::open(&config).unwrap();
    db.conn.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,payload) SELECT 1,0,x,'Message','{}' FROM n;").unwrap();
    // A VM guard makes a full history scan deterministic rather than a timing test.
    let budget = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = budget.clone();
    db.conn
        .progress_handler(
            100,
            Some(move || observed.fetch_add(100, std::sync::atomic::Ordering::Relaxed) > 10000),
        )
        .unwrap();
    let result = retention::tick(&db.conn, &mut Default::default()).unwrap();
    db.conn.progress_handler(0, None::<fn() -> bool>).unwrap();
    assert_eq!(result.event_count, 1);
}

#[test]
fn preview_is_read_only_on_old_schema_and_agrees_with_writer_selection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let config = seed(
        &path,
        &[
            event(Some("Title"), "running", None),
            event(None, "running", Some("result")),
            event(None, "completed", Some("result")),
        ],
    );
    let expected = preview_task_progress_retention(&path).unwrap();
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("ALTER TABLE usage_requests DROP COLUMN usage_reported; ALTER TABLE usage_daily DROP COLUMN unreported_count; ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN messages_len; ALTER TABLE run_contexts DROP COLUMN checkpoints_len; ALTER TABLE run_contexts DROP COLUMN snapshot_revision; DROP INDEX idx_events_durable_retention; DROP INDEX idx_events_task_lifecycle; PRAGMA user_version=25;").unwrap();
    }
    let before = fs::read(&path).unwrap();
    let actual = preview_task_progress_retention(&path).unwrap();
    assert_eq!(actual.event_count, expected.event_count);
    assert_eq!(actual.payload_bytes, expected.payload_bytes);
    assert_eq!(actual.tasks_excluded, expected.tasks_excluded);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        Database::open(&config)
            .unwrap()
            .pragma_i64("user_version")
            .unwrap(),
        i64::try_from(crate::migrations::MIGRATIONS.len()).unwrap()
    );
}

fn directory_contents(
    path: &std::path::Path,
) -> std::collections::BTreeMap<std::ffi::OsString, Vec<u8>> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect()
}

// Exit without destructors to leave WAL/SHM exactly as an abnormal exit does.
#[test]
fn wal_fixture_child() {
    use std::io::Write;
    let Some(path) = std::env::var_os("EVORCH_TEST_WAL_FIXTURE") else {
        return;
    };
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE events SET payload=json_remove(payload,'$.payload.payload.progress.resume_cursor') WHERE kind='Orchestrator';").unwrap();
    println!("WAL committed");
    std::io::stdout().flush().unwrap();
    // Deliberately skip Connection::drop/checkpoint, as on an abnormal exit.
    std::process::exit(0);
}

fn wal_fixture(path: &std::path::Path) -> std::process::Child {
    use std::io::BufRead;
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "maintenance::tests::wal_fixture_child",
            "--nocapture",
        ])
        .env("EVORCH_TEST_WAL_FIXTURE", path)
        .stdout(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        assert!(output.read_line(&mut line).unwrap() > 0);
        if line.contains("WAL committed") {
            break;
        }
        line.clear();
    }
    child
}

#[test]
fn wal_preview_never_changes_source_files_including_after_abnormal_exit() {
    for keep_shm in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        seed(&path, &[event(Some("Title"), "completed", None)]);
        let mut child = wal_fixture(&path);
        assert!(child.wait().unwrap().success());
        if !keep_shm {
            fs::remove_file(crate::db::suffixed_path(&path, "-shm")).unwrap();
        }
        let before = directory_contents(dir.path());
        assert!(!before[std::ffi::OsStr::new("events.db-wal")].is_empty());
        let error = strip_task_progress_cursor(&path, &StripOptions::default()).unwrap_err();
        assert!(error.to_string().contains("nonempty WAL"));
        assert!(preview_task_progress_retention(&path).is_err());
        assert_eq!(directory_contents(dir.path()), before);
    }
}

#[cfg(unix)]
#[test]
fn symlink_preview_uses_canonical_wal_and_propagates_sidecar_metadata_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("actual.db");
    seed(&path, &[event(Some("Title"), "completed", None)]);
    let mut child = wal_fixture(&path);
    assert!(child.wait().unwrap().success());
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("alias.db");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    let before = directory_contents(dir.path());
    assert!(
        strip_task_progress_cursor(&alias, &StripOptions::default())
            .unwrap_err()
            .to_string()
            .contains("nonempty WAL")
    );
    assert_eq!(directory_contents(dir.path()), before);
    assert_eq!(fs::read_dir(aliases.path()).unwrap().count(), 1);
    let wal = crate::db::suffixed_path(&path, "-wal");
    fs::remove_file(&wal).unwrap();
    // ELOOP is a deterministic metadata error even when tests run as root.
    std::os::unix::fs::symlink(&wal, &wal).unwrap();
    assert!(strip_task_progress_cursor(&alias, &StripOptions::default()).is_err());
    assert!(preview_task_progress_retention(&alias).is_err());
}

#[test]
fn preview_requires_a_lease_and_detects_database_or_sidecar_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    seed(&path, &[event(Some("Title"), "completed", None)]);
    for changed in ["events.db", "events.db-wal", "events.db-shm"] {
        let (conn, files) = offline::read_only_preview_connection(&path).unwrap();
        let file = dir.path().join(changed);
        let original = fs::read(&file).ok();
        fs::write(&file, b"concurrent modification").unwrap();
        assert!(files.verify().is_err(), "{changed}");
        drop(conn);
        if let Some(original) = original {
            fs::write(file, original).unwrap();
        } else {
            fs::remove_file(file).unwrap();
        }
    }
    fs::remove_file(offline::lease_path(&path).unwrap()).unwrap();
    let before = directory_contents(dir.path());
    assert!(
        strip_task_progress_cursor(&path, &StripOptions::default())
            .unwrap_err()
            .to_string()
            .contains("existing maintenance lease")
    );
    assert!(preview_task_progress_retention(&path).is_err());
    assert_eq!(directory_contents(dir.path()), before);
}

#[test]
fn retention_and_stripping_ignore_compressed_payloads_of_other_kinds() {
    // Given: task history next to a large tool output stored as a zstd BLOB.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let events = [
        event(Some("Title"), "running", None),
        event(None, "running", Some("result")),
        event(None, "completed", Some("result")),
    ];
    let config = seed(&path, &events);
    let expected = preview_task_progress_retention(&path).unwrap();
    {
        let storage = Storage::open(config.clone()).unwrap();
        storage
            .handle()
            .append_event(
                Some("session"),
                &Event::new(event_bus::ToolEvent::ToolCompleted {
                    tool_name: "shell".into(),
                    call_id: "call".into(),
                    is_error: false,
                    output: Some("large tool output\n".repeat(1000)),
                    detail: None,
                    run_id: Some("run".into()),
                    parent_call_id: None,
                }),
            )
            .unwrap();
        storage.close();
    }
    let db = Database::open(&config).unwrap();
    let tool_class: String = db
        .conn
        .query_row(
            "SELECT typeof(payload) FROM events WHERE kind = 'Tool'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tool_class, "blob");

    // When: retention previews and ticks over the mixed history.
    let actual = preview_task_progress_retention(&path).unwrap();
    let result = retention::tick(&db.conn, &mut Default::default()).unwrap();

    // Then: only the task rows are selected, exactly as without the BLOB row.
    assert_eq!(actual.event_count, expected.event_count);
    assert_eq!(actual.payload_bytes, expected.payload_bytes);
    assert_eq!(result.event_count, expected.event_count);
    let remaining: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'Tool'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 1);
}

/// Rebuild an existing database as auto_vacuum=NONE, as every database created
/// before INCREMENTAL was enabled, leaving a checkpointed WAL and a lease.
fn make_none(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "auto_vacuum", 0).unwrap();
    conn.execute_batch(
        "VACUUM; CREATE TABLE IF NOT EXISTS ballast(x BLOB); INSERT INTO ballast VALUES (x'01');",
    )
    .unwrap();
    assert_eq!(auto_vacuum(&conn), 0);
    drop(conn);
    drop(offline::writer_lease(path).unwrap());
}

fn legacy_none_database(path: &std::path::Path) {
    drop(
        Database::open(&StorageConfig {
            db_path: path.into(),
            ..Default::default()
        })
        .unwrap(),
    );
    make_none(path);
}

fn auto_vacuum(conn: &Connection) -> i64 {
    conn.pragma_query_value(None, "auto_vacuum", |row| row.get(0))
        .unwrap()
}

fn file_names(dir: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[test]
fn vacuum_switches_none_to_incremental_after_read_only_preview_and_verified_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    legacy_none_database(&path);
    let bytes = fs::read(&path).unwrap();
    let names = file_names(dir.path());

    let preview = vacuum_database(&path, &VacuumOptions::default()).unwrap();
    assert!(preview.preview && !preview.already_incremental && !preview.vacuum);
    assert_eq!(
        (preview.auto_vacuum_before, preview.auto_vacuum_after),
        (0, 0)
    );
    assert!(preview.backup.is_none() && preview.required_free_space_bytes > 0);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(file_names(dir.path()), names);

    let applied = vacuum_database(
        &path,
        &VacuumOptions {
            apply: true,
            backup: None,
        },
    )
    .unwrap();
    assert!(applied.vacuum && applied.checkpoint_truncated);
    assert_eq!(
        (applied.auto_vacuum_before, applied.auto_vacuum_after),
        (0, 2)
    );
    assert_eq!(applied.integrity_check.as_deref(), Some("ok"));
    let conn = Connection::open(&path).unwrap();
    assert_eq!(auto_vacuum(&conn), 2);
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM ballast", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1);

    // The backup is the verified pre-migration copy: still NONE, same data.
    let backup = Connection::open_with_flags(
        applied.backup.as_ref().unwrap(),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(auto_vacuum(&backup), 0);

    // The point of the migration: freed pages now return to the filesystem.
    conn.execute_batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<64)
         INSERT INTO ballast SELECT zeroblob(8192) FROM n; DELETE FROM ballast WHERE length(x) > 1;",
    )
    .unwrap();
    let free: i64 = conn
        .pragma_query_value(None, "freelist_count", |row| row.get(0))
        .unwrap();
    assert!(free >= 64);
    // Each step of incremental_vacuum releases one free page.
    let mut vacuum = conn.prepare("PRAGMA incremental_vacuum").unwrap();
    let mut steps = vacuum.query([]).unwrap();
    while steps.next().unwrap().is_some() {}
    drop(steps);
    drop(vacuum);
    let free: i64 = conn
        .pragma_query_value(None, "freelist_count", |row| row.get(0))
        .unwrap();
    assert_eq!(free, 0);
}

#[test]
fn vacuum_on_an_incremental_database_is_a_noop_without_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    legacy_none_database(&path);
    let options = VacuumOptions {
        apply: true,
        backup: None,
    };
    vacuum_database(&path, &options).unwrap();
    let bytes = fs::read(&path).unwrap();
    let names = file_names(dir.path());

    for apply in [false, true] {
        let again = vacuum_database(
            &path,
            &VacuumOptions {
                apply,
                backup: None,
            },
        )
        .unwrap();
        assert!(again.already_incremental && !again.vacuum && again.backup.is_none());
        assert_eq!((again.auto_vacuum_before, again.auto_vacuum_after), (2, 2));
        assert!(again.integrity_check.is_none());
    }
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(file_names(dir.path()), names);
}

#[test]
fn vacuum_refuses_an_existing_backup_path_before_changing_the_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    legacy_none_database(&path);
    let backup = dir.path().join("taken.db");
    fs::write(&backup, b"keep").unwrap();

    let result = vacuum_database(
        &path,
        &VacuumOptions {
            apply: true,
            backup: Some(backup.clone()),
        },
    );
    assert!(result.is_err());
    assert_eq!(fs::read(&backup).unwrap(), b"keep");
    let conn = Connection::open(&path).unwrap();
    assert_eq!(auto_vacuum(&conn), 0);
}

/// Records every inotify event in one directory. The kernel queues events
/// synchronously inside each syscall, so draining after a call returns observes
/// even a sidecar that was created and deleted again within that call.
#[cfg(target_os = "linux")]
struct DirectoryWatch(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl DirectoryWatch {
    fn new(dir: &std::path::Path) -> Self {
        use rustix::fs::inotify;
        let fd = inotify::init(inotify::CreateFlags::NONBLOCK).unwrap();
        inotify::add_watch(&fd, dir, inotify::WatchFlags::ALL_EVENTS).unwrap();
        Self(fd)
    }

    /// Events on anything but the database and its lease, or any event on the
    /// database that creates, changes, renames or deletes it.
    fn violations(&self) -> Vec<String> {
        use rustix::fs::inotify::{self, ReadFlags};
        let mutating = ReadFlags::MODIFY
            | ReadFlags::ATTRIB
            | ReadFlags::CLOSE_WRITE
            | ReadFlags::CREATE
            | ReadFlags::DELETE
            | ReadFlags::MOVED_FROM
            | ReadFlags::MOVED_TO;
        let mut buffer = [std::mem::MaybeUninit::uninit(); 4096];
        let mut reader = inotify::Reader::new(&self.0, &mut buffer);
        let mut found = Vec::new();
        loop {
            let event = match reader.next() {
                Err(rustix::io::Errno::WOULDBLOCK) => break,
                other => other.unwrap(),
            };
            let name = event
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let allowed = name == "events.db.maintenance-lock"
                || name == "events.db" && !event.events().intersects(mutating);
            if !allowed {
                found.push(format!("{name}: {:?}", event.events()));
            }
        }
        found
    }
}

#[cfg(target_os = "linux")]
fn assert_untouched(watch: &DirectoryWatch) {
    let violations = watch.violations();
    assert!(violations.is_empty(), "{violations:?}");
}

#[cfg(not(target_os = "linux"))]
struct DirectoryWatch;

#[cfg(not(target_os = "linux"))]
impl DirectoryWatch {
    fn new(_: &std::path::Path) -> Self {
        Self
    }
}

#[cfg(not(target_os = "linux"))]
fn assert_untouched(_: &DirectoryWatch) {}

#[test]
fn vacuum_that_changes_nothing_never_touches_the_database_or_its_sidecars() {
    // (legacy NONE database?, apply?, stale empty sidecars?)
    for (none, apply, sidecars) in [
        (false, false, false),
        (false, true, false),
        (false, true, true),
        (true, false, false),
        (true, false, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        seed(&path, &[event(Some("Title"), "completed", None)]);
        if none {
            make_none(&path);
        }
        if sidecars {
            fs::write(crate::db::suffixed_path(&path, "-wal"), b"").unwrap();
            fs::write(crate::db::suffixed_path(&path, "-shm"), b"").unwrap();
        }
        let before = directory_contents(dir.path());
        let watch = DirectoryWatch::new(dir.path());

        let summary = vacuum_database(
            &path,
            &VacuumOptions {
                apply,
                backup: None,
            },
        )
        .unwrap();

        assert_eq!(summary.already_incremental, !none);
        assert!(!summary.vacuum && summary.backup.is_none());
        assert_untouched(&watch);
        assert_eq!(directory_contents(dir.path()), before);
    }
}

#[test]
fn vacuum_refuses_a_nonempty_wal_without_touching_anything() {
    for (none, apply, keep_shm) in [
        (false, false, true),
        (false, true, true),
        (false, true, false),
        (true, true, true),
        (true, true, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        seed(&path, &[event(Some("Title"), "completed", None)]);
        if none {
            make_none(&path);
        }
        let mut child = wal_fixture(&path);
        assert!(child.wait().unwrap().success());
        if !keep_shm {
            fs::remove_file(crate::db::suffixed_path(&path, "-shm")).unwrap();
        }
        let before = directory_contents(dir.path());
        assert!(!before[std::ffi::OsStr::new("events.db-wal")].is_empty());
        let watch = DirectoryWatch::new(dir.path());

        let error = vacuum_database(
            &path,
            &VacuumOptions {
                apply,
                backup: None,
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("nonempty WAL"), "{error}");
        assert_untouched(&watch);
        assert_eq!(directory_contents(dir.path()), before);
    }
}

fn logical_content(conn: &Connection) -> (Vec<Vec<u8>>, i64, String) {
    let mut ballast = conn
        .prepare("SELECT x FROM ballast ORDER BY rowid")
        .unwrap();
    let rows = ballast
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let events = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    let integrity = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    (rows, events, integrity)
}

#[test]
fn vacuum_interrupted_after_the_backup_keeps_the_source_and_a_retry_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    seed(&path, &[event(Some("Title"), "completed", None)]);
    make_none(&path);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<300)
         INSERT INTO ballast SELECT randomblob(2048) FROM n;",
    )
    .unwrap();
    let expected = logical_content(&conn);
    drop(conn);
    drop(offline::writer_lease(&path).unwrap());

    // Interrupt inside the VACUUM itself: the pragma and mode reads run far
    // fewer VM steps than the interval, the table copy runs many more.
    let error = vacuum::vacuum_with(
        &path,
        &VacuumOptions {
            apply: true,
            backup: None,
        },
        |conn| conn.progress_handler(50, Some(|| true)).unwrap(),
    )
    .unwrap_err();
    assert!(
        error.to_string().to_lowercase().contains("interrupt"),
        "{error}"
    );

    let source = Connection::open(&path).unwrap();
    assert_eq!(auto_vacuum(&source), 0);
    assert_eq!(logical_content(&source), expected);
    drop(source);
    let backups: Vec<_> = file_names(dir.path())
        .into_iter()
        .filter(|name| name.to_string_lossy().contains(".before-vacuum-"))
        .collect();
    assert_eq!(backups.len(), 1);
    let backup = Connection::open_with_flags(
        dir.path().join(&backups[0]),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(auto_vacuum(&backup), 0);
    assert_eq!(logical_content(&backup), expected);

    let retry = vacuum_database(
        &path,
        &VacuumOptions {
            apply: true,
            backup: None,
        },
    )
    .unwrap();
    assert_eq!(retry.auto_vacuum_after, 2);
    let migrated = Connection::open(&path).unwrap();
    assert_eq!(auto_vacuum(&migrated), 2);
    assert_eq!(logical_content(&migrated), expected);
}
