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
        host_inputs_json: "[]".into(),
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
            parent_call_id: None,
        }),
        Event::new(ToolEvent::ToolCompleted {
            tool_name: "read".into(),
            call_id: "call-1".into(),
            is_error: false,
            output: Some(SECRET.into()),
            detail: Some(json!({"source":SECRET})),
            run_id: Some("run-1".into()),
            parent_call_id: None,
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

mod responses_reasoning {
    use super::*;
    use providers::{
        ContentBlock, Message, ResolvedResponsesScope, ResponsesAccount, ResponsesDialect,
        ResponsesSpeed, Role,
    };

    const ITEM_ID: &str = "rs_b3_storage_item";
    const SUMMARY: &str = "b3 plaintext reasoning summary";

    /// A ciphertext that the heuristic guard would flag if it were scanned.
    fn ciphertext() -> String {
        format!("gAAAAB3storage/{SECRET}/QUJDRA==")
    }

    fn origin() -> ResolvedResponsesScope {
        ResolvedResponsesScope {
            dialect: ResponsesDialect::Codex,
            profile: "work".into(),
            route_model: "gpt-5.1-codex+fast".into(),
            wire_model: "gpt-5.1-codex".into(),
            speed: ResponsesSpeed::Fast,
            service_tier: None,
            endpoint_fingerprint: "ab".repeat(32),
            account: ResponsesAccount::Codex {
                identity: "account-b3".into(),
            },
        }
    }

    fn item(ciphertext: &str) -> Value {
        json!({
            "type": "reasoning",
            "id": ITEM_ID,
            "summary": [{"type": "summary_text", "text": SUMMARY}],
            "encrypted_content": ciphertext,
            "status": "completed",
            "future_field": {"nested": ["kept", 7], "flag": true}
        })
    }

    fn assistant(item: Value) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Reasoning {
                    text: SUMMARY.into(),
                },
                ContentBlock::ResponsesReasoning {
                    item,
                    origin: origin(),
                },
                ContentBlock::Text {
                    text: "visible answer".into(),
                },
            ],
        }
    }

    fn open(dir: &tempfile::TempDir) -> (StorageConfig, Storage, Database) {
        let config = StorageConfig {
            db_path: dir.path().join("responses.db"),
            ..StorageConfig::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let db = Database::open(&config).unwrap();
        (config, storage, db)
    }

    #[test]
    fn native_item_round_trips_exactly_even_when_ciphertext_looks_like_a_secret() {
        // Given: a ciphertext the guard would flag and an item with unknown fields.
        let ciphertext = ciphertext();
        assert!(
            secret_guard::SecretRedactor::with_known_values([])
                .detect(&ciphertext)
                .is_some(),
            "fixture must exercise the protected-ciphertext path"
        );
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "question".into(),
                }],
            },
            assistant(item(&ciphertext)),
        ];
        let dir = tempfile::tempdir().unwrap();
        let (_config, storage, db) = open(&dir);
        let mut record = context();
        record.messages_json = serde_json::to_string(&messages).unwrap();
        record.checkpoints_json = json!([{
            "id": "checkpoint-1",
            "range": [0, 1],
            "summary": serde_json::to_value(assistant(item(&ciphertext))).unwrap()
        }])
        .to_string();

        // When: the snapshot is saved and read back.
        storage.handle().upsert_run_context(&record).unwrap();
        let saved = db.run_context("run-1").unwrap().unwrap();

        // Then: the canonical messages, including unknown fields and the exact
        // ciphertext, survive byte-for-byte in value terms.
        let restored: Vec<Message> = serde_json::from_str(&saved.messages_json).unwrap();
        assert_eq!(restored, messages);
        let checkpoints: Value = serde_json::from_str(&saved.checkpoints_json).unwrap();
        assert_eq!(
            checkpoints[0]["summary"],
            serde_json::to_value(assistant(item(&ciphertext))).unwrap()
        );
        assert_eq!(
            restored[1].content[1],
            ContentBlock::ResponsesReasoning {
                item: item(&ciphertext),
                origin: origin(),
            }
        );
        assert!(!saved.messages_json.contains("[REDACTED"));
    }

    #[test]
    fn plaintext_secret_rejects_snapshot_without_rewriting_the_native_item() {
        // Given: a valid prior snapshot.
        let dir = tempfile::tempdir().unwrap();
        let (_config, storage, db) = open(&dir);
        let mut record = context();
        storage.handle().upsert_run_context(&record).unwrap();
        let prior = db.run_context("run-1").unwrap().unwrap();
        let ciphertext = "gAAAAB3clean-ciphertext==";
        let mut summary_secret = item(ciphertext);
        summary_secret["summary"][0]["text"] = json!(format!("leaked {SECRET}"));
        let mut content_secret = item(ciphertext);
        content_secret["content"] = json!([{"type": "reasoning_text", "text": SECRET}]);
        let mut unknown_value = item(ciphertext);
        unknown_value["future_field"]["nested"][0] = json!(SECRET);
        let mut unknown_key = item(ciphertext);
        unknown_key["future_field"][SECRET] = json!(1);
        let mut non_string_ciphertext = item(ciphertext);
        non_string_ciphertext["encrypted_content"] = json!({"inner": SECRET});
        let cases = [
            summary_secret,
            content_secret,
            unknown_value,
            unknown_key,
            non_string_ciphertext,
        ]
        .map(|item| serde_json::to_value(assistant(item)).unwrap());
        let mut origin_secret = serde_json::to_value(assistant(item(ciphertext))).unwrap();
        origin_secret["content"][1]["origin"]["profile"] = json!(SECRET);

        for message in cases.into_iter().chain([origin_secret]) {
            for in_checkpoint in [false, true] {
                record.messages_json = "[]".into();
                record.checkpoints_json = "[]".into();
                if in_checkpoint {
                    record.checkpoints_json =
                        json!([{"id": "c", "range": [0, 0], "summary": message}]).to_string();
                } else {
                    record.messages_json = json!([message]).to_string();
                }

                // When: the snapshot is saved.
                let error = storage.handle().upsert_run_context(&record).unwrap_err();

                // Then: the save is refused without echoing the item and the prior
                // snapshot stays intact; a new run is not created either.
                assert!(matches!(error, storage::StorageError::Serialization(_)));
                let rendered = format!("{error} / {error:?}");
                for private in [SECRET, ciphertext, ITEM_ID, SUMMARY, "future_field"] {
                    assert!(!rendered.contains(private), "diagnostic leaked {private:?}");
                }
                assert_eq!(db.run_context("run-1").unwrap().unwrap(), prior);
                record.run_id = "new-run".into();
                assert!(storage.handle().upsert_run_context(&record).is_err());
                assert!(db.run_context("new-run").unwrap().is_none());
                record.run_id = "run-1".into();
            }
        }
    }

    #[test]
    fn legacy_reasoning_is_still_redacted_next_to_a_clean_native_item() {
        // Given: a pre-B-3 plain reasoning block holding a secret next to a clean item.
        let dir = tempfile::tempdir().unwrap();
        let (_config, storage, db) = open(&dir);
        let mut record = context();
        let mut message = assistant(item("gAAAAB3clean-ciphertext=="));
        message.content[0] = ContentBlock::Reasoning {
            text: format!("legacy {SECRET}"),
        };
        record.messages_json = serde_json::to_string(&[message.clone()]).unwrap();
        record.checkpoints_json = "[]".into();

        // When: it is saved.
        storage.handle().upsert_run_context(&record).unwrap();

        // Then: the legacy block keeps the existing redaction and the item is exact.
        let saved = db.run_context("run-1").unwrap().unwrap();
        let restored: Vec<Message> = serde_json::from_str(&saved.messages_json).unwrap();
        assert_eq!(
            restored[0].content[0],
            ContentBlock::Reasoning {
                text: "legacy [REDACTED:openai-style-key]".into()
            }
        );
        assert_eq!(restored[0].content[1..], message.content[1..]);
    }
}
