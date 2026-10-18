use serde_json::{Value, json};
use storage::{Database, RunContextRecord, Storage, StorageConfig};

const SECRET: &str = "sk-test-evorch-9f8e7d6c5b4a3f2e1d";

fn context() -> RunContextRecord {
    RunContextRecord {
        run_id: "run-1".into(), role: "Worker".into(), name: "chat:Worker:test".into(),
        parent_run_id: None, config_json: "{}".into(),
        messages_json: json!([
            {"role":"user", "content":[{"type":"text","text":"read fixture"}]},
            {"role":"assistant", "content":[{"type":"tool_use","id":"call-1","name":"read","input":{"path":"fixture.rs","description":SECRET}}]},
            {"role":"user", "content":[{"type":"tool_result","tool_call_id":"call-1","is_error":false,"content":[{"type":"text","text":format!("const TEST_KEY: &str = \"{SECRET}\";")}]}]}
        ]).to_string(),
        checkpoints_json: json!([{"id":"checkpoint-1", "range":[0,3], "summary":{"role":"user","content":[{"type":"text","text":SECRET}]}}]).to_string(),
        terminal_phase: "Checkpoint".into(), restorable: true, updated_at_ns: 1,
    }
}

#[test]
fn tool_fixture_and_summary_are_redacted_without_losing_restore_structure() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("restore.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let record = context();
    storage.handle().upsert_run_context(&record).unwrap();
    let db = Database::open(&config).unwrap();
    let saved = db
        .latest_terminal_run_context(&record.name)
        .unwrap()
        .unwrap();
    assert!(saved.restorable);
    assert!(!saved.messages_json.contains(SECRET));
    assert!(!saved.checkpoints_json.contains(SECRET));
    assert!(
        record.messages_json.contains(SECRET),
        "live/source copy stays unchanged"
    );
    let messages: Value = serde_json::from_str(&saved.messages_json).unwrap();
    assert_eq!(messages[1]["content"][0]["id"], "call-1");
    assert_eq!(messages[2]["content"][0]["tool_call_id"], "call-1");
    assert_eq!(messages[2]["role"], "user");
    assert!(
        messages[2]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[REDACTED:openai-style-key]")
    );
}

#[test]
fn tool_result_images_are_stored_byte_exact_while_text_is_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("image.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut record = context();
    let image = json!({"type":"image","media_type":"image/png","data":"AKIAIOSFODNN7EXAMPLE","source":"/tmp/shot.png"});
    record.messages_json = json!([{"role":"user","content":[{"type":"tool_result","tool_call_id":"call-1","is_error":false,"content":[{"type":"text","text":SECRET}, image]}]}]).to_string();
    storage.handle().upsert_run_context(&record).unwrap();
    let saved = Database::open(&config)
        .unwrap()
        .run_context("run-1")
        .unwrap()
        .unwrap();
    let messages: Value = serde_json::from_str(&saved.messages_json).unwrap();
    let parts = &messages[0]["content"][0]["content"];
    assert!(!parts[0].to_string().contains(SECRET));
    assert_eq!(parts[1], image);
}

#[test]
fn failed_update_retains_the_last_valid_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("restore.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut record = context();
    storage.handle().upsert_run_context(&record).unwrap();
    let db = Database::open(&config).unwrap();
    let prior = db.run_context("run-1").unwrap().unwrap();
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    conn.execute_batch("CREATE TRIGGER deny_context_update BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(ABORT, 'fixture write failure'); END;").unwrap();
    record.terminal_phase = "Error".into();
    record.updated_at_ns = 2;
    assert!(storage.handle().upsert_run_context(&record).is_err());
    assert_eq!(db.run_context("run-1").unwrap().unwrap(), prior);
}

#[test]
fn anthropic_payloads_redact_plaintext_and_preserve_opaque_signatures() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("anthropic.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut record = context();
    let blocks = json!([
        {"type":"anthropic","prefix_hash":"prefix","block":{"type":"thinking","thinking":"safe reasoning","signature":SECRET}},
        {"type":"anthropic","block":{"type":"redacted_thinking","data":SECRET}},
        {"type":"anthropic","block":{"type":"compaction","content":"safe summary","signature":SECRET,"encrypted_content":SECRET}},
        {"type":"anthropic","block":{"type":"text","text":SECRET,"citations":[{"cited_text":SECRET}]}},
        {"type":"anthropic","block":{"type":"server_tool_use","id":"server-1","input":{"query":SECRET}}},
        {"type":"anthropic","block":{"type":"web_search_tool_result","content":[{"title":SECRET}]}},
        {"type":"anthropic","block":{"type":"future_block","payload":SECRET}}
    ]);
    record.messages_json = json!([{"role":"assistant","content":blocks}]).to_string();
    record.checkpoints_json = "[]".into();
    storage.handle().upsert_run_context(&record).unwrap();
    let db = Database::open(&config).unwrap();
    let saved = db.run_context("run-1").unwrap().unwrap();
    let messages: Value = serde_json::from_str(&saved.messages_json).unwrap();
    let persisted = &messages[0]["content"];
    assert_eq!(persisted[0], blocks[0]);
    assert_eq!(persisted[1], blocks[1]);
    assert_eq!(persisted[2], blocks[2]);
    for block in persisted.as_array().unwrap().iter().skip(3) {
        assert!(!block.to_string().contains(SECRET));
        assert!(block.to_string().contains("[REDACTED:openai-style-key]"));
    }
}

#[test]
fn signed_anthropic_secret_rejects_snapshot_without_overwriting_prior_context() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("signed.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut record = context();
    storage.handle().upsert_run_context(&record).unwrap();
    let db = Database::open(&config).unwrap();
    let prior = db.run_context("run-1").unwrap().unwrap();
    for (signed, in_checkpoint) in [
        json!({"type":"anthropic","block":{"type":"thinking","thinking":SECRET,"signature":"unchanged-signature"}}),
        json!({"type":"anthropic","block":{"type":"compaction","content":SECRET,"signature":"unchanged-signature","encrypted_content":"unchanged-encrypted-content"}}),
    ].into_iter().flat_map(|signed| [false, true].map(|in_checkpoint| (signed.clone(), in_checkpoint))) {
        record.messages_json = "[]".into();
        record.checkpoints_json = "[]".into();
        let message = json!({"role":"assistant","content":[signed.clone()]});
        if in_checkpoint {
            record.checkpoints_json = json!([{"summary":message}]).to_string();
        } else {
            record.messages_json = json!([message]).to_string();
        }
        let error = storage.handle().upsert_run_context(&record).unwrap_err();
        assert!(matches!(error, storage::StorageError::Serialization(_)));
        assert!(!error.to_string().contains(SECRET));
        assert_eq!(db.run_context("run-1").unwrap().unwrap(), prior);
        record.run_id = "new-run".into();
        assert!(storage.handle().upsert_run_context(&record).is_err());
        assert!(db.run_context("new-run").unwrap().is_none());
        record.run_id = "run-1".into();
    }
}

#[test]
fn persisted_tool_event_copy_masks_arguments_output_and_metadata() {
    use event_bus::{Event, ToolEvent};
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let events = [
        Event::new(ToolEvent::ToolStarted {
            tool_name: "edit".into(),
            call_id: "call-1".into(),
            input: Some(json!({"new_string":SECRET})),
            run_id: Some("run-1".into()),
        }),
        Event::new(ToolEvent::ToolCompleted {
            tool_name: "read".into(),
            call_id: "call-1".into(),
            is_error: false,
            output: Some(SECRET.into()),
            detail: Some(json!({"source":SECRET})),
            run_id: Some("run-1".into()),
        }),
    ];
    for event in &events {
        storage.handle().append_event(None, event).unwrap();
    }
    let db = Database::open(&config).unwrap();
    for event in db.events_all_ordered().unwrap() {
        let persisted = serde_json::to_string(&event.event).unwrap();
        assert!(!persisted.contains(SECRET));
        assert!(persisted.contains("[REDACTED:openai-style-key]"));
        assert!(persisted.contains("call-1"));
    }
    assert!(serde_json::to_string(&events[1]).unwrap().contains(SECRET));
}
