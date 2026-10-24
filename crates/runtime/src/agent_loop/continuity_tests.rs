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

async fn denied(state: &mut LoopState, source: &str) -> String {
    let result = crate::meta::dispatch(state, "history_read", json!({"source":source})).await;
    assert!(result.result.is_error);
    assert!(
        result.result.content.starts_with("history_source_denied: "),
        "{}",
        result.result.content
    );
    result.result.content
}

#[tokio::test]
async fn source_handles_gate_reads_and_note_or_history_never_add_review_evidence() {
    let (_dir, storage, runtime, mut state) = stored_fixture();
    let run = state.caller_run_id();
    let before = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .review_evidence_for_lineage(&[run.to_string()])
        .unwrap();
    assert!(
        denied(&mut state, "escalation-run-999")
            .await
            .contains("no handoff handles")
    );
    state.continuity = state
        .continuity
        .freeze(&runtime, RunId::new(999), &state.context.messages)
        .unwrap();
    assert!(
        denied(&mut state, "escalation-run-999")
            .await
            .contains(r#"did you mean "handoff:run-999"?"#)
    );
    assert!(
        denied(&mut state, &run.to_string())
            .await
            .contains(r#"did you mean "self"?"#)
    );
    for source in ["run-999", "run-1", "handoff:run-998"] {
        denied(&mut state, source).await;
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

#[tokio::test(flavor = "current_thread")]
async fn checkpoint_preflight_observes_stop_and_cancel_before_finalization_drains_active() {
    use crate::ownership::{Lease, OwnerCheckpoint, OwnerPermit, Registry, ThreadOwner};
    for (interrupt, expected) in [
        (RunInterrupt::Stop, AgentRunPhase::Stopped),
        (RunInterrupt::Cancel, AgentRunPhase::Error),
    ] {
        let (dir, _storage, runtime, mut state) = stored_fixture();
        let run = state.caller_run_id();
        state.transition(AgentRunPhase::Running, None).unwrap();
        let (cancel, cancel_rx) = watch::channel(RunInterrupt::None);
        state.channels.cancel_rx = cancel_rx;
        let path = dir.path().join("owners.db");
        let owner = ThreadOwner::new(
            "thread".into(),
            Lease {
                owner_id: "owner".into(),
                generation: 1,
                expires_at: u64::MAX,
            },
        );
        Registry::open(&path).unwrap().start(&owner).unwrap();
        let permit = OwnerPermit {
            registry_path: path.clone(),
            thread_id: owner.thread_id,
            lease: owner.lease,
            run_id: Some(run.to_string()),
        };
        permit.begin_turn().unwrap();
        state.task.config.ownership = Some(permit);
        let lock = rusqlite::Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        cancel.send_replace(interrupt);
        assert!(!state.checkpoint_preflight().await);
        assert_eq!(state.phase(), expected);
        drop(lock);
        let owner = Registry::open_readonly(&path)
            .unwrap()
            .attach("thread")
            .unwrap();
        assert!(
            owner.active_runs.contains(&run.to_string()),
            "preflight commits nothing"
        );
        // The interrupt remains set. Finalization must ignore it, persist the
        // terminal snapshot and checkpoint, and only then drain this run.
        state.finalize().await;
        assert_eq!(state.phase(), expected);
        let owner = Registry::open_readonly(&path)
            .unwrap()
            .attach("thread")
            .unwrap();
        assert!(!owner.active_turn);
        assert!(owner.active_runs.is_empty());
        let payload: String = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT payload FROM owner_checkpoints WHERE thread_id='thread'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let checkpoint = OwnerCheckpoint::decode(&payload).unwrap();
        let saved = runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(run)
            .unwrap()
            .unwrap();
        assert!(
            checkpoint.runs[&run.to_string()]
                .snapshot
                .available(Some(&saved))
        );
        assert_eq!(saved.terminal_phase, format!("{expected:?}"));
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
    }
}
