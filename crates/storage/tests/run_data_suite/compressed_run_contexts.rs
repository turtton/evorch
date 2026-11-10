//! Large run snapshots keep their long strings as compressed content blobs; reads, receipts
//! and limits still see plaintext.

use serde_json::json;
use storage::thread_index::snapshot_sha256;
use storage::{Database, RunContextRecord, Storage, StorageConfig};
use tempfile::TempDir;

fn messages(total_bytes: usize) -> String {
    let mut messages = Vec::new();
    let mut size = 0;
    for index in 0.. {
        let text = format!(
            "turn {index}: {}",
            "tool output line with repeated text\n".repeat(200)
        );
        size += text.len();
        messages.push(
            json!({"role": if index % 2 == 0 { "user" } else { "assistant" },
            "content": [{"type": "text", "text": text}]}),
        );
        if size >= total_bytes {
            break;
        }
    }
    serde_json::to_string(&messages).unwrap()
}

fn record(run_id: &str, messages_json: String) -> RunContextRecord {
    RunContextRecord {
        run_id: run_id.into(),
        role: "worker".into(),
        name: "compressed".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json,
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: true,
        updated_at_ns: 10,
        host_inputs_json: "[]".into(),
    }
}

fn open(temp: &TempDir) -> (Storage, StorageConfig) {
    let config = StorageConfig {
        db_path: temp.path().join("compressed.db"),
        ..StorageConfig::default()
    };
    (Storage::open(config.clone()).unwrap(), config)
}

fn scalar(config: &StorageConfig, sql: &str) -> i64 {
    rusqlite::Connection::open(&config.db_path)
        .unwrap()
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

#[test]
fn nine_mebibyte_snapshot_is_stored_as_small_blobs_and_comes_back_byte_identical() {
    // Given: a snapshot of more than 9 MiB.
    let temp = TempDir::new().unwrap();
    let (storage, config) = open(&temp);
    let input = record("run-big", messages(9 * 1024 * 1024));
    assert!(input.messages_json.len() > 9 * 1024 * 1024);
    let message_count = serde_json::from_str::<Vec<serde_json::Value>>(&input.messages_json)
        .unwrap()
        .len();

    // When: it is saved and reloaded.
    let receipt = storage.handle().upsert_run_context_receipt(&input).unwrap();
    let saved = Database::open(&config)
        .unwrap()
        .run_context("run-big")
        .unwrap()
        .unwrap();

    // Then: one row per message, whose long strings are blobs far smaller than the
    // text, the plaintext length is recorded in the header, and the snapshot, its digest
    // and its receipt are unchanged.
    assert_eq!(
        scalar(&config, "SELECT COUNT(*) FROM run_messages"),
        i64::try_from(message_count).unwrap()
    );
    let recorded = scalar(&config, "SELECT messages_len FROM run_contexts");
    assert_eq!(
        usize::try_from(recorded).unwrap(),
        input.messages_json.len()
    );
    let blobs = scalar(&config, "SELECT SUM(length(data)) FROM content_blobs");
    let skeletons = scalar(&config, "SELECT SUM(length(skeleton)) FROM run_messages");
    assert!(
        blobs + skeletons < recorded / 10,
        "{blobs}+{skeletons} vs {recorded}"
    );
    assert_eq!(saved, input);
    assert!(receipt.matches(&saved));
    assert_eq!(
        snapshot_sha256(&saved.messages_json, &saved.checkpoints_json),
        snapshot_sha256(&input.messages_json, &input.checkpoints_json)
    );
}

#[test]
fn small_large_and_empty_snapshots_read_through_every_entry_point() {
    // Given: a snapshot with blob-backed messages, a small one that needs no blob, and an
    // empty one.
    let temp = TempDir::new().unwrap();
    let (storage, config) = open(&temp);
    let large = messages(64 * 1024);
    let handle = storage.handle();
    handle
        .upsert_run_context(&record("run-compressed", large.clone()))
        .unwrap();
    handle
        .upsert_run_context(&record(
            "run-small",
            r#"[{"role":"assistant","content":[{"type":"text","text":"hi"}]}]"#.into(),
        ))
        .unwrap();
    handle
        .upsert_run_context(&record("run-empty", "[]".into()))
        .unwrap();
    storage.close();
    let ids = ["run-compressed", "run-small", "run-empty"].map(str::to_owned);

    // When/Then: full reads decode all of them.
    let db = Database::open(&config).unwrap();
    for id in &ids {
        let loaded = db.run_context(id).unwrap().unwrap();
        match id.as_str() {
            "run-compressed" => assert_eq!(loaded.messages_json, large),
            "run-empty" => assert_eq!(loaded.messages_json, "[]"),
            _ => assert!(loaded.messages_json.contains("\"hi\"")),
        }
    }
    let contexts = db.run_contexts_for(&ids).unwrap();
    assert_eq!(contexts.len(), 3);

    // And: the terminal summary reads the count from the header and only the last message.
    let summaries = db.terminal_summaries(&ids).unwrap();
    let expected_count = u64::try_from(
        serde_json::from_str::<Vec<serde_json::Value>>(&large)
            .unwrap()
            .len(),
    )
    .unwrap();
    assert_eq!(summaries[0].message_count, expected_count);
    assert_eq!(summaries[1].message_count, 1);
    assert_eq!(summaries[1].final_text.as_deref(), Some("hi"));
    assert_eq!(summaries[2].message_count, 0);
    assert_eq!(summaries[2].final_text, None);

    // And: inspection expands the conversation into JSON.
    let inspected = db.inspect_run("run-compressed", true).unwrap();
    assert_eq!(
        inspected["context"]["messages_json"]
            .as_array()
            .unwrap()
            .len(),
        usize::try_from(expected_count).unwrap()
    );
}

#[test]
fn learning_source_limit_counts_plaintext_not_stored_bytes() {
    // Given: a snapshot whose plaintext exceeds the 16 MiB learning limit but compresses far below it.
    let temp = TempDir::new().unwrap();
    let (storage, config) = open(&temp);
    storage
        .handle()
        .upsert_run_context(&record("run-limit", messages(17 * 1024 * 1024)))
        .unwrap();
    storage
        .handle()
        .upsert_run_context(&record("run-ok", messages(1024 * 1024)))
        .unwrap();
    storage.close();
    let db = Database::open(&config).unwrap();

    // When/Then: the oversized one is refused and the small one is read.
    assert!(db.learning_source_contexts("run-limit").is_err());
    assert_eq!(db.learning_source_contexts("run-ok").unwrap().len(), 1);
}
