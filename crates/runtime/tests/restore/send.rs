use super::*;
use event_bus::{AgentMessageEvent, AgentMessageKind, DeliveryDisposition};
use runtime::{RunRestoreFailure, RuntimeError};

async fn pair(runtime: &AgentRuntime) -> (RunId, RunId) {
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    terminal(runtime, parent).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    terminal(runtime, child).await;
    (parent, child)
}

fn model() -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new(
        (0..16).map(|_| Ok(text_response("answer", FinishReason::Stop))),
    ))
}

#[tokio::test]
async fn send_to_error_run_restores_and_completes() {
    // Given: a child terminated by a provider error.
    let (_dir, config, storage, _) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response("parent", FinishReason::Stop)),
        Err(RuntimeError::Model {
            reason: "provider failure".into(),
        }),
        Ok(text_response("recovered", FinishReason::Stop)),
    ]));
    let (runtime, _) = runtime_with(model);
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let (parent, child) = pair(&runtime).await;
    assert_eq!(terminal(&runtime, child).await, AgentRunPhase::Error);
    // When: an authorized sender starts a recovery turn.
    runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "recover", None)
        .unwrap();
    // Then: the same run reaches Done instead of retaining Error.
    assert_eq!(terminal(&runtime, child).await, AgentRunPhase::Done);
}

#[tokio::test]
async fn send_to_memory_absent_run_restores_after_runtime_restart() {
    // Given: a terminated run stored by a runtime that is subsequently dropped.
    let (_dir, config, storage, database) = storage_fixture();
    let (first, _) = runtime_with(model());
    let first = first.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let (parent, child) = pair(&first).await;
    let record = database.run_context(&child.to_string()).unwrap().unwrap();
    let original: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
    drop(first);
    let model = model();
    let (runtime, _) = runtime_with(Arc::clone(&model));
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    runtime.spawn_reserved(
        parent,
        None,
        Role::Orchestrator,
        "parent",
        RunConfig::default(),
    );
    terminal(&runtime, parent).await;
    // When: the new runtime receives a send for its absent child.
    runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "restart turn", None)
        .unwrap();
    // Then: persisted history is present in the model request.
    assert_eq!(terminal(&runtime, child).await, AgentRunPhase::Done);
    let requests = model.observed().await;
    assert_eq!(&requests.last().unwrap()[..original.len()], original);
}

#[tokio::test]
async fn restore_emits_agent_run_restored_and_delivered_events() {
    // Given: a durable terminal child and a bus subscriber.
    let (_dir, config, storage, _) = storage_fixture();
    let (runtime, bus) = runtime_with(model());
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let (parent, child) = pair(&runtime).await;
    let mut events = bus.subscribe();
    // When: send restores the child.
    let id = runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "turn", None)
        .unwrap();
    // Then: both lifecycle restoration and restored delivery carry the same IDs.
    let mut restored = false;
    timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Lifecycle(LifecycleEvent::AgentRunRestored {
                    run_id,
                    restored_by,
                    message_id,
                }) => {
                    assert_eq!(
                        (run_id, restored_by, message_id),
                        (child.to_string(), parent.to_string(), id.clone())
                    );
                    restored = true;
                }
                EventKind::AgentMessage(AgentMessageEvent::Delivered {
                    message,
                    disposition: DeliveryDisposition::Restored,
                }) => {
                    assert!(restored);
                    assert_eq!(message.message_id, id);
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    terminal(&runtime, child).await;
}

#[tokio::test]
async fn restore_without_snapshot_returns_missing_context() {
    // Given: registry-known terminal runs created before storage attachment.
    let (_dir, config, storage, _) = storage_fixture();
    let (runtime, _) = runtime_with(model());
    let (parent, child) = pair(&runtime).await;
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    // When: send requires a missing snapshot.
    let result = runtime.send_agent_message(parent, child, AgentMessageKind::Send, "turn", None);
    // Then: no fresh session is substituted.
    assert_eq!(
        result,
        Err(RuntimeError::RunRestoreFailed {
            run_id: child.to_string(),
            reason: RunRestoreFailure::MissingContext
        })
    );
}

#[tokio::test]
async fn restore_of_unsupported_config_returns_typed_error() {
    // Given: a stored descriptor explicitly marked unsupported.
    let (_dir, config, storage, database) = storage_fixture();
    let (runtime, _) = runtime_with(model());
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let (parent, child) = pair(&runtime).await;
    let mut record = database.run_context(&child.to_string()).unwrap().unwrap();
    record.restorable = false;
    storage.handle().upsert_run_context(&record).unwrap();
    // When: send attempts restoration.
    let result = runtime.send_agent_message(parent, child, AgentMessageKind::Send, "turn", None);
    // Then: configuration rejection remains typed.
    assert!(matches!(
        result,
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::UnsupportedConfig(_),
            ..
        })
    ));
}

#[tokio::test]
async fn authz_violation_on_restore_path_returns_message_denied() {
    // Given: corrupt content belonging to an unrelated terminal recipient.
    let (_dir, config, storage, database) = storage_fixture();
    let (runtime, _) = runtime_with(model());
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let (_, child) = pair(&runtime).await;
    let unrelated =
        runtime.delegate_background(Role::Worker, "unrelated".into(), RunConfig::default());
    terminal(&runtime, unrelated).await;
    let mut record = database.run_context(&child.to_string()).unwrap().unwrap();
    record.messages_json = "invalid".into();
    storage.handle().upsert_run_context(&record).unwrap();
    // When: an unrelated sender attempts restoration.
    let result = runtime.send_agent_message(unrelated, child, AgentMessageKind::Send, "turn", None);
    // Then: authorization precedes content decoding.
    assert!(matches!(result, Err(RuntimeError::MessageDenied { .. })));
}

#[tokio::test]
async fn restore_of_corrupt_context_returns_typed_error() {
    // Given: an authorized recipient with invalid stored JSON.
    let (_dir, config, storage, database) = storage_fixture();
    let (runtime, _) = runtime_with(model());
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let (parent, child) = pair(&runtime).await;
    let mut record = database.run_context(&child.to_string()).unwrap().unwrap();
    record.messages_json = "invalid".into();
    storage.handle().upsert_run_context(&record).unwrap();
    // When: an authorized send attempts restoration.
    let result = runtime.send_agent_message(parent, child, AgentMessageKind::Send, "turn", None);
    // Then: corrupt history cannot silently become an empty session.
    assert!(matches!(
        result,
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::CorruptContext(_),
            ..
        })
    ));
}

#[tokio::test]
async fn restored_run_id_never_collides_with_future_runs() {
    // Given: storage contains an ID beyond a restarted runtime's counter.
    let (_dir, config, storage, _) = storage_fixture();
    let (first, _) = runtime_with(model());
    let first = first.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        first.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    terminal(&first, parent).await;
    let child = RunId::new(100);
    first.spawn_reserved(
        child,
        Some(parent),
        Role::Worker,
        "child",
        RunConfig::default(),
    );
    terminal(&first, child).await;
    drop(first);
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
    // When: the stored high ID is restored.
    runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "turn", None)
        .unwrap();
    // Then: new allocation is above the restored ID.
    let next = runtime.delegate_background(Role::Worker, "next".into(), RunConfig::default());
    assert!(next.get() > child.get());
    assert_eq!(terminal(&runtime, child).await, AgentRunPhase::Done);
    terminal(&runtime, next).await;
}
