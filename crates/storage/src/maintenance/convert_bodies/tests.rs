use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::*;
use crate::Database;
use crate::StorageConfig;
use crate::codec;
use crate::migrations::MIGRATIONS;
use crate::repo::run_messages;

const MIB: usize = 1024 * 1024;

pub(super) fn lease(path: &Path) {
    drop(offline::lease_file(path).unwrap());
}

/// A database exactly as migration `version` leaves it, built by replaying the real migrations.
pub(super) fn database_at(dir: &Path, version: usize) -> PathBuf {
    let path = dir.join("events.db");
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "auto_vacuum", 2).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    for migration in &MIGRATIONS[..version] {
        conn.execute_batch(migration).unwrap();
    }
    conn.pragma_update(None, "user_version", version).unwrap();
    drop(conn);
    lease(&path);
    path
}

/// The schema version a converted database ends at: the conversion applies every migration
/// after v40 before it publishes.
fn latest_schema() -> u32 {
    u32::try_from(MIGRATIONS.len()).unwrap()
}

pub(super) fn v39(dir: &Path) -> PathBuf {
    database_at(dir, 39)
}

pub(super) struct Run {
    id: &'static str,
    messages: String,
    checkpoints: String,
    config: String,
    revision: i64,
    updated: i64,
    restorable: bool,
}

pub(super) fn long(tag: &str, size: usize) -> String {
    let unit = format!("{tag} こんにちは \"quoted\" \\ line\n");
    unit.repeat(size / unit.len() + 1)
}

pub(super) fn plain_run(id: &'static str, messages: &Value, revision: i64) -> Run {
    Run {
        id,
        messages: serde_json::to_string(messages).unwrap(),
        checkpoints: "[]".into(),
        config: "{}".into(),
        revision,
        updated: 1_000 + revision,
        restorable: true,
    }
}

pub(super) fn insert(conn: &Connection, run: &Run) {
    let messages = codec::encode(&run.messages);
    let checkpoints = codec::encode(&run.checkpoints);
    conn.execute(
        "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json, \
         checkpoints_json, terminal_phase, restorable, updated_at_ns, host_inputs_json, \
         snapshot_revision, messages_len, checkpoints_len) \
         VALUES (?1, 'worker', ?2, NULL, ?3, ?4, ?5, 'Done', ?10, ?6, '[\"input-1\"]', ?7, ?8, ?9)",
        params![
            run.id,
            format!("name-{}", run.id),
            run.config,
            messages,
            checkpoints,
            run.updated,
            run.revision,
            run.messages.len() as i64,
            run.checkpoints.len() as i64,
            run.restorable
        ],
    )
    .unwrap();
}

pub(super) fn conversation(tag: &str) -> Value {
    let shared = long("shared tool output", 5_000);
    json!([
        {"role": "system", "content": [{"type": "text", "text": format!("system {tag}")}]},
        {"role": "user", "content": [{"type": "text", "text": "hello"}]},
        {"role": "assistant", "content": [
            {"type": "reasoning", "text": long(&format!("reasoning {tag}"), 3_000)},
            {"type": "tool_use", "id": "t1", "name": "shell", "input": {"cmd": long(tag, 2_500)}}
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": shared.clone()}
        ]},
        {"role": "assistant", "content": [{"type": "text", "text": shared}]},
        {"role": "user", "content": [{"type": "text", "text": "\u{e000}looks like a token"}]},
        {"role": "assistant", "content": [{"type": "text", "text": "x".repeat(2_047)}]},
        {"role": "assistant", "content": [{"type": "text", "text": "x".repeat(2_048)}]},
    ])
}

pub(super) fn seeded() -> (tempfile::TempDir, PathBuf, Vec<Run>) {
    seeded_at(39)
}

pub(super) fn seeded_at(version: usize) -> (tempfile::TempDir, PathBuf, Vec<Run>) {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), version);
    let mut big = plain_run("run-big", &conversation("big"), 7);
    big.checkpoints = serde_json::to_string(&json!([
        {"summary": {"role": "assistant", "content": [{"type": "text", "text": long("summary", 4_000)}]}, "end": 3}
    ]))
    .unwrap();
    big.config = serde_json::to_string(
        &json!({"restorable": false, "non_restorable_reason": "persist_failed", "model": "m"}),
    )
    .unwrap();
    let runs = vec![
        plain_run("run-a-empty", &json!([]), 1),
        plain_run(
            "run-b-small",
            &json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}]),
            3,
        ),
        big,
        plain_run("run-d-shares", &conversation("big"), 9),
    ];
    let conn = Connection::open(&path).unwrap();
    for run in &runs {
        insert(&conn, run);
    }
    drop(conn);
    (dir, path, runs)
}

pub(super) fn apply(path: &Path) -> Result<ConvertBodiesSummary, ConvertBodiesFailure> {
    convert_bodies(
        path,
        &ConvertBodiesOptions {
            apply: true,
            backup: None,
            ..Default::default()
        },
    )
}

pub(super) fn preview(path: &Path) -> Result<ConvertBodiesSummary, ConvertBodiesFailure> {
    convert_bodies(path, &ConvertBodiesOptions::default())
}

fn columns(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    conn.prepare("SELECT name FROM pragma_table_info('run_contexts')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

pub(super) fn version(path: &Path) -> u32 {
    Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn read(conn: &Connection, id: &str) -> crate::RunContextRecord {
    run_messages::get(conn, id).unwrap().unwrap()
}

pub(super) fn files(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(path.parent().unwrap()).unwrap() {
        let entry = entry.unwrap().path();
        if entry.is_file() {
            out.push((
                entry.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(&entry).unwrap(),
            ));
        }
    }
    out.sort();
    out
}

fn single(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[test]
fn every_run_reads_back_as_the_bytes_the_old_column_held() {
    // Given: a version 39 database with an empty run, a small one, and two that share bodies.
    let (_dir, path, runs) = seeded();

    // When: it is converted.
    let summary = apply(&path).unwrap();

    // Then: the format is current and every run reads back byte for byte.
    assert_eq!(summary.converted_runs, 4);
    assert_eq!(
        (summary.source_schema, summary.target_schema),
        (39, latest_schema())
    );
    assert_eq!(summary.unconverted_runs_remaining, 0);
    assert_eq!(summary.mismatched_runs, 0);
    assert_eq!(summary.events_tokenized, 0);
    assert!(summary.checkpoint_truncated && summary.vacuum);
    assert_eq!(version(&path), latest_schema());
    assert!(!columns(&path).contains(&"messages_json".to_owned()));
    let conn = Connection::open(&path).unwrap();
    for run in &runs {
        let record = read(&conn, run.id);
        assert_eq!(record.messages_json, run.messages, "{}", run.id);
        assert_eq!(record.checkpoints_json, run.checkpoints, "{}", run.id);
        let config: Value = serde_json::from_str(&record.config_json).unwrap();
        assert_eq!(config, serde_json::from_str::<Value>(&run.config).unwrap());
        assert_eq!(record.host_inputs_json, "[\"input-1\"]");
        let (revision, updated, header_rev): (i64, i64, i64) = conn
            .query_row(
                "SELECT snapshot_revision, updated_at_ns, header_rev FROM run_contexts \
                 WHERE run_id = ?1",
                [run.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (revision, updated, header_rev),
            (run.revision, run.updated, 1)
        );
    }
    // The run that persist_failed marked keeps its projection.
    let reason: Option<String> = conn
        .query_row(
            "SELECT non_restorable_reason FROM run_contexts WHERE run_id = 'run-big'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reason.as_deref(), Some("persist_failed"));
    // The body shared by two runs is one blob.
    assert!(summary.blobs_created > 0 && summary.references_created > summary.blobs_created);
    // The verified backup is the old format, untouched.
    let backup = summary.backup.unwrap();
    assert!(columns(&backup).contains(&"messages_json".to_owned()));
    assert_eq!(version(&backup), 39);
    // A normal open accepts the result.
    Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
}

#[test]
fn a_converted_database_is_left_alone_by_every_further_run() {
    // Given: a converted database.
    let (_dir, path, _) = seeded();
    apply(&path).unwrap();
    let before = files(&path);

    // When/Then: preview and apply both report it, and no byte of any file changes.
    for run in [preview(&path), apply(&path)] {
        let summary = run.unwrap();
        assert!(summary.already_converted && summary.backup.is_none());
        assert_eq!(summary.converted_runs, 0);
    }
    assert_eq!(files(&path), before);
}

#[test]
fn preview_reports_what_would_happen_and_writes_nothing() {
    // Given: a version 39 database.
    let (_dir, path, _) = seeded();
    let before = files(&path);

    // When: previewed.
    let summary = preview(&path).unwrap();

    // Then: it reports the measurements, and no file changed or appeared.
    assert!(summary.preview && summary.backup.is_none());
    let analysis = summary.analysis.unwrap();
    assert_eq!(analysis.convertible_runs, 4);
    assert_eq!(analysis.unconvertible_runs, 0);
    assert_eq!(analysis.messages, 1 + 8 + 8);
    assert!(analysis.estimated_blob_candidates > analysis.estimated_distinct_blobs);
    assert!(analysis.old_payload_plain_bytes > 0);
    assert_eq!(files(&path), before);
}

#[test]
fn preview_without_the_lease_file_is_refused_and_creates_nothing() {
    // Given: a database without its lease file.
    let (_dir, path, _) = seeded();
    fs::remove_file(offline::lease_path(&path).unwrap()).unwrap();
    let before = files(&path);

    // When/Then: preview refuses and leaves the directory as it was.
    let failure = preview(&path).unwrap_err();
    assert!(failure.detail.contains("lease file"), "{failure}");
    assert!(!failure.restore_required);
    assert_eq!(files(&path), before);
}

#[test]
fn a_non_empty_wal_is_refused_by_preview_and_apply_alike() {
    // Given: a database whose log holds a committed write (its connection is still open).
    let (_dir, path, _) = seeded();
    let writer = Connection::open(&path).unwrap();
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    writer
        .execute(
            "UPDATE run_contexts SET name = 'renamed' WHERE run_id = 'run-b-small'",
            [],
        )
        .unwrap();
    assert!(
        fs::metadata(format!("{}-wal", path.display()))
            .unwrap()
            .len()
            > 0
    );
    let before = files(&path);

    // When/Then: both refuse before SQLite opens anything, and nothing changes.
    for run in [preview(&path), apply(&path)] {
        let failure = run.unwrap_err();
        assert!(failure.detail.contains("non-empty WAL"), "{failure}");
        assert!(!failure.restore_required);
    }
    assert_eq!(files(&path), before);
}

#[test]
fn data_that_cannot_be_converted_is_refused_before_anything_is_changed() {
    // Given: a restorable run whose saved messages carry whitespace the serializer does
    // not produce.
    let (_dir, path, _) = seeded();
    let conn = Connection::open(&path).unwrap();
    insert(
        &conn,
        &Run {
            id: "run-c-spaced",
            messages: "[ { \"role\": \"user\" } ]".into(),
            checkpoints: "[]".into(),
            config: "{}".into(),
            revision: 2,
            updated: 5,
            restorable: true,
        },
    );
    drop(conn);
    let before = files(&path);

    // When: previewed and applied.
    let analysis = preview(&path).unwrap().analysis.unwrap();
    let failure = apply(&path).unwrap_err();

    // Then: preview lists the run, and apply refuses on the same finding before the
    // backup or any change: no file differs and no run is converted.
    assert_eq!(analysis.unconvertible_runs, 1);
    assert_eq!(analysis.problems[0].run_id, "run-c-spaced");
    assert_eq!(failure.stage, "analyze");
    assert!(failure.detail.contains("run-c-spaced"), "{failure}");
    assert_eq!(failure.converted_runs, 0);
    assert!(failure.backup.is_none() && !failure.restore_required);
    assert_eq!(files(&path), before);
}

#[test]
fn a_stopped_conversion_is_recovered_by_restoring_the_backup_and_starting_over() {
    // Given: a conversion stopped by a failure injected at each point in turn.
    let points = [
        Point::AfterPrepare,
        Point::AfterRetention,
        Point::Run(RunPoint::BeforeReadback),
        Point::Run(RunPoint::BeforeCommit),
        Point::AfterRun { frames: 0 },
        Point::BeforeVerify,
        Point::BeforeDrop,
        Point::AfterDrop,
        Point::BeforeVacuum,
        Point::BeforePublish,
    ];
    for point in points {
        let (_dir, path, runs) = seeded();
        let mut seen = 0;
        let failure = convert_with(
            &path,
            &ConvertBodiesOptions {
                apply: true,
                backup: None,
                ..Default::default()
            },
            &mut |at, _| {
                if std::mem::discriminant(&at) == std::mem::discriminant(&point)
                    && (matches!(at, Point::Run(_)) == matches!(point, Point::Run(_)))
                    && (!matches!(at, Point::Run(_)) || at == point)
                {
                    seen += 1;
                    if seen == 2 || !matches!(point, Point::Run(_) | Point::AfterRun { .. }) {
                        return Err(StorageError::Serialization("injected".into()));
                    }
                }
                Ok(())
            },
        )
        .unwrap_err();

        // Then: the failure says where, keeps the backup, and the database is not usable.
        assert!(failure.detail.contains("injected"), "{point:?}: {failure}");
        assert!(failure.restore_required, "{point:?}");
        let guidance = failure.to_string();
        assert!(
            guidance.contains("the database was changed; do not return it to normal use"),
            "{point:?}: {guidance}"
        );
        assert!(
            guidance.contains("recover only from the verified backup made before this conversion"),
            "{point:?}: {guidance}"
        );
        assert!(
            guidance.contains("Recovery (there is no resume)"),
            "{point:?}: {guidance}"
        );
        assert!(
            !guidance.contains("a normal start and this command refuse it"),
            "{point:?}: {guidance}"
        );
        assert_eq!(version(&path), 39, "{point:?}: not published");
        let backup = failure.backup.clone().unwrap();
        assert!(
            Database::open(&StorageConfig {
                db_path: path.clone(),
                ..Default::default()
            })
            .is_err()
        );
        // The command does not guess where to continue either.
        let again = apply(&path).unwrap_err();
        assert!(
            again.detail.contains("did not finish"),
            "{point:?}: {again}"
        );

        // When: the backup is restored over the database (no stale log) and converted again.
        for suffix in ["-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", path.display()));
        }
        fs::copy(&backup, &path).unwrap();
        let summary = apply(&path).unwrap();

        // Then: it succeeds from the start, with every run intact.
        assert_eq!(summary.converted_runs, 4, "{point:?}");
        let conn = Connection::open(&path).unwrap();
        for run in &runs {
            assert_eq!(read(&conn, run.id).messages_json, run.messages, "{point:?}");
        }
    }
}

#[test]
fn a_migration_failure_always_directs_recovery_to_the_starting_backup() {
    let (_dir, path, _) = seeded_at(33);
    let failure = convert_with(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            backup: None,
            ..Default::default()
        },
        &mut |point, _| {
            if point == Point::AfterMigrate {
                return Err(StorageError::Serialization(
                    "injected migration failure".into(),
                ));
            }
            Ok(())
        },
    )
    .unwrap_err();

    assert_eq!(failure.stage, "migrate");
    assert!(failure.restore_required);
    assert_eq!(version(&path), 39);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(classify(&conn).unwrap(), Shape::Source);

    let guidance = failure.to_string();
    assert!(guidance.contains("verified backup made before this conversion"));
    assert!(guidance.contains("do not return it to normal use"));
    assert!(guidance.contains("use this changed database as the starting point for recovery"));
    assert!(!guidance.contains("a normal start and this command refuse it"));
}

#[test]
fn a_run_that_fails_leaves_nothing_of_itself_and_keeps_the_runs_before_it() {
    // Given: a failure at the commit of the third run.
    let (_dir, path, runs) = seeded();
    let mut seen = 0;
    let mut state: Option<(i64, i64)> = None;
    let failure = convert_with(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            backup: None,
            ..Default::default()
        },
        &mut |at, conn| {
            match at {
                Point::Run(RunPoint::BeforeCommit) => {
                    seen += 1;
                    if seen == 3 {
                        return Err(StorageError::Serialization("injected".into()));
                    }
                }
                Point::AfterRun { .. } if seen == 2 => {
                    state = Some((
                        single(conn, "SELECT COUNT(*) FROM run_messages"),
                        single(conn, "SELECT COUNT(*) FROM content_blobs"),
                    ));
                }
                _ => {}
            }
            Ok(())
        },
    )
    .unwrap_err();

    // Then: the stored counts are those after the second run, and the third is unchanged.
    assert_eq!(failure.run_id.as_deref(), Some("run-big"));
    assert_eq!(failure.converted_runs, 2);
    let conn = Connection::open(&path).unwrap();
    let after = (
        single(&conn, "SELECT COUNT(*) FROM run_messages"),
        single(&conn, "SELECT COUNT(*) FROM content_blobs"),
    );
    assert_eq!(Some(after), state);
    let (messages, header_rev): (String, i64) = conn
        .query_row(
            "SELECT messages_json, header_rev FROM run_contexts WHERE run_id = 'run-big'",
            [],
            |row| Ok((codec::text_column(row, 0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(messages, runs[2].messages);
    assert_eq!(header_rev, 0);
}

#[test]
fn a_blob_damaged_before_the_read_back_is_found_and_the_run_is_not_committed() {
    // Given: a hook that damages the stored bytes of a blob inside the run's transaction.
    let (_dir, path, _) = seeded();
    let failure = convert_with(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            backup: None,
            ..Default::default()
        },
        &mut |at, conn| {
            if at == Point::Run(RunPoint::BeforeReadback) {
                conn.execute("UPDATE content_blobs SET data = x'00ff'", [])?;
            }
            Ok(())
        },
    )
    .unwrap_err();

    // Then: the read back reports a mismatch for the first run that has a blob.
    assert_eq!(failure.reason, "mismatch");
    assert_eq!(failure.run_id.as_deref(), Some("run-big"));
    assert_eq!(failure.converted_runs, 2);
}

#[test]
fn foreign_keys_stay_enabled_from_the_first_transaction_to_the_last() {
    // Given/When: a conversion that records the pragma at every point.
    let (_dir, path, _) = seeded();
    let mut checked = 0;
    convert_with(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            backup: None,
            ..Default::default()
        },
        &mut |_, conn| {
            assert_eq!(single(conn, "PRAGMA foreign_keys"), 1);
            checked += 1;
            Ok(())
        },
    )
    .unwrap();

    // Then: it was on at every point.
    assert!(checked > 10);
}

#[test]
fn rows_of_deleted_runs_are_reclaimed_first_and_never_converted() {
    // Given: a deleted run (tombstoned, a retention job pending) whose header holds a
    // five-megabyte conversation, next to a kept run.
    let (_dir, path, runs) = seeded();
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json, \
         checkpoints_json, terminal_phase, restorable, updated_at_ns, host_inputs_json, \
         snapshot_revision, messages_len, checkpoints_len) \
         VALUES ('run-deleted', 'worker', 'n', NULL, '{}', ?1, '[]', 'Done', 1, 1, '[]', 1, ?2, 2)",
        params![
            format!("[\"{}\"]", "x".repeat(5 * MIB)),
            (5 * MIB + 4) as i64
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES ('run-deleted', 't', 'fam')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO reclaim_jobs(family_key, state, created_at_ns, updated_at_ns) \
         VALUES ('fam', 'db_pending', 1, 1)",
        [],
    )
    .unwrap();
    drop(conn);
    let analysis = preview(&path).unwrap().analysis.unwrap();
    assert_eq!(
        (analysis.convertible_runs, analysis.deleted_runs_to_reclaim),
        (4, 1)
    );

    // When: converted.
    let summary = apply(&path).unwrap();

    // Then: the deleted run is reported as reclaimed, with the old payload counted in
    // bytes, and is not converted; the kept runs are converted and intact.
    assert_eq!(summary.converted_runs, 4);
    assert_eq!(summary.retention.reclaimed_runs, 1);
    assert!(summary.retention.reclaimed_bytes >= 5 * MIB as u64);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        single(
            &conn,
            "SELECT COUNT(*) FROM run_contexts WHERE run_id = 'run-deleted'"
        ),
        0
    );
    assert_eq!(single(&conn, "SELECT COUNT(*) FROM run_tombstones"), 1);
    for run in &runs {
        assert_eq!(read(&conn, run.id).messages_json, run.messages);
    }
}

#[test]
fn a_deleted_thread_of_a_pending_job_is_reclaimed_before_the_lane_tables_exist() {
    // Given: a version 39 database whose retention job for a deleted family (a thread
    // with a start state, and a run) has not been worked off yet.
    let (_dir, path, runs) = seeded();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "INSERT INTO threads(thread_id, root_run_id, created_at_ns, updated_at_ns, source) \
             VALUES ('t-deleted', 'run-deleted', 1, 1, 'host');
         INSERT INTO thread_start_states(thread_id, root_run_id, project_root, captured_at_ns, replayable, reason) \
             VALUES ('t-deleted', 'run-deleted', '/p', 1, 0, 'no_commit');
         INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json, \
             checkpoints_json, terminal_phase, restorable, updated_at_ns, host_inputs_json, \
             snapshot_revision, messages_len, checkpoints_len) \
             VALUES ('run-deleted', 'worker', 'n', NULL, '{}', '[]', '[]', 'Done', 1, 1, '[]', 1, 2, 2);
         INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) VALUES ('t-deleted', 'fam', 1);
         INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES ('run-deleted', 't-deleted', 'fam');
         INSERT INTO reclaim_jobs(family_key, state, created_at_ns, updated_at_ns) \
             VALUES ('fam', 'db_pending', 1, 1);
         DELETE FROM thread_tombstones WHERE 0;",
    )
    .unwrap();
    assert_eq!(
        single(
            &conn,
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'lanes'"
        ),
        0
    );
    drop(conn);

    // When: it is converted (the reclaim runs while the database is at version 40).
    let summary = apply(&path).unwrap();

    // Then: the thread's rows went with the family without touching a lane table, the
    // kept runs are converted, and the finished database has the lane ledger.
    assert_eq!(summary.converted_runs, 4);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        single(
            &conn,
            "SELECT COUNT(*) FROM thread_start_states WHERE thread_id = 't-deleted'"
        ),
        0
    );
    assert_eq!(
        single(
            &conn,
            "SELECT COUNT(*) FROM run_contexts WHERE run_id = 'run-deleted'"
        ),
        0
    );
    assert_eq!(
        single(
            &conn,
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'lanes'"
        ),
        1
    );
    assert_eq!(version(&path), latest_schema());
    for run in &runs {
        assert_eq!(read(&conn, run.id).messages_json, run.messages);
    }
}

#[test]
fn a_deleted_run_without_a_reclaim_job_stops_the_conversion_before_any_run() {
    // Given: a tombstone whose run still has rows and no job to reclaim them.
    let (_dir, path, _) = seeded();
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES ('run-b-small', 't', 'fam')",
        [],
    )
    .unwrap();
    drop(conn);

    // When/Then: preview names the blocker and apply refuses before the backup, with
    // no run converted and no file changed.
    let before = files(&path);
    let analysis = preview(&path).unwrap().analysis.unwrap();
    assert_eq!(analysis.reclaim_blockers.len(), 1);
    let failure = apply(&path).unwrap_err();
    assert_eq!(failure.stage, "analyze");
    assert_eq!(failure.converted_runs, 0);
    assert!(failure.backup.is_none() && !failure.restore_required);
    assert!(failure.detail.contains("no reclaim job"), "{failure}");
    assert_eq!(files(&path), before);
}

#[test]
fn a_reclaim_cursor_this_build_does_not_read_is_refused_not_started_over() {
    for cursor in ["{not json", "{\"phase\": \"no_such_phase\"}", "[]"] {
        // Given: a deleted run whose pending job holds a cursor the reclaim cannot read.
        let (_dir, path, _) = seeded();
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO run_tombstones(run_id, thread_id, family_key) \
             VALUES ('run-b-small', 't', 'fam')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO reclaim_jobs(family_key, state, cursor_json, created_at_ns, updated_at_ns) \
             VALUES ('fam', 'db_pending', ?1, 1, 1)",
            [cursor],
        )
        .unwrap();
        drop(conn);
        let before = files(&path);

        // When/Then: preview reports it, apply refuses with no run converted and no
        // change, and the job's cursor is not rewritten.
        let analysis = preview(&path).unwrap().analysis.unwrap();
        assert!(
            analysis.reclaim_blockers[0].contains("cursor"),
            "{cursor}: {:?}",
            analysis.reclaim_blockers
        );
        let failure = apply(&path).unwrap_err();
        assert_eq!(failure.stage, "analyze", "{cursor}");
        assert_eq!(failure.converted_runs, 0);
        assert!(!failure.restore_required);
        assert_eq!(files(&path), before, "{cursor}");
    }
}

/// Runs of the log-size test: fillers that make the B-trees large, then one run of each
/// kind the contract names. Run ids sort in this order, which is the conversion order.
fn io_runs() -> Vec<Run> {
    let message =
        |text: String| json!({"role": "assistant", "content": [{"type": "text", "text": text}]});
    let mut runs = Vec::new();
    for index in 0..300 {
        let id: &'static str = Box::leak(format!("a-fill-{index:03}").into_boxed_str());
        let messages: Vec<Value> = (0..3)
            .map(|n| message(format!("filler {index} message {n} with some words")))
            .collect();
        runs.push(plain_run(id, &Value::Array(messages), 1));
    }
    runs.push(plain_run("z1-small", &json!([message("hi".into())]), 1));
    let short: Vec<Value> = (0..2_500)
        .map(|n| message(format!("short message {n}")))
        .collect();
    runs.push(plain_run("z2-many-short", &Value::Array(short), 1));
    let with_refs: Vec<Value> = (0..100)
        .map(|n| {
            json!({"role": "assistant", "content": (0..3)
                .map(|k| json!({"type": "text", "text": long(&format!("body {n}-{k}"), 2_100)}))
                .collect::<Vec<_>>()})
        })
        .collect();
    runs.push(plain_run(
        "z3-many-refs",
        &Value::Array(with_refs.clone()),
        1,
    ));
    runs.push(plain_run("z4-reuses-blobs", &Value::Array(with_refs), 1));
    runs.push(plain_run(
        "z5-one-big",
        &json!([message(long("big body", 300_000))]),
        1,
    ));
    runs
}

#[test]
fn one_run_writes_a_log_in_proportion_to_what_it_stores() {
    // Given: a database of 300 small runs and one run of each kind the contract names
    // (small, many short messages, many references, bodies shared with an earlier run,
    // one large body).
    let dir = tempfile::tempdir().unwrap();
    let path = v39(dir.path());
    let runs = io_runs();
    let conn = Connection::open(&path).unwrap();
    for run in &runs {
        insert(&conn, run);
    }
    let page: u64 = conn
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    drop(conn);
    let old_stored: Vec<u64> = runs
        .iter()
        .map(|run| {
            (codec::encode(&run.messages).stored_len()
                + codec::encode(&run.checkpoints).stored_len()) as u64
        })
        .collect();

    // When: converted, with the figures of each table read after every run commits.
    let totals = |conn: &Connection| -> [u64; 4] {
        let sum = |sql: &str| u64::try_from(single(conn, sql)).unwrap();
        [
            sum("SELECT COALESCE(SUM(OCTET_LENGTH(data)), 0) FROM content_blobs"),
            sum("SELECT COALESCE(SUM(OCTET_LENGTH(skeleton)), 0) FROM run_messages"),
            sum("SELECT COUNT(*) FROM run_messages"),
            sum("SELECT (SELECT COUNT(*) FROM content_blob_refs_message) \
                 + (SELECT COUNT(*) FROM content_blob_refs_header)"),
        ]
    };
    let mut seen: Vec<(u64, [u64; 4])> = Vec::new();
    let summary = convert_with(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            backup: None,
            ..Default::default()
        },
        &mut |point, conn| {
            if let Point::AfterRun { frames } = point {
                seen.push((frames, totals(conn)));
            }
            Ok(())
        },
    )
    .unwrap();

    // Then: every run's transaction wrote at most ceil(2 * S / page) + 256 frames, with S
    // the bytes it changed: new blobs, skeletons, rows and references (a generous
    // allowance each) and the old payload it removed.
    assert_eq!(seen.len(), runs.len());
    let mut previous = [0_u64; 4];
    for (index, (frames, now)) in seen.iter().enumerate() {
        let delta = |slot: usize| now[slot] - previous[slot];
        let changed = delta(0) + delta(1) + 160 * (delta(2) + delta(3)) + old_stored[index] + 2_048;
        let budget = (2 * changed).div_ceil(page) + 256;
        assert!(
            *frames <= budget,
            "run {} wrote {frames} frames, the budget is {budget} (S = {changed})",
            runs[index].id
        );
        if runs[index].id == "z4-reuses-blobs" {
            assert_eq!(delta(0), 0, "a run that shares every body stores no blob");
            assert!(delta(3) >= 300, "but it still takes its own references");
        }
        previous = *now;
    }
    assert_eq!(
        summary.peak_run_wal_frames,
        seen.iter().map(|(frames, _)| *frames).max().unwrap()
    );
}

#[test]
fn another_holder_of_the_lease_or_of_the_database_lock_stops_apply_unchanged() {
    // Given: a database whose maintenance lease another process holds.
    let (_dir, path, _) = seeded();
    let before = files(&path);
    let holder = offline::lease_file(&path).unwrap();
    holder.try_lock().unwrap();

    // When/Then: apply is refused without touching anything.
    let failure = apply(&path).unwrap_err();
    assert!(failure.detail.contains("in use"), "{failure}");
    assert!(!failure.restore_required);
    drop(holder);
    assert_eq!(files(&path), before);

    // Given: a writer that ignores the lease and holds SQLite's write lock.
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();

    // When/Then: the exclusive lock is refused before any backup or change.
    let failure = apply(&path).unwrap_err();
    assert!(
        failure.detail.contains("exclusive SQLite lock"),
        "{failure}"
    );
    assert!(failure.backup.is_none() && !failure.restore_required);
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn an_existing_backup_path_is_never_overwritten() {
    // Given: a file where the backup would go.
    let (dir, path, _) = seeded();
    let target = dir.path().join("taken.db");
    fs::write(&target, b"precious").unwrap();

    // When/Then: apply stops at the backup, the file and the database are as they were.
    let failure = convert_bodies(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            backup: Some(target.clone()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(failure.stage, "backup");
    assert!(!failure.restore_required);
    assert_eq!(fs::read(&target).unwrap(), b"precious");
    assert_eq!(version(&path), 39);
    assert!(columns(&path).contains(&"messages_json".to_owned()));
}

const OLD_KEY_ORDER: &str = r#"[{"role":"user","content":[{"type":"text","text":"hi"}]}]"#;
const CHECKPOINT_OLD_KEY_ORDER: &str = r#"[{"summary":"s","end":1}]"#;

fn old_key_order_run(id: &'static str, restorable: bool) -> Run {
    Run {
        id,
        messages: OLD_KEY_ORDER.into(),
        checkpoints: CHECKPOINT_OLD_KEY_ORDER.into(),
        config: "{}".into(),
        revision: 0,
        updated: 77,
        restorable,
    }
}

#[test]
fn a_run_that_cannot_be_restored_is_stored_in_the_serializers_key_order() {
    // Given: runs saved before the serializer ordered keys (role first), one of them not
    // restorable and one restorable but already in the serializer's order.
    let (_dir, path, runs) = seeded();
    let old = old_key_order_run("run-old-order", false);
    let conn = Connection::open(&path).unwrap();
    insert(&conn, &old);
    drop(conn);

    // When: previewed and converted.
    let analysis = preview(&path).unwrap().analysis.unwrap();
    let summary = apply(&path).unwrap();

    // Then: it is counted as canonicalizable, then stored as the same JSON value in the
    // serializer's order, and named in the report; the other runs keep their exact bytes.
    assert_eq!(
        (analysis.canonicalizable_runs, analysis.unconvertible_runs),
        (1, 0)
    );
    assert_eq!(summary.canonicalized_runs.count, 1);
    assert_eq!(summary.canonicalized_runs.run_ids, ["run-old-order"]);
    let conn = Connection::open(&path).unwrap();
    let record = read(&conn, "run-old-order");
    assert_ne!(record.messages_json, old.messages);
    assert_eq!(
        serde_json::from_str::<Value>(&record.messages_json).unwrap(),
        serde_json::from_str::<Value>(&old.messages).unwrap()
    );
    assert_eq!(
        serde_json::from_str::<Value>(&record.checkpoints_json).unwrap(),
        serde_json::from_str::<Value>(&old.checkpoints).unwrap()
    );
    assert_eq!(
        record.messages_json,
        r#"[{"content":[{"text":"hi","type":"text"}],"role":"user"}]"#
    );
    let (revision, updated): (i64, i64) = conn
        .query_row(
            "SELECT snapshot_revision, updated_at_ns FROM run_contexts WHERE run_id = 'run-old-order'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((revision, updated), (0, 77));
    for run in &runs {
        assert_eq!(read(&conn, run.id).messages_json, run.messages);
    }
}

#[test]
fn a_restorable_run_that_is_not_in_the_serializers_form_stops_the_conversion() {
    // Given: a restorable run whose old bytes have the old key order.
    let (_dir, path, _) = seeded();
    let conn = Connection::open(&path).unwrap();
    insert(&conn, &old_key_order_run("run-old-order", true));
    drop(conn);

    // When: previewed and applied.
    let analysis = preview(&path).unwrap().analysis.unwrap();
    let failure = apply(&path).unwrap_err();

    // Then: both refuse it; apply does so before anything changes and names the run.
    assert_eq!(
        (analysis.canonicalizable_runs, analysis.unconvertible_runs),
        (0, 1)
    );
    assert_eq!(failure.stage, "analyze");
    assert!(!failure.restore_required);
    assert!(failure.detail.contains("run-old-order"), "{failure}");
    assert!(failure.detail.contains("restorable"), "{failure}");
}

struct LimitGuard;

impl LimitGuard {
    fn new(limit: u64) -> Self {
        source::limit_override::set(Some(limit));
        Self
    }
}

impl Drop for LimitGuard {
    fn drop(&mut self) {
        source::limit_override::set(None);
    }
}

fn insert_raw(
    conn: &Connection,
    id: &str,
    messages: SqlValue,
    messages_len: i64,
    restorable: bool,
) {
    conn.execute(
        "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json, \
         checkpoints_json, terminal_phase, restorable, updated_at_ns, host_inputs_json, \
         snapshot_revision, messages_len, checkpoints_len) \
         VALUES (?1, 'worker', 'n', NULL, '{}', ?2, '[]', 'Done', ?3, 1, '[]', 1, ?4, 2)",
        params![id, messages, restorable, messages_len],
    )
    .unwrap();
}

#[test]
fn a_run_recorded_above_the_limit_is_reported_without_being_read() {
    // Given: a limit of 64 KiB and a run whose length columns say it is far above it; its
    // stored bytes are not a valid frame, so reading it would be a finding of its own.
    let _limit = LimitGuard::new(64 * 1024);
    let (_dir, path, _) = seeded();
    let conn = Connection::open(&path).unwrap();
    insert_raw(
        &conn,
        "run-huge",
        SqlValue::Blob(vec![1, 2, 3]),
        200_000,
        true,
    );
    drop(conn);
    let before = files(&path);

    // When: previewed and applied.
    let analysis = preview(&path).unwrap().analysis.unwrap();
    let failure = apply(&path).unwrap_err();

    // Then: it is counted as oversized and never decoded; apply refuses unchanged.
    assert_eq!(analysis.oversized_run_count, 1);
    assert_eq!(analysis.oversized_runs, ["run-huge"]);
    assert_eq!(analysis.unconvertible_runs, 0);
    assert_eq!(failure.stage, "analyze");
    assert!(failure.detail.contains("run-huge"), "{failure}");
    assert!(!failure.restore_required);
    assert_eq!(files(&path), before);
}

#[test]
fn a_frame_that_lies_about_its_size_is_stopped_while_it_decompresses() {
    // Given: a limit of 64 KiB and runs whose length columns are small but whose frames
    // expand past it: one declares its size, one (streamed) declares none.
    let _limit = LimitGuard::new(64 * 1024);
    let plain = vec![b'a'; MIB];
    let declared = zstd::bulk::compress(&plain, 3).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    std::io::Write::write_all(&mut encoder, &plain).unwrap();
    let undeclared = encoder.finish().unwrap();
    assert!(
        zstd::zstd_safe::get_frame_content_size(&undeclared)
            .unwrap()
            .is_none()
    );
    for (name, frame) in [("declared", declared), ("undeclared", undeclared)] {
        let (_dir, path, _) = seeded();
        let conn = Connection::open(&path).unwrap();
        insert_raw(&conn, "run-liar", SqlValue::Blob(frame), 10, true);
        drop(conn);
        let before = files(&path);

        // When/Then: preview reports it unconvertible, and apply refuses unchanged.
        let analysis = preview(&path).unwrap().analysis.unwrap();
        assert_eq!(analysis.unconvertible_runs, 1, "{name}");
        assert!(
            analysis.problems[0].reason.contains("larger than"),
            "{name}"
        );
        let failure = apply(&path).unwrap_err();
        assert_eq!(failure.stage, "analyze", "{name}");
        assert!(!failure.restore_required, "{name}");
        assert_eq!(files(&path), before, "{name}");
    }
}

#[test]
fn the_decompression_limit_is_exact() {
    // Given: a frame of 1,000 bytes without a declared size, and a text of 1,000 bytes.
    let plain = vec![b'x'; 1_000];
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    std::io::Write::write_all(&mut encoder, &plain).unwrap();
    let frame = encoder.finish().unwrap();
    let text = "y".repeat(1_000);

    // When/Then: exactly the limit is read; one byte less is refused; nothing is guessed.
    use rusqlite::types::ValueRef;
    assert_eq!(
        source::decode_bounded(ValueRef::Blob(&frame), 1_000)
            .unwrap()
            .len(),
        1_000
    );
    assert!(source::decode_bounded(ValueRef::Blob(&frame), 999).is_err());
    assert_eq!(
        source::decode_bounded(ValueRef::Text(text.as_bytes()), 1_000).unwrap(),
        text
    );
    assert!(source::decode_bounded(ValueRef::Text(text.as_bytes()), 999).is_err());
    assert!(source::decode_bounded(ValueRef::Blob(b"not a frame"), 1_000).is_err());
}

#[test]
fn the_set_of_distinct_bodies_stops_growing_at_its_cap_and_the_estimate_stays_an_upper_bound() {
    // Given: the seeded runs analyzed once without and once with a cap of one body.
    let (_dir, path, _) = seeded();
    let conn = Connection::open(&path).unwrap();
    let full = source::analyze_with_cap(&conn, usize::MAX).unwrap();
    let capped = source::analyze_with_cap(&conn, 1).unwrap();

    // Then: the capped pass says its distinct count is a lower bound; what bounds the
    // growth (every candidate's bytes) does not depend on the cap.
    assert!(!full.distinct_tracking_capped && full.estimated_distinct_blobs > 1);
    assert!(capped.distinct_tracking_capped);
    assert_eq!(capped.estimated_distinct_blobs, 1);
    assert_eq!(
        capped.estimated_candidate_plain_bytes,
        full.estimated_candidate_plain_bytes
    );
    assert_eq!(capped.estimated_growth_bytes, full.estimated_growth_bytes);
    assert!(full.estimated_candidate_plain_bytes >= full.estimated_blob_plain_bytes);
}

#[test]
fn free_space_is_planned_per_volume_from_the_growth_and_checked_before_any_change() {
    use std::path::Path;
    let place = |directory: &'static str, volume| space::Place {
        directory: Path::new(directory),
        volume,
    };

    // Given/When: one volume holding everything, and the three on separate volumes.
    let together = space::plan(
        &place("/db", 1),
        &place("/db", 1),
        &place("/db", 1),
        100,
        1_000,
    );
    let apart = space::plan(
        &place("/db", 1),
        &place("/bk", 2),
        &place("/tmp", 3),
        100,
        1_000,
    );

    // Then: one volume sums growth + log (1,000 + 1,100), the backup (100) and the
    // scratch file (1,100); apart, each volume needs only its own part, and the volumes
    // of the database and the backup keep the old floor of four databases.
    assert_eq!(together.len(), 1);
    assert_eq!(together[0].bytes, 3_300);
    let bytes: Vec<u64> = apart.iter().map(|need| need.bytes).collect();
    assert_eq!(bytes, [2_100, 400, 1_100]);
    assert_eq!(apart[2].purposes, ["SQLite scratch of the VACUUM"]);
    let vacuum_together = space::plan_vacuum(&place("/db", 1), &place("/db", 1), 500);
    let vacuum_apart = space::plan_vacuum(&place("/db", 1), &place("/tmp", 3), 500);
    assert_eq!(
        vacuum_together
            .iter()
            .map(|need| need.bytes)
            .collect::<Vec<_>>(),
        [1_000]
    );
    assert_eq!(
        vacuum_apart
            .iter()
            .map(|need| need.bytes)
            .collect::<Vec<_>>(),
        [500, 500]
    );

    // Given: a database and free space reported as one byte.
    let (_dir, path, _) = seeded();
    let before = files(&path);
    space::tests_override::set(Some(1));
    let preview_summary = preview(&path).unwrap();
    let failure = apply(&path).unwrap_err();
    space::tests_override::set(None);

    // Then: preview reports what each volume needs, and apply refuses before the backup
    // with nothing changed.
    assert!(preview_summary.estimated_growth_bytes > 0);
    assert!(!preview_summary.space_requirements.is_empty());
    assert_eq!(
        preview_summary.required_free_space_bytes,
        preview_summary
            .space_requirements
            .iter()
            .map(|need| need.bytes)
            .max()
            .unwrap()
    );
    assert_eq!(failure.stage, "analyze");
    assert!(
        failure.detail.contains("insufficient free space"),
        "{failure}"
    );
    assert!(failure.backup.is_none() && !failure.restore_required);
    assert_eq!(files(&path), before);
}

#[test]
fn a_database_the_running_build_wrote_converts_after_it_is_taken_back_to_version_39() {
    // Given: runs written through the current writer (long bodies, a shared body, an
    // empty run), then rewritten in the version 39 shape.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    let mut records = Vec::new();
    for (index, id) in ["run-1", "run-2", "run-3"].into_iter().enumerate() {
        let mut record = run_messages::fixtures::record(id);
        if index > 0 {
            record.messages_json = serde_json::to_string(&conversation("shared")).unwrap();
            record.checkpoints_json = serde_json::to_string(
                &json!([{"summary": {"role": "assistant", "content": [{"type": "text", "text": long("s", 3_000)}]}, "end": 2}]),
            )
            .unwrap();
        }
        run_messages::fixtures::insert(&db.conn, &record, 3);
        records.push(record);
    }
    drop(db);
    super::testing::downgrade_to_v39(&path).unwrap();
    lease(&path);
    assert_eq!(version(&path), 39);
    assert!(columns(&path).contains(&"messages_json".to_owned()));

    // When: converted.
    let summary = apply(&path).unwrap();

    // Then: the schema is that of a new database (the comparison inside the command
    // passed) and every run reads back as the runtime saved it.
    assert_eq!(summary.converted_runs, 3);
    assert_eq!(summary.canonicalized_runs.count, 0);
    let conn = Connection::open(&path).unwrap();
    for record in &records {
        let read = read(&conn, &record.run_id);
        assert_eq!(read.messages_json, record.messages_json);
        assert_eq!(read.checkpoints_json, record.checkpoints_json);
    }
}

/// Characters that zstd shrinks little (six random bits each), deterministic per seed.
fn noise(seed: u64, len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ALPHABET[(x >> 58) as usize] as char
        })
        .collect()
}

/// Runs whose growth is not in their blobs, one fixture per kind the review named.
fn growth_fixtures() -> Vec<(&'static str, Run)> {
    // Many messages repeating one text just under the token threshold: no blob at all, and
    // the old column compresses the repetition away while every skeleton keeps its copy.
    let repeated = noise(1, 1_900);
    let messages: Vec<Value> = (0..300)
        .map(|_| json!({"role": "assistant", "content": [{"type": "text", "text": repeated}]}))
        .collect();
    let duplicated = plain_run("run-duplicated", &Value::Array(messages), 1);

    // Large JSON made of short values and numbers.
    let messages: Vec<Value> = (0..6u64)
        .map(|message| {
            let values: Vec<f64> = (0..4_000u64)
                .map(|n| ((message * 7_919 + n * 104_729) % 1_000_003) as f64 / 8.0)
                .collect();
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t", "name": "shell", "input": {"values": values}}
            ]})
        })
        .collect();
    let numbers = plain_run("run-numbers", &Value::Array(messages), 1);

    // A configuration and checkpoint list that are large without any long string.
    let mut header = plain_run("run-header", &json!([]), 1);
    let config: serde_json::Map<String, Value> = (0..2_000u64)
        .map(|n| (format!("setting_{n}"), Value::String(noise(n, 120))))
        .collect();
    header.config = serde_json::to_string(&config).unwrap();
    let checkpoints: Vec<Value> = (0..400u64)
        .map(|n| json!({"summary": noise(n + 9, 1_500), "end": n}))
        .collect();
    header.checkpoints = serde_json::to_string(&checkpoints).unwrap();

    vec![
        ("duplicated short text", duplicated),
        ("short values and numbers", numbers),
        ("large configuration and checkpoints", header),
    ]
}

#[test]
fn the_growth_estimate_is_not_below_what_the_conversion_adds() {
    for (name, run) in growth_fixtures() {
        // Given: a database with a small run and the fixture's run.
        let dir = tempfile::tempdir().unwrap();
        let path = v39(dir.path());
        let conn = Connection::open(&path).unwrap();
        insert(&conn, &plain_run("run-0-small", &json!([]), 1));
        insert(&conn, &run);
        drop(conn);
        let basis = fs::metadata(&path).unwrap().len();
        let analysis = preview(&path).unwrap().analysis.unwrap();

        // When: converted, with the size the file has when the rows are converted and
        // before the VACUUM gives pages back.
        let mut before_vacuum = 0;
        convert_with(
            &path,
            &ConvertBodiesOptions {
                apply: true,
                backup: None,
                ..Default::default()
            },
            &mut |point, conn| {
                if point == Point::BeforeVacuum {
                    let page: u64 = single(conn, "PRAGMA page_size").try_into().unwrap();
                    let count: u64 = single(conn, "PRAGMA page_count").try_into().unwrap();
                    before_vacuum = page * count;
                }
                Ok(())
            },
        )
        .unwrap();

        // Then: the estimate covers what the file grew by, and each part of it covers what
        // was stored for that part (a skeleton or a header is stored compressed or as is,
        // never larger than its text).
        let conn = Connection::open(&path).unwrap();
        let stored = |sql: &str| u64::try_from(single(&conn, sql)).unwrap();
        let skeletons = stored("SELECT COALESCE(SUM(OCTET_LENGTH(skeleton)), 0) FROM run_messages");
        let blobs = stored("SELECT COALESCE(SUM(OCTET_LENGTH(data)), 0) FROM content_blobs");
        let headers = stored(
            "SELECT SUM(OCTET_LENGTH(config_json) + OCTET_LENGTH(checkpoints_json)) FROM run_contexts",
        );
        assert!(
            analysis.estimated_growth_bytes >= before_vacuum.saturating_sub(basis),
            "{name}: estimated {}, the file grew by {}",
            analysis.estimated_growth_bytes,
            before_vacuum.saturating_sub(basis)
        );
        assert!(
            analysis.estimated_growth_bytes >= skeletons + blobs + headers,
            "{name}: estimated {}, stored {}",
            analysis.estimated_growth_bytes,
            skeletons + blobs + headers
        );
        assert!(analysis.estimated_skeleton_bytes >= skeletons, "{name}");
        assert!(analysis.estimated_header_bytes >= headers, "{name}");
        // The estimate is what the old per-message allowance could not give: here the
        // skeletons and headers alone are more than 384 bytes per message and 2 KiB per run.
        let old_allowance = analysis.messages * 384 + analysis.convertible_runs * 2_048;
        assert!(
            skeletons + headers > old_allowance + analysis.estimated_blob_candidates * 160,
            "{name}: the fixture must be one the allowance alone would under-count"
        );
    }
}

#[test]
fn an_estimate_the_free_space_cannot_cover_refuses_before_any_change() {
    // Given: the fixture whose growth is all skeleton (no blob), and what preview says
    // every volume must have free.
    let (_, run) = growth_fixtures().remove(0);
    let dir = tempfile::tempdir().unwrap();
    let path = v39(dir.path());
    let conn = Connection::open(&path).unwrap();
    insert(&conn, &run);
    drop(conn);
    let summary = preview(&path).unwrap();
    let required = summary.required_free_space_bytes;
    let analysis = summary.analysis.unwrap();
    assert_eq!(analysis.estimated_blob_candidates, 0);
    assert!(analysis.estimated_skeleton_bytes >= 300 * 1_900);
    let before = files(&path);

    // When: one byte less than that is free.
    space::tests_override::set(Some(required - 1));
    let failure = apply(&path).unwrap_err();

    // Then: apply refuses before the backup, and the database is as it was.
    assert_eq!(failure.stage, "analyze");
    assert!(
        failure.detail.contains("insufficient free space"),
        "{failure}"
    );
    assert!(failure.backup.is_none() && !failure.restore_required);
    assert_eq!(files(&path), before);

    // When: exactly that much is free.
    space::tests_override::set(Some(required));
    let converted = apply(&path);
    space::tests_override::set(None);

    // Then: the conversion goes through, so the refusal came from the estimate alone.
    assert_eq!(converted.unwrap().converted_runs, 1);
}

#[test]
fn a_version_33_database_is_migrated_and_converted_by_the_one_command() {
    // Given: a database at version 33, as the user's is.
    let (_dir, path, runs) = seeded_at(33);
    let before = files(&path);

    // Then: a normal start refuses it without touching it (it does not migrate it first).
    let error = Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .err()
    .unwrap();
    assert!(
        matches!(error, StorageError::ConversionRequired { found: 33, .. }),
        "{error}"
    );
    assert_eq!(files(&path), before);

    // When: previewed (it reads the old schema as it is) and applied.
    let analysis = preview(&path).unwrap().analysis.unwrap();
    assert_eq!(
        (analysis.pending_migrations, analysis.convertible_runs),
        (6, 4)
    );
    assert_eq!(files(&path), before);
    let summary = apply(&path).unwrap();

    // Then: migrations 34 to 39 ran after the backup, every run converted, and the result
    // is a current database that a normal start accepts.
    assert_eq!(
        (summary.source_schema, summary.target_schema),
        (33, latest_schema())
    );
    assert_eq!(summary.migrations_applied, 6);
    assert_eq!(summary.converted_runs, 4);
    assert!(summary.stage_elapsed_seconds.contains_key("migrate"));
    assert_eq!(version(&path), latest_schema());
    let conn = Connection::open(&path).unwrap();
    for run in &runs {
        assert_eq!(
            read(&conn, run.id).messages_json,
            run.messages,
            "{}",
            run.id
        );
    }
    assert_eq!(single(&conn, "SELECT COUNT(*) FROM run_tombstones"), 0);
    drop(conn);
    Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    // The backup is the database as it was: version 33, the old column, no new table.
    let backup = summary.backup.unwrap();
    assert_eq!(version(&backup), 33);
    assert!(columns(&backup).contains(&"messages_json".to_owned()));
    let backup_tables: i64 = single(
        &Connection::open(&backup).unwrap(),
        "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('run_tombstones', 'content_blobs')",
    );
    assert_eq!(backup_tables, 0);
}

#[test]
fn a_stop_after_the_migrations_is_recovered_from_the_backup_like_any_other() {
    // Given: a version 33 database whose conversion is stopped right after migration 39.
    let (_dir, path, runs) = seeded_at(33);
    let failure = convert_with(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            backup: None,
            ..Default::default()
        },
        &mut |point, _| {
            if point == Point::AfterMigrate {
                return Err(StorageError::Serialization("injected".into()));
            }
            Ok(())
        },
    )
    .unwrap_err();

    // Then: the report says the database changed, the backup is the version 33 database,
    // and a normal start still refuses what is left.
    assert_eq!(failure.stage, "migrate");
    assert!(failure.restore_required);
    let backup = failure.backup.clone().unwrap();
    assert_eq!(version(&backup), 33);
    assert!(
        Database::open(&StorageConfig {
            db_path: path.clone(),
            ..Default::default()
        })
        .is_err()
    );

    // When: the backup is restored and the command run again.
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
    fs::copy(&backup, &path).unwrap();
    let summary = apply(&path).unwrap();

    // Then: it converts from the start.
    assert_eq!((summary.source_schema, summary.converted_runs), (33, 4));
    let conn = Connection::open(&path).unwrap();
    for run in &runs {
        assert_eq!(read(&conn, run.id).messages_json, run.messages);
    }
}

#[test]
fn an_older_database_and_unconvertible_data_in_a_version_33_one_are_refused_unchanged() {
    // Given: a database one version below the oldest the command takes.
    let (_dir, path, _) = seeded_at(33);
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "user_version", 32).unwrap();
    drop(conn);
    let before = files(&path);

    // When/Then: both modes refuse it and nothing changes.
    for run in [preview(&path), apply(&path)] {
        let failure = run.unwrap_err();
        assert!(failure.detail.contains("version 33 to 39"), "{failure}");
        assert!(!failure.restore_required);
    }
    assert_eq!(files(&path), before);

    // Given: a version 33 database with a restorable run that is not in the serializer's
    // form.
    let (_dir, path, _) = seeded_at(33);
    let conn = Connection::open(&path).unwrap();
    insert(&conn, &old_key_order_run("run-old-order", true));
    drop(conn);
    let before = files(&path);

    // When/Then: apply refuses on the analysis of the old schema, before the backup and
    // before any migration.
    let failure = apply(&path).unwrap_err();
    assert_eq!(failure.stage, "analyze");
    assert!(failure.backup.is_none() && !failure.restore_required);
    assert_eq!(files(&path), before);
}

// ---- Provenance maps (PR A2c) ----

use crate::thread_index::{
    BlockOrigin, BlockProvenance, CopySource, ExcludedReason, MessageProvenance,
    PROVENANCE_FORMAT_VERSION, ProvenanceEvidence, ProvenanceMap, ProvenanceStatus,
    ProvenanceUnknownReason,
};

fn entry(raw_index: usize, ordinal: usize, origin: BlockOrigin) -> MessageProvenance {
    MessageProvenance {
        raw_index,
        ordinal,
        blocks: vec![BlockProvenance {
            block_index: 0,
            origin,
            excluded_reason: (ordinal == 2).then_some(ExcludedReason::GeneratedBoundary),
        }],
    }
}

/// A native map of `count` messages after a System one.
fn native_map(count: usize) -> ProvenanceMap {
    ProvenanceMap {
        version: PROVENANCE_FORMAT_VERSION,
        raw_message_count: count + 1,
        checkpoint_count: 0,
        messages: (0..count)
            .map(|ordinal| entry(ordinal + 1, ordinal, BlockOrigin::Native))
            .collect(),
    }
}

/// An incomplete map: a repeated raw index, a copy, an order that is not the raw order.
fn incomplete_map() -> ProvenanceMap {
    ProvenanceMap {
        version: PROVENANCE_FORMAT_VERSION,
        raw_message_count: 9,
        checkpoint_count: 1,
        messages: vec![
            entry(5, 0, BlockOrigin::Native),
            entry(
                5,
                1,
                BlockOrigin::ContinuityCopy {
                    source: CopySource::WorkingNote {
                        run_id: "run-x".into(),
                        note_seq: 2,
                    },
                },
            ),
            entry(2, 2, BlockOrigin::Native),
        ],
    }
}

/// What the old column held for each run, and the status columns beside it.
fn insert_sidecar(
    conn: &Connection,
    run_id: &str,
    status: &ProvenanceStatus,
    revision: i64,
    map_json: &str,
) {
    let (status_name, reason, evidence) = match status {
        ProvenanceStatus::Verified { evidence } => (
            "verified",
            None,
            Some(serde_json::to_string(evidence).unwrap()),
        ),
        ProvenanceStatus::Unknown { reason } => (
            "unknown",
            Some(
                serde_json::to_value(reason)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned(),
            ),
            None,
        ),
    };
    conn.execute(
        "INSERT INTO run_context_provenance(run_id, snapshot_revision, snapshot_sha256, \
         format_version, status, reason_code, evidence_json, map_json) \
         VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
        params![
            run_id,
            revision,
            format!("sha-{run_id}"),
            status_name,
            reason,
            evidence,
            map_json
        ],
    )
    .unwrap();
}

/// The seeded runs with a sidecar each: complete, incomplete (unknown) and empty.
pub(super) fn seeded_with_maps() -> (
    tempfile::TempDir,
    PathBuf,
    Vec<(&'static str, ProvenanceStatus, ProvenanceMap)>,
) {
    let (dir, path, _runs) = seeded();
    let sidecars = vec![
        (
            "run-b-small",
            ProvenanceStatus::Verified {
                evidence: ProvenanceEvidence::CompletePositionMap,
            },
            native_map(1),
        ),
        (
            "run-big",
            ProvenanceStatus::Unknown {
                reason: ProvenanceUnknownReason::IncompleteMap,
            },
            incomplete_map(),
        ),
        (
            "run-d-shares",
            ProvenanceStatus::Unknown {
                reason: ProvenanceUnknownReason::LegacyUnproven,
            },
            ProvenanceMap {
                version: PROVENANCE_FORMAT_VERSION,
                raw_message_count: 0,
                checkpoint_count: 0,
                messages: Vec::new(),
            },
        ),
    ];
    let conn = Connection::open(&path).unwrap();
    for (index, (run, status, map)) in sidecars.iter().enumerate() {
        insert_sidecar(
            &conn,
            run,
            status,
            i64::try_from(index).unwrap() + 4,
            &serde_json::to_string(map).unwrap(),
        );
    }
    drop(conn);
    (dir, path, sidecars)
}

fn sidecar_columns(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    conn.prepare("SELECT name FROM pragma_table_info('run_context_provenance')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

type SidecarIdentity = (
    String,
    i64,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
);

/// Everything of the sidecars except their maps, in `run_id` order.
fn sidecar_identities(conn: &Connection) -> Vec<SidecarIdentity> {
    conn.prepare(
        "SELECT run_id, snapshot_revision, snapshot_sha256, format_version, status, \
         reason_code, evidence_json FROM run_context_provenance ORDER BY run_id",
    )
    .unwrap()
    .query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[test]
fn provenance_maps_become_rows_with_their_status_and_entries_unchanged() {
    // Given: a version 39 database whose runs have a complete, an incomplete and an empty map.
    let (_dir, path, sidecars) = seeded_with_maps();
    let before = sidecar_identities(&Connection::open(&path).unwrap());

    // When: it is converted.
    let summary = apply(&path).unwrap();

    // Then: every map is rows, the old column is gone, and the sidecars read back as saved.
    assert_eq!(summary.converted_provenance_maps, 3);
    assert_eq!(summary.provenance_entries_created, 4);
    assert_eq!(
        (summary.source_schema, summary.target_schema),
        (39, latest_schema())
    );
    assert!(summary.stage_elapsed_seconds.contains_key("provenance"));
    assert!(summary.peak_provenance_wal_frames > 0);
    assert!(!sidecar_columns(&path).contains(&"map_json".to_owned()));
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        sidecar_identities(&conn),
        before,
        "no revision, status or evidence moved"
    );
    for (run, status, map) in &sidecars {
        let sidecar = crate::repo::thread_index::read_sidecar(&conn, run)
            .unwrap()
            .unwrap();
        assert_eq!((&sidecar.status, &sidecar.map), (status, map), "{run}");
    }
    assert_eq!(
        single(&conn, "SELECT COUNT(*) FROM run_message_provenance"),
        4
    );
    assert_eq!(
        single(
            &conn,
            "SELECT COUNT(*) FROM thread_index_runs WHERE source_revision > 1"
        ),
        0,
        "the index's source revision was not advanced"
    );
}

#[test]
fn a_provenance_map_that_is_not_a_typed_map_is_refused_before_anything_changes() {
    // Given: one sidecar whose map is valid JSON of another shape.
    let (_dir, path, _sidecars) = seeded_with_maps();
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE run_context_provenance SET map_json = '{\"version\":1}' WHERE run_id = 'run-big'",
        [],
    )
    .unwrap();
    drop(conn);
    let before = files(&path);

    // When/Then: preview reports it and apply refuses; neither changes anything, and no
    // map is replaced by an empty one.
    let report = preview(&path).unwrap();
    let analysis = report.analysis.as_ref().unwrap();
    assert_eq!(analysis.unconvertible_provenance, 1);
    assert_eq!(analysis.problems[0].run_id, "run-big");
    assert!(report.message.contains("would refuse"));
    let failure = apply(&path).unwrap_err();
    assert!(failure.detail.contains("provenance maps"), "{failure}");
    assert!(!failure.restore_required);
    assert_eq!(files(&path), before);
}

#[test]
fn a_stop_in_the_middle_of_the_maps_is_refused_and_recovered_from_the_backup() {
    // Given: a failure at the commit of the second map.
    let (_dir, path, sidecars) = seeded_with_maps();
    let mut seen = 0;
    let failure = convert_with(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            backup: None,
            ..Default::default()
        },
        &mut |at, _| {
            if at == Point::Provenance(ProvenancePoint::BeforeCommit) {
                seen += 1;
                if seen == 2 {
                    return Err(StorageError::Serialization("injected".into()));
                }
            }
            Ok(())
        },
    )
    .unwrap_err();

    // Then: the first map is rows, the second is untouched, and nothing is published.
    assert_eq!(failure.stage, "provenance");
    assert!(failure.restore_required);
    assert_eq!(failure.run_id.as_deref(), Some("run-big"));
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        single(
            &conn,
            "SELECT map_message_count FROM run_context_provenance WHERE run_id = 'run-b-small'"
        ),
        1
    );
    assert_eq!(
        single(
            &conn,
            "SELECT COUNT(*) FROM run_message_provenance WHERE run_id = 'run-big'"
        ),
        0
    );
    assert_eq!(version(&path), 39);
    drop(conn);
    // A normal start and a rerun both refuse the half converted database.
    assert!(
        Database::open(&StorageConfig {
            db_path: path.clone(),
            ..Default::default()
        })
        .is_err()
    );
    assert!(apply(&path).is_err());

    // When: the verified backup is restored, the command starts over.
    let backup = failure.backup.clone().unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let mut target = path.clone().into_os_string();
        target.push(suffix);
        let _ = fs::remove_file(PathBuf::from(target));
    }
    fs::copy(&backup, &path).unwrap();
    lease(&path);
    let summary = apply(&path).unwrap();

    // Then: it completes with every map.
    assert_eq!(summary.converted_provenance_maps, sidecars.len() as u64);
    assert_eq!(version(&path), latest_schema());
}

/// Takes a converted database back to `version` (40: before the lane ledger; 41: with it;
/// 42: with the favorite flag too; the maps are one column again in all of them).
fn take_back(conn: &Connection, version: u32) {
    conn.execute_batch(crate::migrations::UNDO_V44).unwrap();
    conn.execute_batch(include_str!("../../../tests/support/undo_v43.sql"))
        .unwrap();
    if version <= 41 {
        super::testing::take_lessons_back_to_v41(conn).unwrap();
    }
    if version <= 40 {
        conn.execute_batch(include_str!("../../../tests/support/undo_v41.sql"))
            .unwrap();
    }
    conn.pragma_update(None, "user_version", version).unwrap();
}

#[test]
fn a_version_40_database_converts_only_its_maps_and_leaves_the_runs_as_they_are() {
    converts_only_its_maps_from(40);
}

#[test]
fn a_version_41_database_with_the_lane_ledger_converts_only_its_maps_too() {
    converts_only_its_maps_from(41);
}

#[test]
fn a_version_42_database_with_the_favorite_flag_converts_only_its_maps_too() {
    converts_only_its_maps_from(42);
}

fn converts_only_its_maps_from(from: u32) {
    // Given: a database converted to the current format, then taken back to `version`
    // (messages are rows, the maps are one column again).
    let (_dir, path, sidecars) = seeded_with_maps();
    apply(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    take_back(&conn, from);
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    let runs_before: Vec<(i64, String, i64, Vec<u8>)> = conn
        .prepare("SELECT m.id, m.run_id, m.ordinal, m.msg_sha256 FROM run_messages m ORDER BY m.id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let headers_before = single(&conn, "SELECT SUM(header_rev) FROM run_contexts");
    drop(conn);
    lease(&path);

    // When: it is converted.
    let summary = apply(&path).unwrap();

    // Then: no run is converted again (ids, hashes and header revisions are those of
    // before), and every map is rows.
    assert_eq!(
        (summary.source_schema, summary.target_schema),
        (from, latest_schema())
    );
    assert_eq!(summary.converted_runs, 0);
    assert_eq!(summary.converted_provenance_maps, sidecars.len() as u64);
    assert_eq!(version(&path), latest_schema());
    let conn = Connection::open(&path).unwrap();
    let runs_after: Vec<(i64, String, i64, Vec<u8>)> = conn
        .prepare("SELECT m.id, m.run_id, m.ordinal, m.msg_sha256 FROM run_messages m ORDER BY m.id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(runs_after, runs_before);
    assert_eq!(
        single(&conn, "SELECT SUM(header_rev) FROM run_contexts"),
        headers_before
    );
    for (run, status, map) in &sidecars {
        let sidecar = crate::repo::thread_index::read_sidecar(&conn, run)
            .unwrap()
            .unwrap();
        assert_eq!((&sidecar.status, &sidecar.map), (status, map), "{run}");
    }
    drop(conn);

    // And: a normal start now works and restores every run it held.
    let database = crate::Database::open(&crate::StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    for (run, _, _) in &sidecars {
        assert!(database.run_context(run).unwrap().is_some(), "{run}");
    }
}

#[test]
fn a_version_40_database_that_is_not_converted_yet_refuses_a_normal_start() {
    // Given: a converted database taken back to version 40 (maps are one column again).
    let (_dir, path, _sidecars) = seeded_with_maps();
    apply(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    take_back(&conn, 40);
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    drop(conn);

    // When/Then: a normal start fails closed and names the conversion command.
    let error = crate::Database::open(&crate::StorageConfig {
        db_path: path,
        ..Default::default()
    })
    .unwrap_err();
    assert!(
        matches!(error, StorageError::ConversionRequired { found: 40, .. }),
        "{error:?}"
    );
}

#[test]
fn a_half_converted_version_40_database_is_refused() {
    // Given: a version 40 database that already has the provenance tables.
    let (_dir, path, _sidecars) = seeded_with_maps();
    apply(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    take_back(&conn, 40);
    conn.execute_batch(crate::migrations::V43_PREPARE).unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    drop(conn);
    lease(&path);
    let before = files(&path);

    // When/Then: both modes refuse it, and nothing changes.
    for run in [preview(&path), apply(&path)] {
        let failure = run.unwrap_err();
        assert!(failure.detail.contains("did not finish"), "{failure}");
        assert!(!failure.restore_required);
    }
    assert_eq!(files(&path), before);
}

#[test]
fn one_map_writes_a_log_in_proportion_to_the_entries_it_stores() {
    // Given: version 39 runs with maps of 1, 2,000, 10,000 and no entries.
    let (_dir, path, _runs) = seeded();
    let sizes = [
        ("run-a-empty", 1_usize),
        ("run-b-small", 2_000),
        ("run-big", 10_000),
        ("run-d-shares", 0),
    ];
    let conn = Connection::open(&path).unwrap();
    let mut entry_bytes = BTreeMap::new();
    for (run, count) in sizes {
        let map = native_map(count);
        let bytes: usize = map
            .messages
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap().len())
            .sum();
        entry_bytes.insert(run, (count as u64, bytes as u64));
        insert_sidecar(
            &conn,
            run,
            &ProvenanceStatus::Unknown {
                reason: ProvenanceUnknownReason::IncompleteMap,
            },
            1,
            &serde_json::to_string(&map).unwrap(),
        );
    }
    let page: u64 = conn
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    drop(conn);

    // When: converted, with the frames of each map's transaction read after it commits.
    let mut frames_seen = Vec::new();
    let summary = convert_with(
        &path,
        &ConvertBodiesOptions {
            apply: true,
            backup: None,
            ..Default::default()
        },
        &mut |point, _| {
            if let Point::AfterProvenance { frames } = point {
                frames_seen.push(frames);
            }
            Ok(())
        },
    )
    .unwrap();

    // Then: a map's transaction wrote at most ceil(2 * S / page) + 256 frames, with S the
    // entry text, a generous allowance per row (columns and the unique index) and the
    // header; the figure is the length's, not a function of anything else.
    let order: Vec<&str> = {
        let mut runs: Vec<&str> = sizes.iter().map(|(run, _)| *run).collect();
        runs.sort_unstable();
        runs
    };
    assert_eq!(frames_seen.len(), order.len());
    for (run, frames) in order.iter().zip(&frames_seen) {
        let (count, bytes) = entry_bytes[run];
        let changed = bytes + 160 * count + 2_048;
        let budget = (2 * changed).div_ceil(page) + 256;
        assert!(
            *frames <= budget,
            "map of {run} wrote {frames} frames, budget {budget} (S = {changed})"
        );
    }
    assert_eq!(
        summary.peak_provenance_wal_frames,
        *frames_seen.iter().max().unwrap()
    );
}

#[test]
fn the_preview_of_a_version_40_database_counts_what_the_reclaim_has_to_do() {
    // Given: a version 40 database with one deleted run, its reclaim job waiting, and one
    // row left for the exclusive phase.
    let (_dir, path, sidecars) = seeded_with_maps();
    apply(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    take_back(&conn, 40);
    conn.execute(
        "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES (?1, 't', 'fam')",
        [&sidecars[0].0],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO reclaim_jobs(family_key, state, created_at_ns, updated_at_ns) \
         VALUES ('fam', 'db_pending', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO reclaim_oversized(family_key, tbl, key, stored_bytes) \
         VALUES ('fam', 'run_messages', '1', 5000000)",
        [],
    )
    .unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    drop(conn);
    lease(&path);

    // When: it is previewed.
    let analysis = preview(&path).unwrap().analysis.unwrap();

    // Then: the figures of the reclaim are those of the database, not zero.
    assert_eq!(
        (
            analysis.deleted_runs_to_reclaim,
            analysis.reclaim_jobs_pending,
            analysis.reclaim_oversized_registered
        ),
        (1, 1, 1)
    );
}

#[test]
fn a_version_40_database_with_a_deleted_thread_to_reclaim_is_converted() {
    reclaims_a_deleted_thread_while_converting_from(40);
}

#[test]
fn a_version_41_database_with_a_deleted_thread_to_reclaim_is_converted() {
    reclaims_a_deleted_thread_while_converting_from(41);
}

fn reclaims_a_deleted_thread_while_converting_from(from: u32) {
    // Given: a database of version `from` holding a deleted thread, a deleted run of it and
    // the unfinished reclaim job of their family. Version 40 has no lane tables yet.
    let (_dir, path, sidecars) = seeded_with_maps();
    apply(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    take_back(&conn, from);
    assert_eq!(
        single(
            &conn,
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('lanes', 'lane_reports')"
        ),
        if from == 40 { 0 } else { 2 }
    );
    conn.execute(
        "INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) VALUES ('t', 'fam', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES (?1, 't', 'fam')",
        [&sidecars[0].0],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO reclaim_jobs(family_key, state, created_at_ns, updated_at_ns) \
         VALUES ('fam', 'db_pending', 1, 1)",
        [],
    )
    .unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    drop(conn);
    lease(&path);

    // When: it is converted.
    let summary = apply(&path).unwrap();

    // Then: the family is reclaimed, the lane ledger is there afterwards, and every kept
    // map is rows.
    assert_eq!(summary.retention.reclaimed_runs, 1);
    assert_eq!(
        (summary.source_schema, summary.target_schema),
        (from, latest_schema())
    );
    assert_eq!(version(&path), latest_schema());
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        single(
            &conn,
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('lanes', 'lane_reports')"
        ),
        2
    );
    assert_eq!(
        single(&conn, "SELECT COUNT(*) FROM run_context_provenance"),
        sidecars.len() as i64 - 1
    );
}
