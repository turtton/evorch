use super::*;

#[tokio::test]
async fn consumed_snapshot_requires_repersist_before_second_restore() {
    // Given: restore was accepted, but execution has not reached its next snapshot.
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
    first
        .send_agent_message(parent, child, AgentMessageKind::Send, "pending", None)
        .unwrap();
    let (second, _) = runtime_with(model());
    let second = second.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    second.spawn_reserved(
        parent,
        None,
        Role::Orchestrator,
        "parent",
        RunConfig::default(),
    );
    // When: a memory-absent runtime requests the consumed snapshot before yielding execution.
    let result = second.send_agent_message(parent, child, AgentMessageKind::Send, "replay", None);
    // Then: it refuses the consumed snapshot rather than replaying the old context.
    assert_eq!(
        result,
        Err(RuntimeError::RunRestoreFailed {
            run_id: child.to_string(),
            reason: RunRestoreFailure::UnsupportedConfig("snapshot_consumed".into()),
        })
    );
    terminal(&first, child).await;
    terminal(&second, parent).await;
}

#[tokio::test]
async fn restored_run_repersists_and_restores_again() {
    // Given: a child restores and successfully persists the next terminal snapshot.
    let (_dir, config, storage, database) = storage_fixture();
    let model = model();
    let (runtime, _) = runtime_with(Arc::clone(&model));
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    terminal(&runtime, parent).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    terminal(&runtime, child).await;
    runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "first restore", None)
        .unwrap();
    assert!(
        !database
            .run_context(&child.to_string())
            .unwrap()
            .unwrap()
            .restorable
    );
    assert_eq!(terminal(&runtime, child).await, AgentRunPhase::Done);
    let fresh = database.run_context(&child.to_string()).unwrap().unwrap();
    let messages: Vec<Message> = serde_json::from_str(&fresh.messages_json).unwrap();
    assert!(fresh.restorable);
    // When: another send restores the freshly persisted history.
    runtime
        .send_agent_message(
            parent,
            child,
            AgentMessageKind::Send,
            "second restore",
            None,
        )
        .unwrap();
    // Then: execution completes with every message from the fresh snapshot preserved.
    assert_eq!(terminal(&runtime, child).await, AgentRunPhase::Done);
    let observed = model.observed().await;
    let resumed = observed.last().unwrap();
    assert_eq!(&resumed[..messages.len()], messages);
    assert_eq!(resumed.len(), messages.len() + 1);
}

#[tokio::test]
async fn closed_writer_refuses_restore_before_execution() {
    // Given: a restorable snapshot exists, but its writer is closed.
    let (_dir, config, storage, database) = storage_fixture();
    let model = model();
    let (runtime, _) = runtime_with(Arc::clone(&model));
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    terminal(&runtime, parent).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    terminal(&runtime, child).await;
    let original = database.run_context(&child.to_string()).unwrap();
    storage.close();
    // When: restore cannot durably consume the snapshot.
    let result = runtime.send_agent_message(parent, child, AgentMessageKind::Send, "refused", None);
    // Then: the typed failure precedes execution and leaves the unconsumed snapshot intact.
    assert!(matches!(
        result,
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::SnapshotConsumeFailed(_),
            ..
        })
    ));
    assert_eq!(terminal(&runtime, child).await, AgentRunPhase::Done);
    assert_eq!(model.observed().await.len(), 2);
    assert_eq!(database.run_context(&child.to_string()).unwrap(), original);
}

#[tokio::test]
async fn failed_first_snapshot_stays_missing_context() {
    // Given: run-1 has no snapshot because its first terminal write failed.
    let (_dir, config, storage, _) = storage_fixture();
    let (runtime, _) = runtime_with(model());
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    storage.close();
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    terminal(&runtime, parent).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    terminal(&runtime, child).await;
    // When: its authorized child asks it to resume.
    let result = runtime.send_agent_message(child, parent, AgentMessageKind::Send, "resume", None);
    // Then: failure memory does not turn a missing snapshot into a stale snapshot error.
    assert_eq!(
        result,
        Err(RuntimeError::RunRestoreFailed {
            run_id: parent.to_string(),
            reason: RunRestoreFailure::MissingContext,
        })
    );
}
