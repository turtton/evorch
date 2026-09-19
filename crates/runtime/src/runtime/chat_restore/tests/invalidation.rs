use super::*;

#[tokio::test]
async fn legacy_secret_snapshot_stays_invalid_after_store_restart() {
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
    let error = fixture
        .storage
        .handle()
        .upsert_run_context(&legacy)
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains(secret));
    // When: invalidating the rejected terminal save, then replacing the entire RunStore.
    let result = store.invalidate_snapshot(run);
    assert!(
        result.is_ok(),
        "invalidation must not revalidate legacy history: {result:?}"
    );
    let record = store.restore_record(run).unwrap().unwrap();
    assert!(!record.restorable);
    assert_eq!(record.messages_json, messages);
    let entry = lock_runs(&fixture.runtime.shared.runs)
        .remove(&run)
        .unwrap();
    drop(fixture.runtime);
    let bus = Arc::new(EventBus::new(128));
    let mut events = bus.subscribe();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(CompletingModel),
    )
    .with_run_store(crate::RunStore::open(&config, fixture.storage.handle()).unwrap());
    lock_runs(&runtime.shared.runs).insert(run, entry);
    // Then: durable refusal survives restart without disclosing the legacy secret.
    let store = runtime.shared.run_store.get().unwrap();
    assert!(!store.snapshot_failed(run));
    assert!(!store.restore_record(run).unwrap().unwrap().restorable);
    let error = runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains(secret));
    assert!(matches!(error, RuntimeError::RunRestoreFailed {
        reason: RunRestoreFailure::UnsupportedConfig(ref reason), ..
    } if reason == "persist_failed"));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.recv())
            .await
            .is_err()
    );
}
