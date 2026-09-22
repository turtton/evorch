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
