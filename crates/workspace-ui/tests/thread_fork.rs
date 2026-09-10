use serde_json::json;
use workspace_ui::{ProjectId, ThreadId, ThreadRecord};

#[test]
fn legacy_thread_migrates_without_fork_metadata() {
    // Given: persisted thread data from before session forks.
    let value = json!({
        "id": "root", "project_id": "project", "title": "Root",
        "pinned": false, "paused": false, "run_ids": [],
        "branch": null, "worktree_path": null
    });
    // When: the legacy record is loaded.
    let record: ThreadRecord = serde_json::from_value(value).expect("legacy record");
    // Then: it remains a root with no fork point.
    assert_eq!(record.parent_thread_id, None);
    assert_eq!(record.fork_event_id, None);
}

#[test]
fn fork_metadata_survives_serialization() {
    // Given: a child thread with an explicit persisted event boundary.
    let mut record = ThreadRecord::new(ThreadId::new("child"), ProjectId::new("project"), "Child");
    record.parent_thread_id = Some(ThreadId::new("root"));
    record.fork_event_id = Some(42);
    // When: the record is saved and loaded.
    let loaded: ThreadRecord =
        serde_json::from_str(&serde_json::to_string(&record).expect("serialize"))
            .expect("deserialize");
    // Then: ancestry and the exact event boundary are preserved.
    assert_eq!(loaded, record);
}
