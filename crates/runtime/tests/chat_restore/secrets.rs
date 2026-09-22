use super::*;

#[tokio::test]
async fn secret_blocks_are_redacted_and_terminal_context_remains_restorable() {
    for block in [
        ContentBlock::Text {
            text: "sk-abcdefghijklmnopqrstuvwxyz0123456789".into(),
        },
        ContentBlock::Reasoning {
            text: "sk-abcdefghijklmnopqrstuvwxyz0123456789".into(),
        },
        ContentBlock::ToolResult {
            tool_call_id: "tool-1".into(),
            content: vec![providers::ToolResultContent::Text {
                text: "sk-abcdefghijklmnopqrstuvwxyz0123456789".into(),
            }],
            is_error: false,
        },
    ] {
        // Given: a clean terminal snapshot and a provider response containing a secret.
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("secret.sqlite3"),
            ..StorageConfig::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let database = storage::Database::open(&config).unwrap();
        let mut secret_response = text_response("safe", FinishReason::Stop);
        secret_response.message.content.push(block);
        let model = Arc::new(ScriptedModel::new([
            Ok(text_response("safe", FinishReason::Stop)),
            Ok(secret_response),
            Ok(text_response("restored", FinishReason::Stop)),
        ]));
        let bus = Arc::new(EventBus::new(128));
        let mut events = bus.subscribe();
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let run = runtime.delegate_background(Role::Worker, "goal".into(), RunConfig::default());
        runtime.wait(run).await.unwrap();
        runtime
            .continue_goal(run, "continue".into(), RunConfig::default())
            .unwrap();
        loop {
            let event = events.recv().await.unwrap();
            if matches!(
                event.kind,
                event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Waiting,
                    ..
                })
            ) {
                break;
            }
        }
        // When: cancellation persists the accumulated terminal context.
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
        // Then: the complete redacted history remains restorable.
        let record = database.run_context(&run.to_string()).unwrap().unwrap();
        assert!(record.restorable);
        assert!(
            !record
                .messages_json
                .contains("sk-abcdefghijklmnopqrstuvwxyz0123456789")
        );
        let descriptor: runtime::restore::RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).unwrap();
        assert!(descriptor.non_restorable_reason.is_none());
        runtime
            .continue_goal(run, "retry".into(), RunConfig::default())
            .unwrap();
        loop {
            let event = events.recv().await.unwrap();
            if matches!(
                event.kind,
                event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Waiting,
                    ..
                })
            ) {
                break;
            }
        }
        let observed = serde_json::to_string(&model.observed().await[2]).unwrap();
        assert!(!observed.contains("sk-abcdefghijklmnopqrstuvwxyz0123456789"));
        assert!(observed.contains("[REDACTED:openai-style-key]"));
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
    }
}

#[tokio::test]
async fn failed_terminal_write_keeps_the_last_complete_turn_restorable() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("checkpoint.sqlite3"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let database = storage::Database::open(&config).unwrap();
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response("safe first", FinishReason::Stop)),
        Ok(text_response("safe checkpoint", FinishReason::Stop)),
        Ok(text_response("resumed", FinishReason::Stop)),
    ]));
    let bus = Arc::new(EventBus::new(128));
    let mut events = bus.subscribe();
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime.delegate_background(
        Role::Worker,
        "goal".into(),
        RunConfig {
            name: Some("chat:Worker:restart".into()),
            ..RunConfig::default()
        },
    );
    runtime.wait(run).await.unwrap();
    runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if matches!(
            event.kind,
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                to: AgentRunPhase::Waiting,
                ..
            })
        ) {
            break;
        }
    }
    let checkpoint = database.run_context(&run.to_string()).unwrap().unwrap();
    assert_eq!(checkpoint.terminal_phase, "Checkpoint");
    assert!(checkpoint.restorable);
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_terminal_save BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(ABORT, 'fixture disk failure'); END;").unwrap();
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
    assert_eq!(
        database.run_context(&run.to_string()).unwrap().unwrap(),
        checkpoint
    );
    conn.execute_batch("DROP TRIGGER fail_terminal_save;")
        .unwrap();
    drop(runtime);
    let bus = Arc::new(EventBus::new(128));
    let restarted = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let next = restarted
        .delegate_chat(
            "restart",
            Role::Worker,
            "retry after restart".into(),
            RunConfig::default(),
        )
        .unwrap();
    restarted.wait(next).await.unwrap();
    let request = serde_json::to_string(&model.observed().await[2]).unwrap();
    assert!(request.contains("safe checkpoint"));
    assert!(request.contains("retry after restart"));
}
