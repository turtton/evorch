use workspace_ui::ThreadRecord;

const LEGACY_THREAD: &str = r#"{
    "id":"thread-1","project_id":"project-1","title":"Conversation",
    "pinned":false,"paused":false,"run_ids":[],"branch":null,"worktree_path":null
}"#;

#[test]
fn thread_record_deserializes_without_model_preference() {
    // Given / When: a sidebar thread written before model selection existed.
    let thread: ThreadRecord = serde_json::from_str(LEGACY_THREAD).unwrap();
    // Then: routing remains automatic.
    assert_eq!(thread.model_preference, None);
}

#[test]
fn thread_record_round_trips_with_model_preference() {
    // Given: a persisted explicit profile and model.
    let mut fixture: serde_json::Value = serde_json::from_str(LEGACY_THREAD).unwrap();
    fixture["model_preference"] = serde_json::json!({"profile":"local","model":"model-a"});
    let thread: ThreadRecord = serde_json::from_value(fixture.clone()).unwrap();
    // When: serializing and reopening the thread.
    let encoded = serde_json::to_value(&thread).unwrap();
    let restored: ThreadRecord = serde_json::from_value(encoded.clone()).unwrap();
    // Then: both the wire representation and typed preference survive.
    assert_eq!(encoded["model_preference"], fixture["model_preference"]);
    assert_eq!(restored.model_preference, thread.model_preference);
}
