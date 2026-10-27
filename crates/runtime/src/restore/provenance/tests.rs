use super::*;
use providers::ContentBlock;
use storage::thread_index::{BlockLocator, extract_snapshot_bundle};

fn message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

fn record(run: &str, messages: &[Message], checkpoints: &str) -> storage::RunContextRecord {
    storage::RunContextRecord {
        run_id: run.into(),
        role: "worker".into(),
        name: "worker".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: serde_json::to_string(messages).unwrap(),
        checkpoints_json: checkpoints.into(),
        terminal_phase: "Done".into(),
        restorable: true,
        updated_at_ns: 10,
        host_inputs_json: "[]".into(),
    }
}

#[test]
fn continuity_positions_exclude_only_the_inserted_copy_not_identical_native_text() {
    let continuity = crate::continuity::Continuity {
        inherited_note: Some(crate::continuity::WorkingNote {
            note_id: "source".into(),
            revision: 3,
            writer_run_id: "source".into(),
            created_at_ns: 1,
            body: "same utterance".into(),
        }),
        sources: Vec::new(),
    };
    let mut messages = vec![
        message(Role::System, "instructions"),
        message(Role::User, "same utterance"),
    ];
    messages.push(Message {
        role: Role::User,
        content: continuity
            .blocks(continuity.inherited_note.as_ref())
            .unwrap(),
    });
    let mut provenance = ConversationProvenance::default();
    provenance.continuity(&messages, &continuity);
    messages.push(message(Role::User, "same utterance"));
    messages.insert(0, message(Role::System, "restored workspace"));
    let write = provenance.write(record("run-1", &messages, "[]")).unwrap();
    assert!(matches!(
        write.map.messages[1].blocks[1].origin,
        BlockOrigin::ContinuityCopy { .. }
    ));
    assert_eq!(write.map.messages[2].blocks[0].origin, BlockOrigin::Native);
    assert_eq!(write.map.messages[2].raw_index, 4);
    assert_eq!(write.map.messages[2].ordinal, 2);
}

#[test]
fn fork_compaction_restart_and_failed_save_keep_native_duplicate_positions() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("provenance.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    let mut messages = vec![
        message(Role::System, "instructions"),
        message(Role::User, "duplicate"),
        message(Role::Assistant, "done"),
    ];
    let native = ConversationProvenance::default();
    store
        .handle()
        .save_snapshot_bundle(&native.write(record("run-1", &messages, "[]")).unwrap())
        .unwrap();
    let reader = storage::Database::open(&config).unwrap();
    let source = reader.snapshot_bundle("run-1").unwrap().unwrap();
    let mut copied = ConversationProvenance::from_bundle(&source);
    copied.fork(&source, 2);
    messages.push(message(Role::User, "duplicate"));
    messages.push(message(Role::Assistant, "done"));
    let checkpoint = r#"[{"id":"compacted","summary":{"role":"user","content":[{"type":"text","text":"generated summary"}]},"range":[1,3],"protects_history":false}]"#;
    let write = copied
        .write(record("run-2", &messages, checkpoint))
        .unwrap();
    let receipt = store.handle().save_snapshot_bundle(&write).unwrap();
    drop(reader);
    let reopened = storage::Database::open(&config).unwrap();
    let bundle = reopened.snapshot_bundle("run-2").unwrap().unwrap();
    let restored = ConversationProvenance::from_bundle(&bundle);
    let mut stale = restored
        .write(record("run-2", &messages, checkpoint))
        .unwrap();
    stale.expected_snapshot_revision = Some(receipt.snapshot_revision + 1);
    assert!(store.handle().save_snapshot_bundle(&stale).is_err());
    assert_eq!(reopened.snapshot_bundle("run-2").unwrap().unwrap(), bundle);
    let injection = rusqlite::Connection::open(&config.db_path).unwrap();
    injection
        .execute_batch(
            "CREATE TRIGGER fail_w4_provenance BEFORE UPDATE ON run_context_provenance
         WHEN NEW.run_id='run-2' BEGIN SELECT RAISE(ABORT,'w4 injected mid-save failure'); END;",
        )
        .unwrap();
    let mut interrupted = write.clone();
    interrupted.record.updated_at_ns += 1;
    assert!(store.handle().save_snapshot_bundle(&interrupted).is_err());
    assert_eq!(reopened.snapshot_bundle("run-2").unwrap().unwrap(), bundle);
    injection
        .execute_batch("DROP TRIGGER fail_w4_provenance")
        .unwrap();
    let input =
        extract_snapshot_bundle(&bundle, &secret_guard::SecretRedactor::from_env()).unwrap();
    assert_eq!(input.blocks.len(), 2);
    assert_eq!(
        input.blocks[0].locator,
        BlockLocator::Message {
            ordinal: 2,
            block_index: 0
        }
    );
    assert_eq!(input.blocks[0].text, "duplicate");
    messages.push(message(Role::User, "duplicate"));
    let resaved = restored
        .write(record("run-2", &messages, checkpoint))
        .unwrap();
    assert!(matches!(
        resaved.map.messages[0].blocks[0].origin,
        BlockOrigin::ForkCopy { .. }
    ));
    assert_eq!(
        resaved.map.messages[4].blocks[0].origin,
        BlockOrigin::Native
    );
}

#[test]
fn restore_preserves_the_unknown_reason_and_never_upgrades_after_append() {
    let messages = vec![message(Role::User, "native-looking")];
    let saved_record = record("run-1", &messages, "[]");
    let hash = storage::thread_index::snapshot_sha256(
        &saved_record.messages_json,
        &saved_record.checkpoints_json,
    );
    let bundle = SnapshotBundle {
        record: saved_record,
        snapshot_revision: 1,
        source_revision: 1,
        latest_note: None,
        provenance: Some(storage::thread_index::SnapshotProvenance {
            run_id: "run-1".into(),
            snapshot_revision: 1,
            snapshot_sha256: hash,
            format_version: PROVENANCE_FORMAT_VERSION,
            status: ProvenanceStatus::Unknown {
                reason: ProvenanceUnknownReason::IncompleteMap,
            },
            map: ProvenanceMap {
                version: PROVENANCE_FORMAT_VERSION,
                raw_message_count: 1,
                checkpoint_count: 0,
                messages: Vec::new(),
            },
        }),
    };
    let restored = ConversationProvenance::from_bundle(&bundle);
    let mut appended = messages;
    appended.push(message(Role::User, "new native"));
    let write = restored.write(record("run-1", &appended, "[]")).unwrap();
    assert_eq!(
        write.status,
        ProvenanceStatus::Unknown {
            reason: ProvenanceUnknownReason::IncompleteMap
        }
    );
}
