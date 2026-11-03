use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::*;
use crate::Database;
use crate::StorageConfig;
use crate::codec;
use crate::migrations::MIGRATIONS;
use crate::repo::run_messages;

const MIB: usize = 1024 * 1024;

fn lease(path: &Path) {
    drop(offline::lease_file(path).unwrap());
}

/// A database exactly as migration v39 leaves it, built by replaying the real migrations.
fn v39(dir: &Path) -> PathBuf {
    let path = dir.join("events.db");
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "auto_vacuum", 2).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    for migration in &MIGRATIONS[..39] {
        conn.execute_batch(migration).unwrap();
    }
    conn.pragma_update(None, "user_version", 39).unwrap();
    drop(conn);
    lease(&path);
    path
}

struct Run {
    id: &'static str,
    messages: String,
    checkpoints: String,
    config: String,
    revision: i64,
    updated: i64,
    restorable: bool,
}

fn long(tag: &str, size: usize) -> String {
    let unit = format!("{tag} こんにちは \"quoted\" \\ line\n");
    unit.repeat(size / unit.len() + 1)
}

fn plain_run(id: &'static str, messages: &Value, revision: i64) -> Run {
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

fn insert(conn: &Connection, run: &Run) {
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

fn conversation(tag: &str) -> Value {
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

fn seeded() -> (tempfile::TempDir, PathBuf, Vec<Run>) {
    let dir = tempfile::tempdir().unwrap();
    let path = v39(dir.path());
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

fn apply(path: &Path) -> Result<ConvertBodiesSummary, ConvertBodiesFailure> {
    convert_bodies(
        path,
        &ConvertBodiesOptions {
            apply: true,
            backup: None,
        },
    )
}

fn preview(path: &Path) -> Result<ConvertBodiesSummary, ConvertBodiesFailure> {
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

fn version(path: &Path) -> u32 {
    Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn read(conn: &Connection, id: &str) -> crate::RunContextRecord {
    run_messages::get(conn, id).unwrap().unwrap()
}

fn files(path: &Path) -> Vec<(String, Vec<u8>)> {
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
    assert_eq!((summary.source_schema, summary.target_schema), (39, 40));
    assert_eq!(summary.unconverted_runs_remaining, 0);
    assert_eq!(summary.mismatched_runs, 0);
    assert_eq!(summary.events_tokenized, 0);
    assert!(summary.checkpoint_truncated && summary.vacuum);
    assert_eq!(version(&path), 40);
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
fn a_run_that_does_not_round_trip_stops_the_conversion_and_names_the_run() {
    // Given: a run whose saved messages carry whitespace the serializer does not produce.
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

    // When: previewed and applied.
    let analysis = preview(&path).unwrap().analysis.unwrap();
    let failure = apply(&path).unwrap_err();

    // Then: preview lists the run; apply stops at it, with the backup and the way back.
    assert_eq!(analysis.unconvertible_runs, 1);
    assert_eq!(analysis.problems[0].run_id, "run-c-spaced");
    assert_eq!(failure.reason, "mismatch");
    assert_eq!(failure.stage, "convert");
    assert_eq!(failure.run_id.as_deref(), Some("run-c-spaced"));
    assert_eq!(failure.converted_runs, 3);
    assert!(failure.restore_required);
    assert!(failure.backup.as_ref().unwrap().is_file());
    assert!(failure.to_string().contains("verified backup"));
    // The half-converted database is refused by a normal start.
    let error = Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .err()
    .unwrap();
    assert!(
        matches!(error, StorageError::ConversionRequired { .. }),
        "{error}"
    );
    // And by this command: it does not guess where to continue.
    let again = apply(&path).unwrap_err();
    assert!(again.detail.contains("did not finish"), "{again}");
}

#[test]
fn a_stopped_conversion_is_recovered_by_restoring_the_backup_and_starting_over() {
    // Given: a conversion stopped by a failure injected at each point in turn.
    let points = [
        Point::AfterPrepare,
        Point::AfterRetention,
        Point::Run(RunPoint::BeforeReadback),
        Point::Run(RunPoint::BeforeCommit),
        Point::AfterRun,
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
            },
            &mut |at, _| {
                if at == point {
                    seen += 1;
                    if seen == 2 || !matches!(point, Point::Run(_) | Point::AfterRun) {
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
        assert_eq!(version(&path), 39, "{point:?}: not published");
        let backup = failure.backup.clone().unwrap();
        assert!(
            Database::open(&StorageConfig {
                db_path: path.clone(),
                ..Default::default()
            })
            .is_err()
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
        },
        &mut |at, conn| {
            match at {
                Point::Run(RunPoint::BeforeCommit) => {
                    seen += 1;
                    if seen == 3 {
                        return Err(StorageError::Serialization("injected".into()));
                    }
                }
                Point::AfterRun if seen == 2 => {
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

    // When/Then: the conversion stops in the retention step with no run converted.
    let failure = apply(&path).unwrap_err();
    assert_eq!(failure.stage, "retention");
    assert_eq!(failure.converted_runs, 0);
    assert!(failure.restore_required);
}

#[test]
fn one_run_writes_a_log_in_proportion_to_what_it_stores() {
    // Given: a conversion of the seeded runs.
    let (_dir, path, _) = seeded();
    let summary = apply(&path).unwrap();

    // Then: the largest log the transactions left stays within the budget
    // ceil(2 * S / page) + 256 frames, with S the bytes the run's rows and blobs occupy
    // (here bounded by the old payload, the new blobs and a generous skeleton allowance).
    let conn = Connection::open(&path).unwrap();
    let page: u64 = conn
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    let stored: u64 = single(
        &conn,
        "SELECT COALESCE(SUM(OCTET_LENGTH(skeleton)), 0) FROM run_messages",
    )
    .try_into()
    .unwrap();
    let analysis_stored = summary.blob_bytes_created + stored;
    let budget_frames = (2 * analysis_stored).div_ceil(page) + 256;
    let frame = page + 24;
    assert!(
        summary.peak_wal_bytes <= 32 + budget_frames * frame,
        "peak log {} bytes exceeds {} frames of {frame}",
        summary.peak_wal_bytes,
        budget_frames
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

    // Then: both refuse it, and apply names the run; nothing is canonicalized.
    assert_eq!(
        (analysis.canonicalizable_runs, analysis.unconvertible_runs),
        (0, 1)
    );
    assert_eq!(failure.reason, "mismatch");
    assert_eq!(failure.run_id.as_deref(), Some("run-old-order"));
    assert!(failure.detail.contains("restorable"), "{failure}");
}
