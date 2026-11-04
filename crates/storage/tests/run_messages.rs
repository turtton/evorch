//! A run's conversation as message rows: exact round trips through every kind of change,
//! blob sharing, rollback of a failed save, header updates, the integrity check, and the
//! refusal of a database that still has the old format.

use rusqlite::{Connection, types::Value as SqlValue};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use storage::inspect::{BodyCheckScope, RunDetail};
use storage::thread_index::{
    BlockOrigin, BlockProvenance, MessageProvenance, PROVENANCE_FORMAT_VERSION, ProvenanceEvidence,
    ProvenanceMap, ProvenanceStatus, SnapshotApiError, SnapshotWrite, snapshot_sha256,
};
use storage::{Database, RunContextRecord, Storage, StorageConfig, StorageError};
use tempfile::TempDir;

const UNDO_V40: &str = include_str!("support/undo_v40.sql");
const UNDO_V43: &str = include_str!("support/undo_v43.sql");

fn config(dir: &TempDir) -> StorageConfig {
    StorageConfig {
        db_path: dir.path().join("messages.db"),
        // The tests drive maintenance themselves.
        checkpoint_interval: std::time::Duration::from_secs(3_600),
        flush_interval: std::time::Duration::from_secs(3_600),
        ..StorageConfig::default()
    }
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

fn assistant(text: &str) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
}

fn system(text: &str) -> Value {
    json!({"role": "system", "content": [{"type": "text", "text": text}]})
}

fn record(run_id: &str, messages: &[Value]) -> RunContextRecord {
    RunContextRecord {
        run_id: run_id.into(),
        role: "worker".into(),
        name: "messages".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: serde_json::to_string(messages).unwrap(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: true,
        updated_at_ns: 10,
        host_inputs_json: "[]".into(),
    }
}

/// Text of exactly `bytes` bytes that zstd shrinks a great deal.
fn compressible(bytes: usize, tag: &str) -> String {
    let mut text = format!("{tag}:");
    while text.len() < bytes {
        text.push_str("tool output line with repeated text\n");
    }
    text.truncate(bytes);
    text
}

/// Text of exactly `bytes` bytes that zstd cannot shrink (a deterministic stream of
/// hexadecimal digits from a linear congruential generator).
fn incompressible(bytes: usize, seed: u64) -> String {
    let mut state = seed;
    let mut text = String::with_capacity(bytes);
    while text.len() < bytes {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        text.push_str(&format!("{:x}", state >> 33));
    }
    text.truncate(bytes);
    text
}

fn scalar(path: &std::path::Path, sql: &str) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

/// Every row of the listed tables, as text, ordered by all their columns.
fn dump(path: &std::path::Path, tables: &[&str]) -> Vec<(String, Vec<String>)> {
    let connection = Connection::open(path).unwrap();
    tables
        .iter()
        .map(|table| {
            let columns = connection
                .prepare(&format!("SELECT * FROM {table}"))
                .unwrap()
                .column_count();
            let order: Vec<String> = (1..=columns).map(|column| column.to_string()).collect();
            let mut statement = connection
                .prepare(&format!(
                    "SELECT * FROM {table} ORDER BY {}",
                    order.join(", ")
                ))
                .unwrap();
            let rows = statement
                .query_map([], |row| {
                    Ok((0..columns)
                        .map(|index| format!("{:?}", row.get::<_, SqlValue>(index).unwrap()))
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            ((*table).to_owned(), rows)
        })
        .collect()
}

const CONVERSATION_TABLES: [&str; 9] = [
    "run_contexts",
    "run_messages",
    "content_blobs",
    "content_blob_refs_message",
    "content_blob_refs_header",
    "content_blob_gc_queue",
    "run_context_provenance",
    "run_message_provenance",
    "thread_index_runs",
];

fn read(config: &StorageConfig, run_id: &str) -> RunContextRecord {
    Database::open(config)
        .unwrap()
        .run_context(run_id)
        .unwrap()
        .unwrap()
}

fn message_ids(path: &std::path::Path, run_id: &str) -> Vec<i64> {
    let connection = Connection::open(path).unwrap();
    let mut statement = connection
        .prepare("SELECT id FROM run_messages WHERE run_id = ?1 ORDER BY ordinal")
        .unwrap();
    statement
        .query_map([run_id], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[test]
fn every_kind_of_change_reads_back_byte_for_byte_and_rewrites_only_what_changed() {
    // Given: a run saved with a System message, a blob-backed answer and small turns.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let big = compressible(5_000, "big");
    let base = vec![
        system("be brief"),
        user("hello"),
        assistant(&big),
        user("thanks"),
    ];
    let versions: Vec<(&str, Vec<Value>)> = vec![
        ("empty", vec![]),
        ("base", base.clone()),
        ("append", [base.clone(), vec![assistant("done")]].concat()),
        ("shorten", base[..2].to_vec()),
        (
            "replace the first",
            [vec![system("be verbose")], base[1..].to_vec()].concat(),
        ),
        (
            "insert in the middle",
            [
                base[..1].to_vec(),
                vec![user("inserted")],
                base[1..].to_vec(),
            ]
            .concat(),
        ),
        ("delete the first", base[1..].to_vec()),
        ("back to empty", vec![]),
    ];
    let mut previous_ids: Vec<i64> = Vec::new();
    for (name, messages) in versions {
        let input = record("run-a", &messages);

        // When: the version is saved.
        let receipt = handle.upsert_run_context_receipt(&input).unwrap();

        // Then: it reads back with the exact bytes, its digest and receipt, in order.
        let saved = read(&config, "run-a");
        assert_eq!(saved, input, "{name}");
        assert_eq!(receipt.sha256, sha(&input.messages_json), "{name}");
        assert_eq!(receipt.messages_end, messages.len(), "{name}");
        assert_eq!(
            snapshot_sha256(&saved.messages_json, &saved.checkpoints_json),
            snapshot_sha256(&input.messages_json, &input.checkpoints_json),
            "{name}"
        );
        let ids = message_ids(&config.db_path, "run-a");
        assert_eq!(ids.len(), messages.len(), "{name}");
        assert_eq!(
            scalar(
                &config.db_path,
                "SELECT message_count FROM run_contexts WHERE run_id = 'run-a'"
            ),
            i64::try_from(messages.len()).unwrap(),
            "{name}"
        );
        assert_eq!(
            scalar(
                &config.db_path,
                "SELECT COALESCE(MAX(ordinal) + 1 - COUNT(*), 0) FROM run_messages"
            ),
            0,
            "{name}: ordinals have no gap"
        );
        // Rows that were not rewritten keep their identity.
        if name == "append" {
            assert_eq!(&ids[..previous_ids.len()], &previous_ids[..]);
        }
        previous_ids = ids;
    }
}

#[test]
fn a_save_that_changes_nothing_leaves_every_message_and_blob_row_alone() {
    // Given: a saved run with blob-backed messages.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let messages = vec![
        user(&compressible(4_000, "one")),
        assistant(&compressible(4_000, "two")),
        user("short"),
    ];
    handle
        .upsert_run_context(&record("run-a", &messages))
        .unwrap();
    let tables = [
        "run_messages",
        "content_blobs",
        "content_blob_refs_message",
        "content_blob_refs_header",
        "content_blob_gc_queue",
    ];
    let before = dump(&config.db_path, &tables);

    // When: the same conversation is saved again, with a later timestamp.
    let mut again = record("run-a", &messages);
    again.updated_at_ns = 99;
    handle.upsert_run_context(&again).unwrap();

    // Then: no message row, blob, reference or queue entry was written; the header was.
    assert_eq!(dump(&config.db_path, &tables), before);
    assert_eq!(
        scalar(
            &config.db_path,
            "SELECT header_rev FROM run_contexts WHERE run_id = 'run-a'"
        ),
        2
    );
    assert_eq!(read(&config, "run-a"), again);
}

#[test]
fn strings_around_the_blob_threshold_and_token_lookalikes_round_trip() {
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    // 2 bytes per character: 1023 characters are 2,046 bytes, 1,024 are 2,048.
    let two_byte = |characters: usize| "é".repeat(characters);
    let cases: Vec<(&str, Value)> = vec![
        ("2047 bytes", user(&"a".repeat(2_047))),
        ("2048 bytes", user(&"a".repeat(2_048))),
        ("2049 bytes", user(&"a".repeat(2_049))),
        ("multi-byte 2046", user(&two_byte(1_023))),
        ("multi-byte 2048", user(&two_byte(1_024))),
        ("incompressible", user(&incompressible(6_000, 7))),
        (
            "compressible",
            assistant(&compressible(60_000, "compressible")),
        ),
        (
            "a short string that starts the escape",
            user("\u{e000}raw text"),
        ),
        (
            "a short string that looks like a token",
            user(&format!("\u{e000}v1:{}:5", "0".repeat(64))),
        ),
        (
            "a long string that starts the escape",
            user(&format!("\u{e000}{}", "x".repeat(3_000))),
        ),
        (
            "escapes and unicode",
            user("quote \" backslash \\ newline \n tab \t nul \u{0} emoji 🙂 \u{7f}"),
        ),
        (
            "nested tool result with a long key and long values",
            json!({"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "call-1",
                "content": [{"type": "text", "text": compressible(5_000, "tool")}],
                "extra": {"nested": [[compressible(2_100, "deep")], {"k".repeat(3_000): "v"}]}
            }]}),
        ),
    ];
    for (name, message) in cases {
        // Given: a conversation with just this message, replacing the previous case.
        let input = record("run-a", std::slice::from_ref(&message));

        // When: it is saved.
        handle.upsert_run_context(&input).unwrap();

        // Then: it reads back byte for byte, and the check finds every token resolved.
        assert_eq!(read(&config, "run-a"), input, "{name}");
        assert_consistent(&config.db_path, name);
    }
}

#[test]
fn the_blob_threshold_is_exact_on_a_fresh_database() {
    // Each text is saved to a database of its own so no blob can be shared.
    for (bytes, expected) in [(2_047_usize, 0_i64), (2_048, 1), (2_049, 1)] {
        let dir = TempDir::new().unwrap();
        let config = config(&dir);
        let storage = Storage::open(config.clone()).unwrap();
        storage
            .handle()
            .upsert_run_context(&record("run-a", &[user(&"a".repeat(bytes))]))
            .unwrap();
        assert_eq!(
            scalar(&config.db_path, "SELECT COUNT(*) FROM content_blobs"),
            expected,
            "{bytes} bytes"
        );
        assert_eq!(
            scalar(
                &config.db_path,
                "SELECT COUNT(*) FROM content_blob_refs_message"
            ),
            expected,
            "{bytes} bytes"
        );
    }
}

#[test]
fn a_long_text_shared_by_two_runs_is_one_blob_that_goes_with_its_last_reference() {
    // Given: two runs whose first message is the same long text.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let shared = compressible(8_000, "shared");
    handle
        .upsert_run_context(&record("run-a", &[user(&shared), assistant("a")]))
        .unwrap();
    handle
        .upsert_run_context(&record("run-b", &[user(&shared), assistant("b")]))
        .unwrap();
    assert_eq!(
        scalar(&config.db_path, "SELECT COUNT(*) FROM content_blobs"),
        1
    );
    assert_eq!(
        scalar(
            &config.db_path,
            "SELECT COUNT(*) FROM content_blob_refs_message"
        ),
        2
    );

    // When: run A drops the text and a tick collects.
    handle
        .upsert_run_context(&record("run-a", &[user("short"), assistant("a")]))
        .unwrap();
    handle.checkpoint_now().unwrap();

    // Then: run B still reads it, so the blob stays.
    assert_eq!(
        scalar(&config.db_path, "SELECT COUNT(*) FROM content_blobs"),
        1
    );
    assert_eq!(
        read(&config, "run-b").messages_json,
        record("run-b", &[user(&shared), assistant("b")]).messages_json
    );

    // When: run B drops it too.
    handle
        .upsert_run_context(&record("run-b", &[user("short"), assistant("b")]))
        .unwrap();
    handle.checkpoint_now().unwrap();

    // Then: the unreferenced blob was collected.
    assert_eq!(
        scalar(&config.db_path, "SELECT COUNT(*) FROM content_blobs"),
        0
    );
    assert_eq!(
        scalar(
            &config.db_path,
            "SELECT COUNT(*) FROM content_blob_gc_queue"
        ),
        0
    );
}

#[test]
fn a_failure_anywhere_in_a_save_rolls_everything_back() {
    // Given: a saved run, and a changed version that needs a new blob.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    handle
        .upsert_run_context(&record(
            "run-a",
            &[user("first"), assistant(&compressible(3_000, "old"))],
        ))
        .unwrap();
    let mut changed = record(
        "run-a",
        &[
            user("first"),
            assistant(&compressible(3_000, "new")),
            user(&compressible(4_000, "added")),
        ],
    );
    changed.updated_at_ns = 50;
    let before = dump(&config.db_path, &CONVERSATION_TABLES);
    let injector = Connection::open(&config.db_path).unwrap();

    // When/Then: a failure after each kind of write (header, blob, message row,
    // reference row, sidecar) leaves every table as it was.
    for (name, trigger) in [
        (
            "header",
            "CREATE TRIGGER boom BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(ABORT, 'boom'); END",
        ),
        (
            "blob",
            "CREATE TRIGGER boom AFTER INSERT ON content_blobs BEGIN SELECT RAISE(ABORT, 'boom'); END",
        ),
        (
            "message row",
            "CREATE TRIGGER boom BEFORE INSERT ON run_messages BEGIN SELECT RAISE(ABORT, 'boom'); END",
        ),
        (
            "reference row",
            "CREATE TRIGGER boom BEFORE INSERT ON content_blob_refs_message BEGIN SELECT RAISE(ABORT, 'boom'); END",
        ),
        (
            "sidecar",
            "CREATE TRIGGER boom BEFORE UPDATE ON run_context_provenance BEGIN SELECT RAISE(ABORT, 'boom'); END",
        ),
    ] {
        injector.execute_batch(trigger).unwrap();
        let error = handle.upsert_run_context(&changed).unwrap_err();
        assert!(error.to_string().contains("boom"), "{name}: {error}");
        injector.execute_batch("DROP TRIGGER boom").unwrap();
        assert_eq!(
            dump(&config.db_path, &CONVERSATION_TABLES),
            before,
            "{name}"
        );
    }

    // And: once the fault is gone the same save goes through.
    handle.upsert_run_context(&changed).unwrap();
    assert_eq!(read(&config, "run-a"), changed);
}

fn write_with_map(record: RunContextRecord, expected: Option<u64>) -> SnapshotWrite {
    let count = serde_json::from_str::<Vec<Value>>(&record.messages_json)
        .unwrap()
        .len();
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

#[test]
fn a_save_against_a_stale_revision_writes_nothing_and_an_unknown_one_is_not_guessed() {
    // Given: a run saved twice (revision 2).
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let first = handle
        .save_snapshot_bundle(&write_with_map(record("run-a", &[user("one")]), None))
        .unwrap();
    assert_eq!(first.snapshot_revision, 1);
    handle
        .save_snapshot_bundle(&write_with_map(
            record("run-a", &[user("one"), assistant("two")]),
            Some(1),
        ))
        .unwrap();
    let before = dump(&config.db_path, &CONVERSATION_TABLES);

    // When: a writer that still believes revision 1 saves a different conversation with
    // a long text, and another expects a revision that never existed.
    let stale = write_with_map(
        record("run-a", &[user(&compressible(5_000, "stale"))]),
        Some(1),
    );
    let unknown = write_with_map(
        record("run-a", &[user(&compressible(5_000, "unknown"))]),
        Some(77),
    );

    // Then: both are conflicts, and neither left a blob, a row or a header change.
    for write in [stale, unknown] {
        assert!(matches!(
            handle.save_snapshot_bundle(&write),
            Err(SnapshotApiError::RevisionConflict)
        ));
        assert_eq!(dump(&config.db_path, &CONVERSATION_TABLES), before);
    }
}

fn long_reason() -> String {
    compressible(2_500, "why")
}

fn config_with_bodies(restorable: bool, reason: Option<&str>) -> String {
    json!({
        "restorable": restorable,
        "non_restorable_reason": reason,
        "instructions": compressible(3_000, "instructions"),
        "thread_id": "t-1",
    })
    .to_string()
}

fn with_header_bodies(run_id: &str) -> RunContextRecord {
    let mut input = record(
        run_id,
        &[user("hello"), assistant(&compressible(4_000, "a"))],
    );
    input.restorable = false;
    input.config_json = config_with_bodies(false, Some(&long_reason()));
    input.checkpoints_json = json!([{
        "id": "checkpoint-1",
        "summary": {"role": "user", "content": [{"type": "text", "text": compressible(3_500, "summary")}]},
        "range": [0, 1],
    }])
    .to_string();
    input
}

fn checked(path: &std::path::Path) -> Value {
    Database::open_for_inspection(path)
        .unwrap()
        .inspect_check_bodies(&BodyCheckScope::All, true)
        .unwrap()
}

fn assert_consistent(path: &std::path::Path, what: &str) {
    let report = checked(path);
    assert_eq!(report["ok"], true, "{what}: {report}");
}

#[test]
fn restore_state_swap_compares_tokens_and_keeps_every_reference_exact() {
    // Given: a non-restorable run whose configuration and checkpoint list hold blobs.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let input = with_header_bodies("run-a");
    handle.upsert_run_context(&input).unwrap();
    let saved = read(&config, "run-a");
    assert_consistent(&config.db_path, "after the save");
    let header_rev = |sql: &str| scalar(&config.db_path, sql);
    assert_eq!(
        header_rev("SELECT header_rev FROM run_contexts WHERE run_id = 'run-a'"),
        1
    );

    // When: the swap expects a configuration that is not the stored one.
    let before = dump(&config.db_path, &CONVERSATION_TABLES);
    let mut wrong: Value = serde_json::from_str(&saved.config_json).unwrap();
    wrong["thread_id"] = json!("elsewhere");
    let swapped = handle
        .compare_and_swap_run_context_restore_state(
            "run-a",
            &wrong.to_string(),
            &saved.config_json,
            true,
        )
        .unwrap();

    // Then: it reports no swap and changed nothing, not even a blob.
    assert!(!swapped);
    assert_eq!(dump(&config.db_path, &CONVERSATION_TABLES), before);

    // When: it expects the configuration as it was read, with a new long reason.
    let mut retry: Value = serde_json::from_str(&saved.config_json).unwrap();
    retry["non_restorable_reason"] = json!(compressible(2_600, "another reason"));
    retry["restorable"] = json!(false);
    let swapped = handle
        .compare_and_swap_run_context_restore_state(
            "run-a",
            &saved.config_json,
            &retry.to_string(),
            false,
        )
        .unwrap();

    // Then: the configuration is the new one, the header revision went up by one in
    // SQL, the projection follows, and the checkpoint tokens were not escaped again.
    assert!(swapped);
    let after = read(&config, "run-a");
    let after_config: Value = serde_json::from_str(&after.config_json).unwrap();
    assert_eq!(after_config, retry);
    assert_eq!(after.checkpoints_json, saved.checkpoints_json);
    assert_eq!(after.messages_json, saved.messages_json);
    assert_eq!(
        header_rev("SELECT header_rev FROM run_contexts WHERE run_id = 'run-a'"),
        2
    );
    assert_eq!(
        header_rev("SELECT length(non_restorable_reason) FROM run_contexts WHERE run_id = 'run-a'"),
        2_600
    );
    assert_consistent(&config.db_path, "after the swap");

    // When: the run is restorable, the swap does nothing.
    let mut restorable = input.clone();
    restorable.restorable = true;
    handle.upsert_run_context(&restorable).unwrap();
    let before = dump(&config.db_path, &CONVERSATION_TABLES);
    let current = read(&config, "run-a");
    assert!(
        !handle
            .compare_and_swap_run_context_restore_state(
                "run-a",
                &current.config_json,
                &current.config_json,
                true
            )
            .unwrap()
    );
    assert_eq!(dump(&config.db_path, &CONVERSATION_TABLES), before);
}

#[test]
fn invalidation_edits_the_configuration_only_and_replaces_a_blob_backed_reason() {
    // Given: a restorable run whose reason text is a blob and whose messages cannot be read.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let mut input = with_header_bodies("run-a");
    input.restorable = true;
    handle.upsert_run_context(&input).unwrap();
    let saved = read(&config, "run-a");
    let blobs_before = scalar(
        &config.db_path,
        "SELECT COUNT(*) FROM content_blob_refs_header",
    );
    Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "UPDATE run_messages SET codec = 0, skeleton = x'ff' WHERE run_id = 'run-a'",
            [],
        )
        .unwrap();

    // When: the run is invalidated.
    handle.invalidate_run_context("run-a").unwrap();
    handle.invalidate_run_context("absent").unwrap();

    // Then: only restore metadata changed, the history was never read, the long reason
    // is gone, the checkpoint list is exactly as saved and the references follow.
    let connection = Connection::open(&config.db_path).unwrap();
    let (restorable, rev, reason, config_len): (bool, i64, String, i64) = connection
        .query_row(
            "SELECT restorable, header_rev, non_restorable_reason, config_len FROM run_contexts \
             WHERE run_id = 'run-a'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert!(!restorable);
    assert_eq!(rev, 2);
    assert_eq!(reason, "persist_failed");
    let stored_config: String = connection
        .query_row(
            "SELECT config_json FROM run_contexts WHERE run_id = 'run-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let descriptor: Value = serde_json::from_str(&stored_config).unwrap();
    assert_eq!(descriptor["restorable"], false);
    assert_eq!(descriptor["non_restorable_reason"], "persist_failed");
    assert!(
        config_len > 3_000,
        "the length is that of the resolved document"
    );
    let blobs_after = scalar(
        &config.db_path,
        "SELECT COUNT(*) FROM content_blob_refs_header",
    );
    assert_eq!(blobs_after, blobs_before - 1);
    let checkpoints: String = connection
        .query_row(
            "SELECT checkpoints_json FROM run_contexts WHERE run_id = 'run-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        checkpoints.contains('\u{e000}'),
        "the list keeps its tokens"
    );
    let detail = Database::open_for_inspection(&config.db_path)
        .unwrap()
        .inspect_run_detail("run-a", RunDetail::Summary)
        .unwrap();
    assert_eq!(detail["context"]["checkpoints_count"], json!(1), "{detail}");
    // The history was damaged on purpose, so a full read refuses: it is not an empty list.
    assert!(
        Database::open_for_inspection(&config.db_path)
            .unwrap()
            .inspect_run_detail("run-a", RunDetail::Full)
            .is_err()
    );
    assert!(!saved.checkpoints_json.is_empty());
}

#[test]
fn the_integrity_check_names_each_kind_of_damage_and_ranges_read_only_what_they_name() {
    // Given: a consistent run of five messages, some blob-backed.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let messages = vec![
        system("system"),
        user(&compressible(3_000, "one")),
        assistant("two"),
        user(&compressible(3_000, "three")),
        assistant("four"),
    ];
    handle
        .upsert_run_context(&with_messages_and_bodies("run-a", &messages))
        .unwrap();
    assert_consistent(&config.db_path, "fresh");
    let connection = Connection::open(&config.db_path).unwrap();
    let kinds = |report: &Value| -> Vec<String> {
        report["problems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|problem| problem["kind"].as_str().unwrap().to_owned())
            .collect()
    };

    // When/Then: each fault is reported by kind and repairing it clears the report.
    let faults: [(&str, &str, &str); 5] = [
        (
            "missing_reference",
            "CREATE TEMP TABLE saved_ref AS SELECT * FROM content_blob_refs_message \
             ORDER BY 1, 2 LIMIT 1;
             DELETE FROM content_blob_refs_message WHERE (message_id, blob_id) = \
             (SELECT message_id, blob_id FROM saved_ref)",
            "INSERT INTO content_blob_refs_message SELECT * FROM saved_ref; DROP TABLE saved_ref",
        ),
        (
            "message_hash",
            "CREATE TEMP TABLE saved_sha AS SELECT id, msg_sha256 FROM run_messages WHERE ordinal = 2;
             UPDATE run_messages SET msg_sha256 = zeroblob(32) WHERE ordinal = 2",
            "UPDATE run_messages SET msg_sha256 = (SELECT msg_sha256 FROM saved_sha) \
             WHERE ordinal = 2; DROP TABLE saved_sha",
        ),
        (
            "message_count",
            "UPDATE run_contexts SET message_count = message_count + 1",
            "UPDATE run_contexts SET message_count = message_count - 1",
        ),
        (
            "message_ordinals",
            "UPDATE run_messages SET ordinal = 9 WHERE ordinal = 4",
            "UPDATE run_messages SET ordinal = 4 WHERE ordinal = 9",
        ),
        (
            "messages_len",
            "UPDATE run_contexts SET messages_len = messages_len + 1",
            "UPDATE run_contexts SET messages_len = messages_len - 1",
        ),
    ];
    for (kind, damage, repair) in faults {
        connection.execute_batch(damage).unwrap();
        let found = kinds(&checked(&config.db_path));
        assert!(found.iter().any(|found| found == kind), "{kind}: {found:?}");
        connection.execute_batch(repair).unwrap();
        assert_consistent(&config.db_path, kind);
    }

    // And: a range reads exactly the messages it names, even beside damaged ones.
    connection
        .execute_batch(
            "UPDATE run_messages SET codec = 0, skeleton = x'ff' WHERE ordinal IN (0, 4)",
        )
        .unwrap();
    let inspected = Database::open_for_inspection(&config.db_path).unwrap();
    let middle = inspected
        .inspect_run_detail("run-a", RunDetail::Messages(1..4))
        .unwrap();
    let range = middle["context"]["messages_json"].as_array().unwrap();
    assert_eq!(range.len(), 3);
    assert_eq!(range[0], messages[1]);
    assert_eq!(
        middle["context"]["messages_range"],
        json!({"from": 1, "to": 4})
    );
    assert!(
        inspected
            .inspect_run_detail("run-a", RunDetail::Full)
            .is_err()
    );
    assert!(
        inspected
            .inspect_run_detail("run-a", RunDetail::Messages(3..6))
            .is_err(),
        "a range past the end is refused, not clipped"
    );
}

fn with_messages_and_bodies(run_id: &str, messages: &[Value]) -> RunContextRecord {
    let mut input = with_header_bodies(run_id);
    input.messages_json = serde_json::to_string(messages).unwrap();
    input.restorable = true;
    input
}

fn file_bytes(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

/// A database that has the shape of one written before this storage format: the saved
/// run keeps its messages in `run_contexts.messages_json`.
fn old_format_database(path: &std::path::Path, version: u32) {
    let config = StorageConfig {
        db_path: path.to_path_buf(),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).unwrap());
    let connection = Connection::open(path).unwrap();
    connection.execute_batch(UNDO_V43).unwrap();
    connection.execute_batch(UNDO_V40).unwrap();
    connection
        .execute(
            "INSERT INTO run_contexts(run_id, role, name, config_json, messages_json, \
             checkpoints_json, terminal_phase, restorable, updated_at_ns) \
             VALUES ('old', 'worker', 'old', '{}', '[{\"role\":\"user\",\"content\":[]}]', '[]', 'Done', 1, 1)",
            [],
        )
        .unwrap();
    connection
        .pragma_update(None, "user_version", version)
        .unwrap();
    connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    drop(connection);
}

#[test]
fn an_old_format_database_is_refused_by_every_entry_without_being_touched() {
    for version in [39_u32, 31, 10] {
        // Given: a database in the old conversation format.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("old.db");
        old_format_database(&path, version);
        let before = file_bytes(&path);
        let config = StorageConfig {
            db_path: path.clone(),
            ..StorageConfig::default()
        };

        // When/Then: every way to open it fails with the conversion error, and the
        // file is byte for byte what it was.
        let refused = |error: StorageError| {
            assert!(
                matches!(&error, StorageError::ConversionRequired { found, .. } if *found == version),
                "{error:?}"
            );
            assert!(error.to_string().contains("conversion command"));
        };
        refused(Database::open(&config).unwrap_err());
        refused(Database::open_read_only(&path).unwrap_err());
        refused(Database::open_for_inspection(&path).unwrap_err());
        refused(Storage::open(config.clone()).err().unwrap());
        for apply in [false, true] {
            let error = storage::maintenance::compact_events(
                &path,
                &storage::maintenance::CompactOptions {
                    apply,
                    ..Default::default()
                },
            )
            .unwrap_err();
            // The preview needs a lease the test does not create; either refusal leaves
            // the file alone, which is what matters.
            let _ = error;
        }
        assert_eq!(file_bytes(&path), before, "version {version}");
        assert_eq!(
            scalar(&path, "PRAGMA user_version"),
            i64::from(version),
            "the version was not advanced"
        );
        assert_eq!(
            scalar(
                &path,
                "SELECT COUNT(*) FROM pragma_table_info('run_contexts') WHERE name = 'messages_json'"
            ),
            1,
            "the old column is still there"
        );
    }
}

#[test]
fn a_version_40_database_whose_maps_are_a_column_is_refused_by_every_entry_untouched() {
    // Given: a database of version 40 (messages are rows) whose provenance maps are still
    // one column: the form the first conversion left.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("v40.db");
    let config = StorageConfig {
        db_path: path.clone(),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(UNDO_V43).unwrap();
    connection.pragma_update(None, "user_version", 40).unwrap();
    connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    drop(connection);
    let before = file_bytes(&path);

    // When/Then: every way to open it points at the conversion command, and the file
    // is byte for byte what it was.
    let refused = |error: StorageError| {
        assert!(
            matches!(&error, StorageError::ConversionRequired { found: 40, .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("conversion command"));
    };
    refused(Database::open(&config).unwrap_err());
    refused(Database::open_read_only(&path).unwrap_err());
    refused(Database::open_for_inspection(&path).unwrap_err());
    refused(Storage::open(config.clone()).err().unwrap());
    assert_eq!(file_bytes(&path), before);
}

#[test]
fn a_half_converted_or_newer_database_is_refused_and_a_new_one_works() {
    // Given: a current database in which the old column came back (a conversion that stopped).
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("half.db");
    let config = StorageConfig {
        db_path: path.clone(),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("ALTER TABLE run_contexts ADD COLUMN messages_json TEXT NOT NULL DEFAULT ''")
        .unwrap();
    drop(connection);

    // Then: it is refused like the old format.
    assert!(matches!(
        Database::open(&config),
        Err(StorageError::ConversionRequired { .. })
    ));

    // And: a version from the future keeps its own error.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("future.db");
    let config = StorageConfig {
        db_path: path.clone(),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).unwrap());
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 4_000)
        .unwrap();
    assert!(matches!(
        Database::open(&config),
        Err(StorageError::SchemaTooNew { found: 4_000, .. })
    ));

    // And: a new database saves, restarts and restores.
    let dir = TempDir::new().unwrap();
    let fresh = self::config(&dir);
    let storage = Storage::open(fresh.clone()).unwrap();
    let input = with_messages_and_bodies("run-a", &[user("hello"), assistant("world")]);
    storage.handle().upsert_run_context(&input).unwrap();
    storage.close();
    let reopened = Storage::open(fresh.clone()).unwrap();
    assert_eq!(read(&fresh, "run-a").messages_json, input.messages_json);
    reopened.close();
}

#[test]
fn deleting_a_message_row_queues_its_blobs_through_the_cascade() {
    // Given: a saved run whose messages hold two different blobs.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .upsert_run_context(&record(
            "run-a",
            &[
                user(&compressible(3_000, "one")),
                user(&compressible(3_000, "two")),
            ],
        ))
        .unwrap();
    storage.close();
    let connection = Connection::open(&config.db_path).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    assert_eq!(
        scalar(
            &config.db_path,
            "SELECT COUNT(*) FROM content_blob_refs_message"
        ),
        2
    );

    // When: one message row is deleted.
    connection
        .execute("DELETE FROM run_messages WHERE ordinal = 0", [])
        .unwrap();

    // Then: its reference went with it and its blob is queued for the collector (the
    // collector, not the delete, decides whether anything else still names it).
    assert_eq!(
        scalar(
            &config.db_path,
            "SELECT COUNT(*) FROM content_blob_refs_message"
        ),
        1
    );
    assert_eq!(
        scalar(
            &config.db_path,
            "SELECT COUNT(*) FROM content_blob_gc_queue"
        ),
        1
    );
}

#[test]
fn a_snapshot_that_cannot_be_stored_faithfully_is_refused_and_leaves_nothing() {
    // Given: an empty database.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let before = dump(&config.db_path, &CONVERSATION_TABLES);

    // When/Then: a configuration that is not JSON, messages that are not an array and
    // messages that are not JSON at all are each refused, and nothing is written.
    let mut bad_config = record("run-a", &[user("x")]);
    bad_config.config_json = "not json".into();
    let mut object_messages = record("run-a", &[]);
    object_messages.messages_json = r#"{"role":"user"}"#.into();
    let mut garbage = record("run-a", &[]);
    garbage.messages_json = "[[".into();
    for bad in [bad_config, object_messages, garbage] {
        assert!(handle.upsert_run_context(&bad).is_err());
        assert_eq!(dump(&config.db_path, &CONVERSATION_TABLES), before);
    }
}

#[test]
fn a_terminal_summary_restores_only_the_last_message() {
    // Given: a run of three messages whose first two cannot be decoded any more.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .upsert_run_context(&record(
            "run-a",
            &[user("one"), assistant("two"), assistant("the final answer")],
        ))
        .unwrap();
    storage.close();
    Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "UPDATE run_messages SET codec = 0, skeleton = x'ff' WHERE ordinal < 2",
            [],
        )
        .unwrap();
    let database = Database::open(&config).unwrap();

    // When: the summary is read.
    let summaries = database.terminal_summaries(&["run-a".to_owned()]).unwrap();

    // Then: the count comes from the header and the text from the last message alone.
    assert_eq!(summaries[0].message_count, 3);
    assert_eq!(summaries[0].final_text.as_deref(), Some("the final answer"));

    // And: damage to the last message is an error, not a run without a final text.
    Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "UPDATE run_messages SET codec = 0, skeleton = x'ff' WHERE ordinal = 2",
            [],
        )
        .unwrap();
    assert!(database.terminal_summaries(&["run-a".to_owned()]).is_err());
}

#[test]
fn verification_compares_the_projection_with_a_reason_that_is_a_blob() {
    // Given: a run whose non-restorable reason is long enough to be a blob.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .upsert_run_context(&with_header_bodies("run-a"))
        .unwrap();
    storage.close();
    assert_consistent(&config.db_path, "after the save");
    let connection = Connection::open(&config.db_path).unwrap();
    let in_config: String = connection
        .query_row(
            "SELECT config_json FROM run_contexts WHERE run_id = 'run-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        in_config.contains('\u{e000}') && in_config.contains("non_restorable_reason"),
        "the reason is stored as a token"
    );

    // When: only the projection of the reason is damaged.
    connection
        .execute(
            "UPDATE run_contexts SET non_restorable_reason = 'something else'",
            [],
        )
        .unwrap();

    // Then: the plain check (which does not resolve bodies) cannot see it, and the
    // verifying check reports it.
    let kinds = |report: &Value| -> Vec<String> {
        report["problems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|problem| problem["kind"].as_str().unwrap().to_owned())
            .collect()
    };
    let inspected = Database::open_for_inspection(&config.db_path).unwrap();
    let plain = inspected
        .inspect_check_bodies(&BodyCheckScope::All, false)
        .unwrap();
    assert_eq!(plain["ok"], true, "{plain}");
    let verified = inspected
        .inspect_check_bodies(&BodyCheckScope::All, true)
        .unwrap();
    assert_eq!(verified["ok"], false);
    assert_eq!(kinds(&verified), ["non_restorable_reason"], "{verified}");

    // And: restoring the value clears the finding.
    let reason = long_reason();
    connection
        .execute(
            "UPDATE run_contexts SET non_restorable_reason = ?1",
            [&reason],
        )
        .unwrap();
    assert_consistent(&config.db_path, "after the repair");
}

#[test]
fn a_normal_inspection_reports_counts_and_body_statistics_without_resolving_a_body() {
    // Given: a run whose messages and checkpoint summary are blob-backed, and whose
    // configuration is small. Then every blob is damaged: any resolution would fail.
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let mut input = record(
        "run-a",
        &[
            user(&compressible(3_000, "one")),
            assistant(&compressible(4_000, "two")),
        ],
    );
    input.checkpoints_json = json!([
        {"id": "c1", "summary": user(&compressible(3_500, "summary")), "range": [0, 1]},
        {"id": "c2", "summary": user("short"), "range": [0, 2]},
    ])
    .to_string();
    storage.handle().upsert_run_context(&input).unwrap();
    storage.close();
    let connection = Connection::open(&config.db_path).unwrap();
    let plain_bytes: i64 = connection
        .query_row("SELECT SUM(byte_len) FROM content_blobs", [], |row| {
            row.get(0)
        })
        .unwrap();
    connection
        .execute("UPDATE content_blobs SET codec = 0, data = x'ff'", [])
        .unwrap();
    let inspected = Database::open_for_inspection(&config.db_path).unwrap();

    // When: the normal view is read.
    let summary = inspected
        .inspect_run_detail("run-a", RunDetail::Summary)
        .unwrap();

    // Then: counts come from the header and the stored list structure, and the body
    // statistics from the reference tables and blob metadata.
    let context = &summary["context"];
    assert_eq!(context["messages_count"], 2, "{summary}");
    assert_eq!(context["checkpoints_count"], 2, "{summary}");
    assert!(context.get("checkpoints_json").is_none());
    assert!(context.get("messages_json").is_none());
    let bodies = &context["bodies"];
    assert_eq!(bodies["message_blobs"]["references"], 2);
    assert_eq!(bodies["message_blobs"]["distinct_blobs"], 2);
    assert_eq!(bodies["header_blobs"]["references"], 1);
    assert_eq!(
        bodies["message_blobs"]["plain_bytes"].as_i64().unwrap()
            + bodies["header_blobs"]["plain_bytes"].as_i64().unwrap(),
        plain_bytes
    );
    assert_eq!(
        bodies["message_blobs"]["stored_bytes"], 2,
        "damaged: 1 byte each"
    );
    assert!(bodies["message_skeleton_bytes"].as_i64().unwrap() > 0);

    // And: asking for text does resolve, and refuses the damaged bodies instead of
    // showing a placeholder.
    assert!(
        inspected
            .inspect_run_detail("run-a", RunDetail::Full)
            .is_err()
    );
    assert!(
        inspected
            .inspect_run_detail("run-a", RunDetail::Messages(0..1))
            .is_err()
    );
}
