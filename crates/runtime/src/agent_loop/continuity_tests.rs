use super::*;
use crate::continuity::Continuity;
use crate::restore::{RestoredState, RunRestoreDescriptor};
use providers::{Message, Role as MessageRole};
use serde_json::json;

async fn stored_fixture() -> (
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
    let (runtime, mut state) = super::delegate_cleanup_tests::started_fixture(cancel_rx).await;
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
    let (dir, storage, runtime, mut state) = stored_fixture().await;
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
    let (_dir, storage, runtime, mut state) = stored_fixture().await;
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
    let (_dir, storage, runtime, mut state) = stored_fixture().await;
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
    let (dir, _storage, runtime, state) = stored_fixture().await;
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
        let (dir, _storage, runtime, mut state) = stored_fixture().await;
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

#[tokio::test]
async fn b3_preparation_cancel_and_snapshot_failure_leave_replay_and_protection_unchanged() {
    let (dir, storage, runtime, mut state) = stored_fixture().await;
    let a = crate::context::responses_replay_tests::scope("A");
    let b = crate::context::responses_replay_tests::scope("B");
    state
        .context
        .responses_replay
        .prepare(Some(a.clone()), state.context.messages.len());
    state
        .context
        .push_assistant(crate::context::responses_replay_tests::opaque("R1", a));
    crate::restore::persist_prepared_context(&state, &state.context).unwrap();
    let run = state.caller_run_id();
    let record = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let original = state.context.clone();
    let connection = rusqlite::Connection::open(dir.path().join("continuity.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_replay BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(ABORT, 'replay rejected'); END;").unwrap();
    assert!(
        crate::compaction::prepare_responses_scope(&mut state, "codex/B", Some(b.clone()))
            .await
            .is_err()
    );
    assert_eq!(state.context, original);
    assert_eq!(
        runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(run)
            .unwrap()
            .unwrap(),
        record
    );
    connection
        .execute_batch("DROP TRIGGER reject_replay;")
        .unwrap();
    let (cancel, receiver) = watch::channel(RunInterrupt::Cancel);
    state.channels.cancel_rx = receiver;
    assert!(
        crate::compaction::prepare_responses_scope(&mut state, "codex/B", Some(b.clone()))
            .await
            .is_err()
    );
    assert_eq!(state.context, original);
    cancel.send_replace(RunInterrupt::None);
    crate::compaction::prepare_responses_scope(&mut state, "codex/B", Some(b))
        .await
        .unwrap();
    assert_eq!(
        state.context.responses_replay.floor,
        original.messages.len()
    );
    assert_eq!(state.context.messages, original.messages);
    let saved = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let restored = RestoredState::from_record(&saved).unwrap();
    assert_eq!(restored.responses_replay, state.context.responses_replay);
    assert_eq!(record.messages_json, saved.messages_json);
    drop(storage);
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn b3_failed_candidate_does_not_commit_usage_todo_or_success_event() {
    let (dir, _storage, runtime, mut state) = stored_fixture().await;
    let run = state.caller_run_id();
    crate::restore::persist_prepared_context(&state, &state.context).unwrap();
    state.last_usage = Some(providers::Usage {
        input_tokens: 900,
        ..Default::default()
    });
    state.todo_context_pending = false;
    state.todo_context_deferred = true;
    let mut events = state.shared.bus.subscribe();
    let original = state.context.clone();
    let connection = rusqlite::Connection::open(dir.path().join("continuity.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_candidate BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(ABORT, 'candidate rejected'); END;").unwrap();
    assert!(
        crate::compaction::compact_now(&mut state, CompactionReason::Manual)
            .await
            .is_err()
    );
    assert_eq!(state.context, original);
    assert_eq!(
        state.last_usage.map(|u| u.input_tokens),
        Some(900),
        "failed candidate must not clear previous usage"
    );
    assert!(!state.todo_context_pending);
    assert!(state.todo_context_deferred);
    for event in events.drain_pending_snapshot() {
        assert!(
            !matches!(
                event.kind,
                event_bus::EventKind::Compaction(event_bus::CompactionEvent::Compacted { .. })
            ),
            "failed candidate must not publish success"
        );
    }
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn b3_descriptor_saved_end_legacy_corruption_and_fork_reset_round_trip() {
    let (_dir, _storage, runtime, mut state) = stored_fixture().await;
    let run = state.caller_run_id();
    let a = crate::context::responses_replay_tests::scope("A");
    state
        .context
        .responses_replay
        .prepare(Some(a.clone()), state.context.messages.len());
    state
        .context
        .push_assistant(crate::context::responses_replay_tests::opaque(
            "R1",
            a.clone(),
        ));
    crate::restore::persist_prepared_context(&state, &state.context).unwrap();
    let record = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let restored = RestoredState::from_record(&record).unwrap();
    assert_eq!(restored.responses_replay, state.context.responses_replay);
    assert_eq!(restored.messages, state.context.messages);
    let count = restored
        .messages
        .iter()
        .filter(|m| m.role != MessageRole::System)
        .count() as u64;
    let child = RestoredState::for_fork_seed(&record, count).unwrap();
    assert_eq!(
        child.responses_replay,
        crate::ResponsesReplayState::reset(child.messages.len())
    );
    assert_eq!(
        state.context.responses_replay, restored.responses_replay,
        "parent is unchanged"
    );
    let mut legacy = record.clone();
    let mut descriptor: serde_json::Value = serde_json::from_str(&legacy.config_json).unwrap();
    descriptor
        .as_object_mut()
        .unwrap()
        .remove("responses_replay");
    legacy.config_json = descriptor.to_string();
    assert!(
        RestoredState::from_record(&legacy).is_err(),
        "opaque with missing state is corrupt"
    );
    legacy.messages_json = serde_json::to_string(&state.context.messages[..3]).unwrap();
    RestoredState::from_record(&legacy).unwrap();
    let mut corrupt = record.clone();
    let mut descriptor: serde_json::Value = serde_json::from_str(&corrupt.config_json).unwrap();
    descriptor["responses_replay"]["floor"] = json!(state.context.messages.len() + 1);
    corrupt.config_json = descriptor.to_string();
    assert!(RestoredState::from_record(&corrupt).is_err());
    // An outstanding batch is safe-end clipping, not a fork/reset.
    state.context.messages.push(Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "outstanding".into(),
            name: "compact".into(),
            input: json!({}),
        }],
    });
    let end = state.context.messages.len() - 1;
    for floor in [end - 1, end, end + 1] {
        let mut prepared = state.context.clone();
        prepared.responses_replay.floor = floor;
        crate::restore::persist_prepared_context(&state, &prepared).unwrap();
        let saved = runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(run)
            .unwrap()
            .unwrap();
        let restored = RestoredState::from_record(&saved).unwrap();
        assert_eq!(restored.messages.len(), end);
        assert_eq!(restored.responses_replay.floor, floor.min(end));
        for i in 0..end {
            assert_eq!(i >= restored.responses_replay.floor, i >= floor);
        }
        assert_ne!(
            restored.responses_replay.binding,
            crate::ResponsesReplayBinding::Reset
        );
    }
    assert_eq!(state.context.responses_replay, restored.responses_replay);
    runtime.cancel(run).unwrap();
}

#[derive(Default)]
struct ReplayCompactor(std::sync::Mutex<Vec<Vec<Message>>>);
#[async_trait::async_trait]
impl AgentModel for ReplayCompactor {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "codex/A".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        unreachable!()
    }
    async fn compact_context(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<Option<providers::CompactionResult>, crate::RuntimeError> {
        self.0.lock().unwrap().push(messages.to_vec());
        Ok(Some(providers::CompactionResult {
            encrypted_content: "official".into(),
            usage: Default::default(),
        }))
    }
}

#[tokio::test]
async fn b3_candidate_window_filters_old_reasoning_before_estimate_and_official_call() {
    let (_dir, _storage, runtime, mut state) = stored_fixture().await;
    let run = state.caller_run_id();
    let a = crate::context::responses_replay_tests::scope("A");
    let b = crate::context::responses_replay_tests::scope("B");
    let capture = Arc::new(ReplayCompactor::default());
    state.shared.model = capture.clone();
    state
        .shared
        .compaction
        .model_overrides
        .insert("codex/B".into(), 1000);
    state
        .context
        .responses_replay
        .prepare(Some(a.clone()), state.context.messages.len());
    let mut huge = crate::context::responses_replay_tests::opaque("R1", a);
    if let ContentBlock::ResponsesReasoning { item, .. } = &mut huge.content[0] {
        item["encrypted_content"] = json!("x".repeat(100_000));
    }
    state.context.push_assistant(huge);
    let raw = state.context.messages.clone();
    crate::compaction::prepare_responses_scope(&mut state, "codex/B", Some(b))
        .await
        .unwrap();
    assert!(
        capture.0.lock().unwrap().is_empty(),
        "candidate window must not count excluded old opaque"
    );
    assert_eq!(state.context.messages, raw);
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: "large plaintext ".repeat(1000),
        }],
    });
    state.context.push_user("latest");
    let scope = state.context.responses_replay.scope().cloned();
    crate::compaction::prepare_responses_scope(&mut state, "codex/B", scope)
        .await
        .unwrap();
    let requests = capture.0.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        !serde_json::to_string(&requests[0])
            .unwrap()
            .contains("responses_reasoning")
    );
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn b3_protected_summary_survives_manual_automatic_and_window_compaction() {
    let (_dir, _storage, runtime, mut state) = stored_fixture().await;
    let run = state.caller_run_id();
    let a = crate::context::responses_replay_tests::scope("A");
    state
        .context
        .responses_replay
        .prepare(Some(a.clone()), state.context.messages.len());
    state
        .context
        .push_assistant(crate::context::responses_replay_tests::opaque(
            "R1",
            a.clone(),
        ));
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "hidden".into(),
            name: "edit".into(),
            input: json!({"hidden_raw":"never-resurrect"}),
        }],
    });
    state
        .context
        .push_tool_result("hidden", tools::ToolResult::success("done"));
    state.context.push_user("new request");
    crate::compaction::prepare_model_switch(&mut state, "codex/A", true)
        .await
        .unwrap();
    let protected = state.context.latest_checkpoint().unwrap().range;
    assert!(state.context.latest_checkpoint().unwrap().protects_history);
    state
        .context
        .responses_replay
        .prepare(Some(a.clone()), state.context.messages.len());
    let capture = Arc::new(ReplayCompactor::default());
    state.shared.model = capture.clone();
    for reason in [
        CompactionReason::Manual,
        CompactionReason::Automatic,
        CompactionReason::ModelSwitch,
    ] {
        state
            .context
            .push_assistant(crate::context::responses_replay_tests::opaque(
                "R2",
                a.clone(),
            ));
        state.context.push_assistant(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "new evidence ".repeat(200),
            }],
        });
        state.context.push_user("next request");
        state.compaction.turn_counter += 2;
        state.compaction.compacted_this_boundary = false;
        state.compaction.auto_suspended = false;
        state.shared.compaction.cooldown_turns = 0;
        if reason == CompactionReason::Automatic {
            state.shared.compaction.threshold = 0.0;
        }
        crate::compaction::compact_now(&mut state, reason)
            .await
            .unwrap();
        let checkpoint = state.context.latest_checkpoint().unwrap();
        assert!(checkpoint.protects_history);
        assert!(checkpoint.range.0 <= protected.0 && checkpoint.range.1 >= protected.1);
    }
    for request in capture.0.lock().unwrap().iter() {
        let json = serde_json::to_string(request).unwrap();
        assert!(!json.contains("never-resurrect"));
        assert!(!json.contains("cipher-R1"));
    }
    runtime.cancel(run).unwrap();
}

struct BlockedReplayCompactor {
    started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}
#[async_trait::async_trait]
impl AgentModel for BlockedReplayCompactor {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "codex/A".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        unreachable!()
    }
    async fn compact_context(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<Option<providers::CompactionResult>, crate::RuntimeError> {
        self.started
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(())
            .unwrap();
        std::future::pending().await
    }
}

#[tokio::test]
async fn b3_inflight_candidate_cancel_rolls_back_without_waiting_for_provider() {
    let (_dir, _storage, runtime, mut state) = stored_fixture().await;
    let run = state.caller_run_id();
    crate::restore::persist_prepared_context(&state, &state.context).unwrap();
    let record = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let original = state.context.clone();
    let original_compaction = state.compaction.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    state.shared.model = Arc::new(BlockedReplayCompactor {
        started: std::sync::Mutex::new(Some(sender)),
    });
    let (cancel, cancel_rx) = watch::channel(RunInterrupt::None);
    state.channels.cancel_rx = cancel_rx;
    let (outcome, ()) = tokio::join!(
        crate::compaction::compact_now(&mut state, CompactionReason::Manual),
        async {
            receiver.await.unwrap();
            cancel.send_replace(RunInterrupt::Cancel);
        }
    );
    assert!(outcome.is_err());
    assert_eq!(state.context, original);
    assert_eq!(state.compaction, original_compaction);
    assert!(
        !state
            .channels
            .compaction_busy
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    assert_eq!(
        runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(run)
            .unwrap()
            .unwrap(),
        record
    );
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn b3_uncommitted_scope_window_call_is_safe_when_snapshot_fails_and_retries() {
    let (dir, _storage, runtime, mut state) = stored_fixture().await;
    let run = state.caller_run_id();
    let a = crate::context::responses_replay_tests::scope("A");
    let b = crate::context::responses_replay_tests::scope("B");
    state
        .context
        .responses_replay
        .prepare(Some(a.clone()), state.context.messages.len());
    state
        .context
        .push_assistant(crate::context::responses_replay_tests::opaque("R1", a));
    state.context.push_user("latest request before switch");
    crate::restore::persist_prepared_context(&state, &state.context).unwrap();
    let original = state.context.clone();
    let record = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let capture = Arc::new(ReplayCompactor::default());
    state.shared.model = capture.clone();
    state
        .shared
        .compaction
        .model_overrides
        .insert("codex/B".into(), 10);
    let connection = rusqlite::Connection::open(dir.path().join("continuity.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_switch BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(ABORT, 'switch rejected'); END;").unwrap();
    assert!(
        crate::compaction::prepare_responses_scope(&mut state, "codex/B", Some(b.clone()))
            .await
            .is_err()
    );
    assert_eq!(state.context, original);
    assert_eq!(
        runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(run)
            .unwrap()
            .unwrap(),
        record
    );
    assert_eq!(
        capture.0.lock().unwrap().len(),
        1,
        "candidate provider call happened before the failed save"
    );
    connection
        .execute_batch("DROP TRIGGER reject_switch;")
        .unwrap();
    crate::compaction::prepare_responses_scope(&mut state, "codex/B", Some(b))
        .await
        .unwrap();
    assert_eq!(
        state.context.responses_replay.floor,
        original.messages.len()
    );
    assert_eq!(
        capture.0.lock().unwrap().len(),
        2,
        "retry repeats safe preparation rather than bypassing it"
    );
    for request in capture.0.lock().unwrap().iter() {
        let json = serde_json::to_string(request).unwrap();
        assert!(!json.contains("responses_reasoning"));
        assert!(!json.contains("cipher-R1"));
    }
    assert_eq!(state.context.messages, original.messages);
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn b3_plaintext_secret_rejection_preserves_prior_snapshot_and_allows_safe_completion() {
    let (_dir, _storage, runtime, mut state) = stored_fixture().await;
    let run = state.caller_run_id();
    state.transition(AgentRunPhase::Running, None).unwrap();
    let scope = crate::context::responses_replay_tests::scope("A");
    state
        .context
        .responses_replay
        .prepare(Some(scope.clone()), state.context.messages.len());
    crate::restore::persist_checkpoint(&state).unwrap();
    let store = runtime.shared.run_store.get().unwrap();
    let prior = store.restore_record(run).unwrap().unwrap();
    let secret = ["sk-", &"A".repeat(40)].concat();
    let mut message = crate::context::responses_replay_tests::opaque("R-secret", scope);
    if let ContentBlock::ResponsesReasoning { item, .. } = &mut message.content[0] {
        item["unknown_plaintext"] = json!({"nested": secret});
    }
    state.context.push_assistant(message);
    let error = crate::restore::persist_checkpoint(&state).unwrap_err();
    assert!(
        matches!(
            error,
            crate::restore::SnapshotError::Storage(storage::StorageError::Serialization(_))
        ),
        "the real storage guard, not replay-state validation, must reject this snapshot"
    );
    assert!(!error.to_string().contains(&secret));
    let original = state.context.clone();
    let mut events = state.shared.bus.subscribe();
    assert_eq!(
        state.save_checkpoint().unwrap(),
        crate::ownership::SnapshotReference::none(
            crate::ownership::SnapshotNoneReason::WriteFailed
        )
    );
    assert_eq!(state.phase(), AgentRunPhase::Running);
    assert_eq!(
        state.context, original,
        "a rejected native item must not be partially rewritten"
    );
    assert_eq!(store.restore_record(run).unwrap().unwrap(), prior);
    let boundary_error = crate::compaction::prepare_responses_scope(
        &mut state,
        "codex/B",
        Some(crate::context::responses_replay_tests::scope("B")),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        boundary_error,
        crate::compaction::CompactionError::PrepareFailed(_)
    ));
    assert!(!boundary_error.to_string().contains(&secret));
    assert_eq!(
        state.context, original,
        "a required scope boundary must fail closed without committing its floor"
    );
    assert_eq!(store.restore_record(run).unwrap().unwrap(), prior);
    state
        .context
        .push_user("continue without a new durable snapshot");
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: "finished safely".into(),
        }],
    });
    assert_eq!(
        state.save_checkpoint().unwrap(),
        crate::ownership::SnapshotReference::none(
            crate::ownership::SnapshotNoneReason::WriteFailed
        )
    );
    assert!(state.finish_success());
    state.finalize().await;
    assert_eq!(state.phase(), AgentRunPhase::Done);
    assert_eq!(
        store.restore_record(run).unwrap().unwrap(),
        prior,
        "terminal rejection must also leave the old snapshot intact"
    );
    let mut warnings = 0;
    for event in events.drain_pending_snapshot() {
        let serialized = serde_json::to_string(&event).unwrap();
        assert!(!serialized.contains(&secret));
        if let event_bus::EventKind::Diagnostic(diagnostic) = event.kind
            && diagnostic.code == event_bus::event::diagnostic_codes::CONTEXT_SNAPSHOT_FAILED
        {
            warnings += 1;
        }
    }
    assert_eq!(
        warnings, 3,
        "two checkpoints and terminal save each report the rejection"
    );
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
}

#[derive(Default)]
struct NativeCheckpointRecoveryModel {
    summaries: std::sync::Mutex<Vec<Vec<Message>>>,
}

#[async_trait::async_trait]
impl AgentModel for NativeCheckpointRecoveryModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "recovery-model".into()
    }

    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        self.summaries.lock().unwrap().push(messages.to_vec());
        let text = messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let facts = ["D3-SAFE-FACT-7", "AFTER-D3-FACT-8"]
            .into_iter()
            .filter(|fact| text.contains(fact))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(providers::ChatResponse {
            message: Message {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("recovered {facts}"),
                }],
            },
            usage: Default::default(),
            finish_reason: FinishReason::Stop,
        })
    }
}

#[tokio::test]
async fn b3_model_switch_recovers_readable_d3_summary_under_native_checkpoints() {
    assert_readable_d3_recovery(true, false).await;
}

#[tokio::test]
async fn b3_text_compaction_fallback_recovers_readable_d3_summary_under_native_checkpoints() {
    assert_readable_d3_recovery(false, false).await;
}

#[tokio::test]
async fn b3_structural_compaction_recovers_readable_d3_summary_under_native_checkpoints() {
    assert_readable_d3_recovery(false, true).await;
}

async fn assert_readable_d3_recovery(model_switch: bool, structural: bool) {
    let (_dir, _storage, runtime, mut state) = stored_fixture().await;
    let run = state.caller_run_id();
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "old-edit".into(),
            name: "edit".into(),
            input: json!({"hidden":"DO-NOT-REVIVE-OLD-BATCH"}),
        }],
    });
    state
        .context
        .push_tool_result("old-edit", tools::ToolResult::success("old tool output"));
    let d3_end = state.context.messages.len();
    // The fact exists only in the safe D3 summary, never in the hidden raw batch.
    state.context.apply_checkpoint(crate::CompactionCheckpoint {
        id: "d3-readable".into(),
        summary: Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "D3-SAFE-FACT-7".into(),
            }],
        },
        range: (1, d3_end),
        protects_history: true,
    });
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: "AFTER-D3-FACT-8".into(),
        }],
    });
    state.context.push_user("continue after D3");
    state.shared.model = Arc::new(ReplayCompactor::default());
    state.shared.compaction.cooldown_turns = 0;
    state.shared.compaction.max_summary_bytes = 4096;
    for _ in 0..2 {
        crate::compaction::compact_now(&mut state, CompactionReason::Manual)
            .await
            .unwrap();
        assert!(
            state
                .context
                .latest_checkpoint()
                .unwrap()
                .summary
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Compaction { .. }))
        );
        state.compaction.turn_counter += 2;
        state.compaction.compacted_this_boundary = false;
    }
    // Exercise the exact persisted layout: old readable checkpoints must survive
    // native replacements so a later process can recover their safe evidence.
    let record = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let restored = RestoredState::from_record(&record).unwrap();
    state.context = AgentContext::from_restored(
        run,
        state.run_role(),
        restored.messages,
        restored.checkpoints,
        restored.responses_replay,
    );
    let capture = Arc::new(NativeCheckpointRecoveryModel::default());
    state.shared.model = capture.clone();
    state.shared.compaction.summarizer = if structural {
        crate::compaction::policy::SummarizerKindSel::Structural
    } else {
        crate::compaction::policy::SummarizerKindSel::Model
    };
    if model_switch {
        crate::compaction::prepare_model_switch(&mut state, "other/model", true)
            .await
            .unwrap();
    } else {
        crate::compaction::compact_now(&mut state, CompactionReason::Manual)
            .await
            .unwrap();
    }
    let requests = capture.summaries.lock().unwrap();
    assert_eq!(requests.len(), usize::from(!structural));
    if !structural {
        let input = serde_json::to_string(&requests[0]).unwrap();
        assert!(
            input.contains("D3-SAFE-FACT-7"),
            "readable D3 evidence must survive native replacements: {input}"
        );
        assert!(
            input.contains("AFTER-D3-FACT-8"),
            "safe raw tail must also be recovered"
        );
        assert!(!input.contains("DO-NOT-REVIVE-OLD-BATCH"));
        assert!(
            !requests[0]
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(
                    block,
                    ContentBlock::Compaction { .. } | ContentBlock::ResponsesReasoning { .. }
                ))
        );
    }
    let checkpoint = state.context.latest_checkpoint().unwrap();
    assert!(checkpoint.protects_history);
    assert!(checkpoint.range.0 <= 1 && checkpoint.range.1 >= d3_end);
    let summary = serde_json::to_string(&checkpoint.summary).unwrap();
    assert!(summary.contains("D3-SAFE-FACT-7"));
    assert!(summary.contains("AFTER-D3-FACT-8"));
    assert!(!summary.contains("DO-NOT-REVIVE-OLD-BATCH"));
    runtime.cancel(run).unwrap();
}

#[derive(Default)]
struct FailedProtectedSummaryModel(std::sync::Mutex<Vec<Vec<Message>>>);

#[async_trait::async_trait]
impl AgentModel for FailedProtectedSummaryModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "old/A".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        self.0.lock().unwrap().push(messages.to_vec());
        Err(crate::RuntimeError::Model {
            reason: "summary model unavailable".into(),
        })
    }
}

async fn protected_long_tail_fixture() -> (
    tempfile::TempDir,
    storage::Storage,
    crate::AgentRuntime,
    LoopState,
    Arc<FailedProtectedSummaryModel>,
    String,
) {
    let (dir, storage, runtime, mut state) = stored_fixture().await;
    state.context.push_assistant(Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "old-hidden".into(),
            name: "edit".into(),
            input: json!({"evidence":"OLD-HIDDEN-BATCH-997"}),
        }],
    });
    state
        .context
        .push_tool_result("old-hidden", tools::ToolResult::success("hidden result"));
    let protected =
        "保護済み固有事実: D3-PRESERVED-FACT-991\noriginal decision must survive".to_string();
    state.context.apply_checkpoint(crate::CompactionCheckpoint {
        id: "trusted-text-source".into(),
        range: (1, state.context.messages.len()),
        protects_history: true,
        summary: Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: protected.clone(),
            }],
        },
    });
    // At least three newer text messages push the protected User summary out of
    // Structural's recent-two heuristic. Only checkpoint provenance can retain it.
    for index in 0..4 {
        state.context.push_assistant(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: format!("tail {index}: {}", "fresh unrelated evidence ".repeat(100)),
            }],
        });
    }
    state.context.apply_checkpoint(crate::CompactionCheckpoint {
        id: "native-after-text".into(),
        range: (1, state.context.messages.len()),
        protects_history: true,
        summary: Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Compaction {
                encrypted_content: "opaque-native".into(),
            }],
        },
    });
    state.context.push_user("latest input remains verbatim");
    state.context.responses_replay.prepare(
        Some(crate::context::responses_replay_tests::scope("A")),
        state.context.messages.len(),
    );
    let model = Arc::new(FailedProtectedSummaryModel::default());
    state.shared.model = model.clone();
    state.shared.compaction.summarizer = crate::compaction::policy::SummarizerKindSel::Model;
    state.shared.compaction.max_summary_bytes = 1024;
    crate::restore::persist_prepared_context(&state, &state.context).unwrap();
    (dir, storage, runtime, state, model, protected)
}

#[tokio::test]
async fn b3_structural_fallback_preserves_trusted_protected_body_beyond_recent_tail() {
    for model_switch in [true, false] {
        for extra_budget in [0, 1, 1024] {
            assert_protected_structural_recovery(model_switch, extra_budget).await;
        }
    }
}

async fn assert_protected_structural_recovery(model_switch: bool, extra_budget: u64) {
    let (_dir, _storage, runtime, mut state, model, protected) =
        protected_long_tail_fixture().await;
    let run = state.caller_run_id();
    let original = state.context.messages.clone();
    state.shared.compaction.max_summary_bytes = protected.len() as u64 + extra_budget;
    if model_switch {
        crate::compaction::prepare_model_switch(&mut state, "target/B", true)
            .await
            .unwrap();
    } else {
        crate::compaction::compact_now(&mut state, CompactionReason::Manual)
            .await
            .unwrap();
    }
    assert_eq!(
        model.0.lock().unwrap().len(),
        1,
        "the model summary was attempted before fallback"
    );
    let summary = state
        .context
        .latest_checkpoint()
        .unwrap()
        .summary
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .unwrap();
    assert!(
        summary.contains(&protected),
        "trusted protected body must survive byte-for-byte, including UTF-8 and newlines: {summary}"
    );
    assert!(!summary.contains("OLD-HIDDEN-BATCH-997"));
    assert_eq!(state.context.messages, original);
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn b3_protected_summary_limit_failure_rolls_back_scope_checkpoint_and_usage() {
    for (model_switch, byte_limit) in [
        (true, Some(32)),
        (false, Some(32)),
        (true, None),
        (false, None),
    ] {
        let (_dir, _storage, runtime, mut state, model, _) = protected_long_tail_fixture().await;
        let run = state.caller_run_id();
        if let Some(limit) = byte_limit {
            state.shared.compaction.max_summary_bytes = limit;
        } else {
            let selected = if model_switch { "target/B" } else { "old/A" };
            state
                .shared
                .compaction
                .model_overrides
                .insert(selected.into(), 16);
        }
        state.last_usage = Some(providers::Usage {
            input_tokens: 987,
            output_tokens: 12,
            ..Default::default()
        });
        state.compaction.last_usage_estimated_tokens = Some(321);
        let original = state.context.clone();
        let compaction = state.compaction.clone();
        let record = runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(run)
            .unwrap()
            .unwrap();
        let mut events = state.shared.bus.subscribe();
        let error = if model_switch {
            crate::compaction::prepare_model_switch(&mut state, "target/B", true).await
        } else {
            crate::compaction::compact_now(&mut state, CompactionReason::Manual)
                .await
                .map(|_| ())
        }
        .expect_err("protected content that cannot fit must stop preparation");
        assert!(matches!(
            error,
            crate::compaction::CompactionError::PrepareFailed(_)
        ));
        let expected = if byte_limit.is_some() {
            "protected summary body"
        } else {
            "target context window"
        };
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!model.0.lock().unwrap().is_empty());
        assert_eq!(state.context, original);
        assert_eq!(state.compaction, compaction);
        assert_eq!(state.last_usage.unwrap().input_tokens, 987);
        assert_eq!(
            runtime
                .shared
                .run_store
                .get()
                .unwrap()
                .restore_record(run)
                .unwrap()
                .unwrap(),
            record
        );
        assert!(
            !events.drain_pending_snapshot().iter().any(|event| matches!(
                event.kind,
                event_bus::EventKind::Compaction(event_bus::CompactionEvent::Compacted { .. })
            ))
        );
        runtime.cancel(run).unwrap();
    }
}
