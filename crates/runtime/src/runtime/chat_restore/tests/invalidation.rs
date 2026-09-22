use super::*;

#[tokio::test]
async fn legacy_secret_snapshot_is_redacted_on_save_and_restores_after_store_restart() {
    // Given: a legacy snapshot bypassing today's writer guard.
    let fixture = Fixture::new();
    let run = fixture.terminal().await;
    let config = storage::StorageConfig {
        db_path: fixture._dir.path().join("goal.sqlite3"),
        ..storage::StorageConfig::default()
    };
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
    let messages = serde_json::to_string(&vec![providers::Message {
        role: providers::Role::User,
        content: vec![providers::ContentBlock::Text {
            text: secret.into(),
        }],
    }])
    .unwrap();
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    conn.execute(
        "UPDATE run_contexts SET messages_json = ?1 WHERE run_id = ?2",
        rusqlite::params![messages, run.to_string()],
    )
    .unwrap();
    let store = fixture.runtime.shared.run_store.get().unwrap();
    let legacy = store.restore_record(run).unwrap().unwrap();
    assert!(legacy.restorable);
    fixture
        .storage
        .handle()
        .upsert_run_context(&legacy)
        .unwrap();
    let record = store.restore_record(run).unwrap().unwrap();
    assert!(record.restorable);
    assert!(!record.messages_json.contains(secret));
    assert!(record.messages_json.contains("[REDACTED:openai-style-key]"));
    let entry = lock_runs(&fixture.runtime.shared.runs)
        .remove(&run)
        .unwrap();
    drop(fixture.runtime);
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(CompletingModel),
    )
    .with_run_store(crate::RunStore::open(&config, fixture.storage.handle()).unwrap());
    lock_runs(&runtime.shared.runs).insert(run, entry);
    // Then: sanitized history can be restored with the existing ownership checks.
    let store = runtime.shared.run_store.get().unwrap();
    assert!(store.restore_record(run).unwrap().unwrap().restorable);
    runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap();
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
}
