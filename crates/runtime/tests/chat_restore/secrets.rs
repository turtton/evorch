use super::*;

#[tokio::test]
async fn secret_blocks_invalidate_snapshot_when_terminal_context_is_saved() {
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
        ]));
        let bus = Arc::new(EventBus::new(128));
        let mut events = bus.subscribe();
        let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
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
        // Then: old context is invalidated, and no secret reaches the stored history.
        let record = database.run_context(&run.to_string()).unwrap().unwrap();
        assert!(!record.restorable);
        assert!(
            !record
                .messages_json
                .contains("sk-abcdefghijklmnopqrstuvwxyz0123456789")
        );
        let descriptor: runtime::restore::RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).unwrap();
        assert_eq!(
            descriptor.non_restorable_reason.as_deref(),
            Some("persist_failed")
        );
        assert!(
            runtime
                .continue_goal(run, "retry".into(), RunConfig::default())
                .is_err()
        );
    }
}
