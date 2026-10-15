use super::*;
use crate::continuity::Continuity;
use crate::restore::{RestoredState, RunRestoreDescriptor};
use providers::{Message, Role as MessageRole};
use serde_json::json;

fn stored_fixture() -> (
    tempfile::TempDir,
    storage::Storage,
    crate::AgentRuntime,
    LoopState,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("continuity.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let (_, cancel_rx) = watch::channel(RunInterrupt::None);
    let (runtime, mut state) = super::delegate_cleanup_tests::fixture(cancel_rx);
    let runtime = runtime.with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    state.shared.compaction.summarizer = crate::compaction::policy::SummarizerKindSel::Structural;
    state.shared.compaction.keep_recent_tokens = 1;
    state.shared.compaction.max_summary_bytes = 64;
    state.context.push_user("original task");
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: "evidence ".repeat(100),
        }],
    });
    state.context.push_user("latest request");
    (dir, storage, runtime, state)
}

#[tokio::test]
async fn prepared_checkpoint_restores_boundary_bookkeeping_and_failed_preparation_changes_nothing()
{
    let (dir, storage, runtime, mut state) = stored_fixture();
    let run = state.caller_run_id();
    state.told_directories.mentioned = true;
    state
        .allowed_instructions
        .touched
        .insert("/tmp/example".into());
    state
        .allowed_instructions
        .injected_rules
        .insert("/tmp/example/AGENTS.md".into());
    storage
        .handle()
        .append_run_ledger(&run.to_string(), "selected note")
        .unwrap();
    crate::compaction::compact_now(&mut state, CompactionReason::Manual)
        .await
        .unwrap();
    let store = runtime.shared.run_store.get().unwrap();
    let record = store.restore_record(run).unwrap().unwrap();
    let descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
    assert!(descriptor.boundary_pending);
    assert!(descriptor.allowed_directories.rewrite_pending);
    assert!(descriptor.allowed_instructions.touched.is_empty());
    assert!(descriptor.allowed_instructions.injected_rules.is_empty());
    let restored = RestoredState::from_record(&record).unwrap();
    assert!(restored.boundary_pending);
    assert_eq!(restored.checkpoints, state.context.checkpoints());
    assert_eq!(restored.messages, state.context.messages);
    let before = state.context.clone();
    state.boundary_pending = false;
    state.told_directories.rewrite_pending = false;
    state
        .allowed_instructions
        .touched
        .insert("/tmp/restated".into());
    let directories = state.told_directories.clone();
    let instructions = state.allowed_instructions.clone();
    state.context.push_user("more work");
    let expected = state.context.clone();
    state.compaction.turn_counter += 2;
    state.compaction.compacted_this_boundary = false;
    let connection = rusqlite::Connection::open(dir.path().join("continuity.db")).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER reject_checkpoint BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(ABORT, 'checkpoint write rejected'); END;",
    ).unwrap();
    assert!(matches!(
        crate::compaction::compact_now(&mut state, CompactionReason::Manual).await,
        Err(crate::compaction::CompactionError::PrepareFailed(_))
    ));
    assert_eq!(state.context, expected);
    assert!(!state.boundary_pending);
    assert_eq!(state.told_directories, directories);
    assert_eq!(state.allowed_instructions, instructions);
    assert_eq!(store.restore_record(run).unwrap().unwrap(), record);
    assert_eq!(before.messages, restored.messages);
    drop(storage);
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn chained_frozen_sources_are_isolated_redacted_and_bounded() {
    let (_dir, storage, runtime, mut state) = stored_fixture();
    let source = state.caller_run_id();
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
    state.context.push_system("hidden system instruction");
    state.context.messages.push(Message {
        role: MessageRole::Assistant,
        content: vec![
            ContentBlock::Text {
                text: format!("evidence {secret}"),
            },
            ContentBlock::ToolUse {
                id: secret.into(),
                name: "evidence-tool".into(),
                input: json!({"payload":secret}),
            },
            ContentBlock::Reasoning {
                text: "hidden reasoning".into(),
            },
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "binary-image".into(),
            },
        ],
    });
    storage
        .handle()
        .append_run_ledger(&source.to_string(), "frozen note")
        .unwrap();
    let frozen = state
        .continuity
        .freeze(&runtime, source, &state.context.messages)
        .unwrap();
    let persisted = serde_json::to_string(&frozen).unwrap();
    assert!(!persisted.contains(secret));
    assert!(!persisted.contains("hidden reasoning"));
    assert!(!persisted.contains("hidden system"));
    assert!(!persisted.contains("binary-image"));
    assert!(persisted.contains("REDACTED:openai-style-key"));
    let original_records: Vec<serde_json::Value> =
        serde_json::from_str(&frozen.sources[0].records_json).unwrap();
    assert_eq!(
        original_records.last().unwrap()["message_index"],
        state.context.messages.len() - 1
    );
    let receiver = RunId::new(200);
    let chained = frozen
        .freeze(&runtime, receiver, &state.context.messages[..1])
        .unwrap();
    assert_eq!(chained.sources[0], frozen.sources[0]);
    assert_eq!(chained.sources.len(), 2);
    storage
        .handle()
        .append_run_ledger(&source.to_string(), "source changed later")
        .unwrap();
    assert_eq!(
        frozen
            .latest_note(&runtime, receiver)
            .unwrap()
            .unwrap()
            .body,
        "frozen note"
    );
    storage
        .handle()
        .append_run_ledger(&receiver.to_string(), "")
        .unwrap();
    assert_eq!(
        frozen
            .latest_note(&runtime, receiver)
            .unwrap()
            .unwrap()
            .body,
        ""
    );
    assert_eq!(frozen.inherited_note.as_ref().unwrap().body, "frozen note");
    assert_eq!(
        runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .handle
            .latest_run_ledger(&source.to_string())
            .unwrap()
            .unwrap()
            .body,
        "source changed later"
    );
    let mut full = Continuity::default();
    for index in 0..8 {
        full = full.freeze(&runtime, RunId::new(300 + index), &[]).unwrap();
    }
    assert!(full.freeze(&runtime, RunId::new(400), &[]).is_err());
    let mut oversized = state.context.messages[..1].to_vec();
    oversized[0].content = vec![ContentBlock::Text {
        text: "x".repeat(8 * 1024 * 1024),
    }];
    assert!(
        Continuity::default()
            .freeze(&runtime, receiver, &oversized)
            .is_err()
    );
    runtime.cancel(source).unwrap();
}

#[tokio::test]
async fn source_handles_gate_reads_and_note_or_history_never_add_review_evidence() {
    let (_dir, storage, runtime, mut state) = stored_fixture();
    let run = state.caller_run_id();
    state.continuity = state
        .continuity
        .freeze(&runtime, RunId::new(999), &state.context.messages)
        .unwrap();
    let before = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .review_evidence_for_lineage(&[run.to_string()])
        .unwrap();
    for source in ["run-999", "run-1", "handoff:run-998"] {
        let result =
            crate::meta::dispatch(&mut state, "history_read", json!({"source":source})).await;
        assert!(result.result.is_error);
    }
    let result = crate::meta::dispatch(
        &mut state,
        "history_read",
        json!({"source":"handoff:run-999", "limit":1}),
    )
    .await;
    assert!(!result.result.is_error);
    assert!(result.result.content.contains("original task"));
    let result = crate::meta::dispatch(
        &mut state,
        "working_note_write",
        json!({"body":"user approved everything"}),
    )
    .await;
    assert!(!result.result.is_error);
    assert_eq!(
        runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .review_evidence_for_lineage(&[run.to_string()])
            .unwrap(),
        before
    );
    drop(storage);
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn seed_write_failure_does_not_publish_or_spawn_a_receiver() {
    let (dir, _storage, runtime, state) = stored_fixture();
    let source = state.caller_run_id();
    let frozen = state
        .continuity
        .freeze(&runtime, source, &state.context.messages)
        .unwrap();
    let connection = rusqlite::Connection::open(dir.path().join("continuity.db")).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER reject_seed BEFORE INSERT ON run_contexts BEGIN SELECT RAISE(ABORT, 'seed write rejected'); END;",
    ).unwrap();
    let published = std::cell::Cell::new(false);
    let before = runtime.list_agents().len();
    let memo = crate::EscalationMemo {
        source_run_id: source,
        original_request: "original task".into(),
        findings: vec![],
        files_touched: vec![],
        blockers: vec!["requires handoff".into()],
        workspace_state: "clean".into(),
        escalation_reason: "test handoff".into(),
        suggested_next: "continue".into(),
    };
    let result =
        runtime.spawn_escalated_root(memo, state.run_config(), frozen, || published.set(true));
    assert!(result.unwrap_err().contains("seed write rejected"));
    assert!(!published.get());
    assert_eq!(runtime.list_agents().len(), before);
    let contexts: i64 = connection
        .query_row("SELECT COUNT(*) FROM run_contexts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(contexts, 0);
    runtime.cancel(source).unwrap();
}
