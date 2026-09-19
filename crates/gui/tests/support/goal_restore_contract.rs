use super::*;

#[test]
fn composer_cancel_reaches_restored_goal_and_followup_restores_again() {
    // Given: a terminal goal resumed through the composer.
    let mut fixture = Fixture::with_success(true);
    fixture.submit("/goal fixture");
    let root = fixture.wait_phase(AgentRunPhase::Done);
    fixture.submit("continue");
    assert_eq!(fixture.wait_phase(AgentRunPhase::Waiting), root);
    // When: composer cancellation is followed by another ordinary follow-up.
    let cancel = fixture.sink.submit(WorkbenchCommand::CancelChat {
        thread_id: "thread-1".into(),
    });
    assert!(cancel.is_empty(), "{cancel:?}");
    fixture.rt.block_on(fixture.runtime.wait(root)).unwrap();
    let events = fixture.submit("continue after cancel");
    // Then: the same root resumes with the complete history and no extra goal.
    assert!(
        matches!(events.as_slice(), [LoopEvent::ChatAccepted { run_id, .. }] if *run_id == root.to_string())
    );
    assert_eq!(fixture.wait_phase(AgentRunPhase::Waiting), root);
    assert_eq!(fixture.created, 1);
    let requests = fixture.messages.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2]
            .iter()
            .filter(|message| message.role == providers::Role::User)
            .count(),
        3
    );
}

#[test]
fn composer_rejects_nonrestorable_goal_with_recorded_reason() {
    // Given: a terminal goal with a disabled snapshot.
    let mut fixture = Fixture::with_success(true);
    fixture.submit("/goal fixture");
    let root = fixture.wait_phase(AgentRunPhase::Done);
    let database = storage::Database::open(&storage::StorageConfig {
        db_path: fixture._directory.path().join("test.sqlite3"),
        ..storage::StorageConfig::default()
    })
    .unwrap();
    let mut record = database.run_context(&root.to_string()).unwrap().unwrap();
    let mut descriptor: runtime::restore::RunRestoreDescriptor =
        serde_json::from_str(&record.config_json).unwrap();
    descriptor.restorable = false;
    descriptor.non_restorable_reason = Some("snapshot_consumed".into());
    record.restorable = false;
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    fixture
        ._storage
        .handle()
        .upsert_run_context(&record)
        .unwrap();
    // When: the composer requests a continuation.
    let events = fixture.submit("continue");
    // Then: rejection is visible instead of silently replaying old context.
    assert!(
        matches!(events.as_slice(), [LoopEvent::ChatRejected { reason, .. }] if reason.contains("snapshot_consumed"))
    );
    assert_eq!(fixture.messages.lock().unwrap().len(), 1);
}
