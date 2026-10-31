use std::path::PathBuf;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::StorageConfig;

fn create(dir: &TempDir) -> (PathBuf, Database) {
    let path = dir.path().join("storage.db");
    let db = Database::open(&StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    (path, db)
}

fn event(db: &Database, kind: &str, subkind: Option<&str>, run: Option<&str>, bytes: usize) {
    db.conn
        .execute(
            "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind, run_id)
             VALUES ('s', 1, 0, 0, ?1, ?2, ?3, ?4)",
            rusqlite::params![kind, "p".repeat(bytes), subkind, run],
        )
        .unwrap();
}

fn context(db: &Database, run: &str, messages: rusqlite::types::Value, plain_len: i64) {
    db.conn
        .execute(
            "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, messages_json,
                 checkpoints_json, terminal_phase, restorable, updated_at_ns, host_inputs_json,
                 messages_len, checkpoints_len)
             VALUES (?1, 'worker', ?1, NULL, '{}', ?2, '[]', 'Done', 0, 0, '[]', ?3, 2)",
            rusqlite::params![run, messages, plain_len],
        )
        .unwrap();
}

fn index_run(db: &Database, run: &str, state: &str, charged: i64, text: i64, blocks: i64) {
    db.conn
        .execute(
            "INSERT INTO thread_index_runs(run_id, state, source_revision, snapshot_revision,
                 source_context_sha256, extractor_version, accounting_version, source_updated_at_ns,
                 retention_at_ns, text_bytes, block_count, charged_bytes)
             VALUES (?1, ?2, 1, 1, 'sha', 1, 1, 0, 0, ?3, ?4, ?5)",
            rusqlite::params![run, state, text, blocks, charged],
        )
        .unwrap();
}

/// Two threads (one archived) with a run each, plus a run no thread claims.
fn seed(db: &Database) {
    for (thread, archived) in [("t-active", 0), ("t-archived", 1)] {
        db.conn
            .execute(
                "INSERT INTO threads(thread_id, archived, created_at_ns, updated_at_ns, source)
                 VALUES (?1, ?2, 0, 0, 'runtime')",
                rusqlite::params![thread, archived],
            )
            .unwrap();
    }
    for (run, thread) in [("r-a", "t-active"), ("r-b", "t-archived")] {
        db.conn
            .execute(
                "INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth) VALUES (?1, ?2, ?1, 0)",
                rusqlite::params![run, thread],
            )
            .unwrap();
    }
    event(db, "Lifecycle", Some("Started"), Some("r-a"), 100);
    event(db, "Lifecycle", Some("Started"), Some("r-a"), 100);
    event(db, "Message", Some("Assistant"), Some("r-a"), 1000);
    event(db, "Message", Some("Assistant"), Some("r-b"), 3000);
    event(db, "Diagnostic", Some("Warning"), Some("r-x"), 50);
    event(db, "Lifecycle", Some("Started"), None, 10);
    event(db, "Tool", None, None, 7);

    // Secret text in a snapshot must never reach the summary.
    context(db, "r-a", ("SECRET-BODY".repeat(50)).into(), 550);
    context(db, "r-b", rusqlite::types::Value::Blob(vec![0; 1500]), 6000);
    context(db, "r-x", "[]".to_owned().into(), 2);

    index_run(db, "r-a", "indexed", 700, 300, 3);
    index_run(db, "r-b", "pending", 200, 100, 1);
    index_run(db, "r-x", "indexed", 50, 20, 1);
}

fn summary(path: &std::path::Path, options: StorageSummaryOptions) -> Value {
    Database::open_read_only(path)
        .unwrap()
        .storage_summary(path, options)
        .unwrap()
}

#[test]
fn logical_summary_matches_a_fixed_fixture() {
    let dir = TempDir::new().unwrap();
    let (path, db) = create(&dir);
    seed(&db);
    drop(db);

    let summary = summary(&path, StorageSummaryOptions::default());

    assert_eq!(
        summary["events"],
        json!({
            "rows": 7,
            "payload_bytes": 4267,
            "unbackfilled_rows": 1,
            "by_kind": [
                { "kind": "Message", "subkind": "Assistant", "rows": 2, "payload_bytes": 4000 },
                { "kind": "Lifecycle", "subkind": "Started", "rows": 3, "payload_bytes": 210 },
                { "kind": "Diagnostic", "subkind": "Warning", "rows": 1, "payload_bytes": 50 },
                { "kind": "Tool", "subkind": "(unbackfilled)", "rows": 1, "payload_bytes": 7 },
            ],
        })
    );
    assert_eq!(
        summary["run_contexts"],
        json!({
            "rows": 3,
            "messages": { "plain_bytes": 6552, "stored_bytes": 2052, "compressed_rows": 1 },
            "checkpoints": { "plain_bytes": 6, "stored_bytes": 6, "compressed_rows": 0 },
            "config_bytes": 6,
            "host_inputs_bytes": 6,
        })
    );
    assert_eq!(
        summary["conversation_index"],
        json!({
            "runs": 3,
            "charged_bytes": 950,
            "by_state": {
                "indexed": { "runs": 2, "charged_bytes": 750, "text_bytes": 320, "blocks": 4 },
                "pending": { "runs": 1, "charged_bytes": 200, "text_bytes": 100, "blocks": 1 },
            },
        })
    );
    assert_eq!(
        summary["threads"],
        json!({ "total": 2, "active": 1, "archived": 1, "superseded": 0 })
    );
    assert_eq!(
        summary["attribution"],
        json!({
            "events": {
                "active": { "rows": 3, "bytes": 1200 },
                "archived": { "rows": 1, "bytes": 3000 },
                "orphan": { "rows": 1, "bytes": 50 },
                "unattributed": { "rows": 2, "bytes": 17 },
            },
            "run_contexts": {
                "active": { "rows": 1, "bytes": 556 },
                "archived": { "rows": 1, "bytes": 1506 },
                "orphan": { "rows": 1, "bytes": 8 },
                "unattributed": { "rows": 0, "bytes": 0 },
            },
            "conversation_index": {
                "active": { "rows": 1, "bytes": 700 },
                "archived": { "rows": 1, "bytes": 200 },
                "orphan": { "rows": 1, "bytes": 50 },
                "unattributed": { "rows": 0, "bytes": 0 },
            },
        })
    );
    assert_eq!(
        summary["top_threads"],
        json!([
            {
                "thread_id": "t-archived", "archived": true,
                "events": { "rows": 1, "payload_bytes": 3000 },
                "run_contexts": { "rows": 1, "stored_bytes": 1506 },
                "conversation_index_charged_bytes": 200,
                "total_bytes": 4706,
            },
            {
                "thread_id": "t-active", "archived": false,
                "events": { "rows": 3, "payload_bytes": 1200 },
                "run_contexts": { "rows": 1, "stored_bytes": 556 },
                "conversation_index_charged_bytes": 700,
                "total_bytes": 2456,
            },
        ])
    );

    let tables = summary["tables"].as_array().unwrap();
    let table = |name: &str| tables.iter().find(|t| t["table"] == name).cloned();
    // run_id + thread_id + root_run_id: "r-a" "t-active" "r-a" and "r-b" "t-archived" "r-b".
    assert_eq!(
        table("thread_runs"),
        Some(json!({ "table": "thread_runs", "rows": 2, "value_bytes": 30 }))
    );
    assert_eq!(table("events").unwrap()["rows"], 7);
    assert!(
        table("conversation_fts").is_none(),
        "virtual tables are skipped"
    );
    assert!(table("conversation_fts_data").is_some());
    let sizes: Vec<_> = tables
        .iter()
        .map(|t| t["value_bytes"].as_i64().unwrap())
        .collect();
    assert!(
        sizes.is_sorted_by(|a, b| a >= b),
        "largest first: {sizes:?}"
    );

    let file_bytes = std::fs::metadata(&path).unwrap().len();
    assert_eq!(summary["files"]["db_bytes"], file_bytes);
    assert_eq!(
        summary["files"]["total_bytes"],
        summary["files"]["db_bytes"].as_u64().unwrap()
            + summary["files"]["wal_bytes"].as_u64().unwrap()
            + summary["files"]["shm_bytes"].as_u64().unwrap()
    );
    assert!(summary.get("physical").is_none());
    assert!(!summary.to_string().contains("SECRET-BODY"));
}

#[test]
fn top_limits_threads_and_clamps_to_at_least_one() {
    let dir = TempDir::new().unwrap();
    let (path, db) = create(&dir);
    seed(&db);
    drop(db);

    for top in [0, 1] {
        let summary = summary(
            &path,
            StorageSummaryOptions {
                top,
                physical: false,
            },
        );
        let threads = summary["top_threads"].as_array().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0]["thread_id"], "t-archived");
    }
}

#[test]
fn physical_summary_reports_pages_files_and_pragmas() {
    let dir = TempDir::new().unwrap();
    let (path, db) = create(&dir);
    seed(&db);
    // Bulk rows that are then deleted leave free pages behind.
    for _ in 0..64 {
        event(&db, "Bulk", Some("Churn"), None, 4000);
    }
    db.conn
        .execute("DELETE FROM events WHERE kind = 'Bulk'", [])
        .unwrap();
    drop(db);

    let summary = summary(
        &path,
        StorageSummaryOptions {
            top: 3,
            physical: true,
        },
    );
    let physical = &summary["physical"];

    let page_size = physical["page_size"].as_i64().unwrap();
    let page_count = physical["page_count"].as_i64().unwrap();
    let freelist = physical["freelist_count"].as_i64().unwrap();
    assert_eq!(page_size, 4096);
    assert!(freelist > 0, "deleting bulk rows must free pages");
    assert_eq!(physical["freelist_bytes"], freelist * page_size);
    assert_eq!(
        physical["auto_vacuum"],
        json!({ "value": 2, "mode": "incremental" })
    );
    assert_eq!(
        physical["user_version"],
        u32::try_from(crate::migrations::MIGRATIONS.len()).unwrap()
    );
    // The last connection closing checkpointed the WAL, so the file holds every page.
    assert_eq!(summary["files"]["db_bytes"], page_count * page_size);

    let tables = physical["tables"].as_array().unwrap();
    let btree: i64 = tables
        .iter()
        .map(|t| t["total_bytes"].as_i64().unwrap())
        .sum();
    assert_eq!(physical["btree_bytes"], btree);
    // Pointer-map pages (incremental auto_vacuum) and the freelist are outside b-trees.
    assert!(btree + freelist * page_size <= page_count * page_size);
    for table in tables {
        assert_eq!(
            table["total_bytes"].as_i64().unwrap(),
            table["table_bytes"].as_i64().unwrap() + table["index_bytes"].as_i64().unwrap()
        );
        let objects: i64 = table["objects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["bytes"].as_i64().unwrap())
            .sum();
        assert_eq!(objects, table["total_bytes"].as_i64().unwrap());
    }

    let group = |name: &str| tables.iter().find(|t| t["table"] == name).unwrap();
    let events = group("events");
    assert!(events["table_bytes"].as_i64().unwrap() >= 4096);
    assert!(events["index_bytes"].as_i64().unwrap() >= 4096);
    let kind_of = |group: &Value, name: &str| {
        group["objects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["name"] == name)
            .map(|o| o["kind"].clone())
    };
    assert_eq!(kind_of(events, "events"), Some(json!("table")));
    assert_eq!(kind_of(events, "idx_events_run_id"), Some(json!("index")));
    // FTS shadow tables are charged to their virtual table.
    assert_eq!(
        kind_of(group("conversation_fts"), "conversation_fts_data"),
        Some(json!("table"))
    );
    assert!(tables.iter().any(|t| t["table"] == "sqlite_schema"));
    let sizes: Vec<_> = tables
        .iter()
        .map(|t| t["total_bytes"].as_i64().unwrap())
        .collect();
    assert!(
        sizes.is_sorted_by(|a, b| a >= b),
        "largest first: {sizes:?}"
    );
}

#[test]
fn empty_store_reports_zeroes() {
    let dir = TempDir::new().unwrap();
    let (path, db) = create(&dir);
    drop(db);

    let summary = summary(&path, StorageSummaryOptions::default());

    assert_eq!(summary["events"]["rows"], 0);
    assert_eq!(summary["events"]["by_kind"], json!([]));
    assert_eq!(summary["top_threads"], json!([]));
    assert_eq!(summary["threads"]["total"], 0);
    assert_eq!(summary["conversation_index"]["by_state"], json!({}));
    assert_eq!(
        summary["attribution"]["events"]["unattributed"],
        json!({ "rows": 0, "bytes": 0 })
    );
}
