use super::*;
use event_bus::AgentMessageKind;
use runtime::{RunRestoreFailure, RuntimeError};

fn model() -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new(
        (0..16).map(|_| Ok(text_response("answer", FinishReason::Stop))),
    ))
}

#[tokio::test]
async fn fresh_run_after_restart_preserves_snapshot_and_ledger() {
    // Given: a terminal run and its ledger survive the first runtime.
    let (_dir, config, storage, database) = storage_fixture();
    let (first, _) = runtime_with(model());
    let first = first.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let old = first.delegate_background(Role::Worker, "old".into(), RunConfig::default());
    terminal(&first, old).await;
    storage
        .handle()
        .append_run_ledger(&old.to_string(), "decision")
        .unwrap();
    let snapshot = database.run_context(&old.to_string()).unwrap();
    let ledger = database.run_ledger(&old.to_string()).unwrap();
    drop(first);
    let (second, _) = runtime_with(model());
    let second = second.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    // When: a fresh run is spawned without restoring any old run.
    let fresh = second.delegate_background(Role::Worker, "new".into(), RunConfig::default());
    terminal(&second, fresh).await;
    // Then: both durable namespaces remain disjoint.
    assert!(fresh.get() > old.get());
    assert_eq!(database.run_context(&old.to_string()).unwrap(), snapshot);
    assert_eq!(database.run_ledger(&old.to_string()).unwrap(), ledger);
}

#[tokio::test]
async fn failed_terminal_update_rejects_stale_restore() {
    for close_writer in [false, true] {
        // Given: a child with a snapshot that predates writer closure.
        let (_dir, config, storage, database) = storage_fixture();
        let handle = storage.handle();
        let (runtime, _) = runtime_with(model());
        let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let parent =
            runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
        terminal(&runtime, parent).await;
        let child = runtime
            .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
            .unwrap();
        terminal(&runtime, child).await;
        if close_writer {
            storage.close();
        } else {
            let conn = rusqlite::Connection::open(&config.db_path).unwrap();
            conn.execute_batch("CREATE TRIGGER reject_snapshot_update BEFORE UPDATE ON run_contexts WHEN NEW.restorable = 1 BEGIN SELECT RAISE(ABORT, 'snapshot failure'); END;").unwrap();
        }
        runtime
            .send_agent_message(parent, child, AgentMessageKind::Send, "lost turn", None)
            .unwrap();
        terminal(&runtime, child).await;
        // When: the caller requests another turn after failed persistence.
        let result =
            runtime.send_agent_message(parent, child, AgentMessageKind::Send, "next", None);
        // Then: stale history is never accepted as complete context.
        assert_eq!(
            result,
            Err(RuntimeError::RunRestoreFailed {
                run_id: child.to_string(),
                reason: RunRestoreFailure::UnsupportedConfig("persist_failed".into()),
            })
        );
        if !close_writer {
            assert!(
                !database
                    .run_context(&child.to_string())
                    .unwrap()
                    .unwrap()
                    .restorable
            );
            drop(runtime);
            let (restarted, _) = runtime_with(model());
            let restarted = restarted.with_run_store(RunStore::open(&config, handle).unwrap());
            restarted.spawn_reserved(
                parent,
                None,
                Role::Orchestrator,
                "parent",
                RunConfig::default(),
            );
            terminal(&restarted, parent).await;
            assert_eq!(
                restarted.send_agent_message(parent, child, AgentMessageKind::Send, "next", None),
                Err(RuntimeError::RunRestoreFailed {
                    run_id: child.to_string(),
                    reason: RunRestoreFailure::UnsupportedConfig("persist_failed".into())
                })
            );
        }
    }
}

#[tokio::test]
async fn absent_recipient_authorizes_before_reading_corrupt_payload() {
    for payload in [
        rusqlite::types::Value::Text("invalid".into()),
        rusqlite::types::Value::Blob(vec![0x80]),
    ] {
        // Given: persisted child metadata is valid, but the payload cannot be read as text.
        let (_dir, config, storage, _) = storage_fixture();
        let (first, _) = runtime_with(model());
        let first = first.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let parent =
            first.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
        terminal(&first, parent).await;
        let child = first
            .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
            .unwrap();
        terminal(&first, child).await;
        drop(first);
        let conn = rusqlite::Connection::open(&config.db_path).unwrap();
        conn.execute(
            "UPDATE run_contexts SET messages_json = ?2 WHERE run_id = ?1",
            rusqlite::params![child.to_string(), payload],
        )
        .unwrap();
        let (runtime, _) = runtime_with(model());
        let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        runtime.spawn_reserved(
            parent,
            None,
            Role::Orchestrator,
            "parent",
            RunConfig::default(),
        );
        terminal(&runtime, parent).await;
        let unrelated = RunId::new(99);
        runtime.spawn_reserved(
            unrelated,
            None,
            Role::Worker,
            "unrelated",
            RunConfig::default(),
        );
        terminal(&runtime, unrelated).await;
        // When: both identities request the same corrupt recipient.
        let denied =
            runtime.send_agent_message(unrelated, child, AgentMessageKind::Send, "turn", None);
        let authorized =
            runtime.send_agent_message(parent, child, AgentMessageKind::Send, "turn", None);
        // Then: only the authorized sender can observe payload corruption.
        assert!(matches!(denied, Err(RuntimeError::MessageDenied { .. })));
        assert!(matches!(
            authorized,
            Err(RuntimeError::RunRestoreFailed {
                reason: RunRestoreFailure::CorruptContext(_),
                ..
            })
        ));
    }
}
