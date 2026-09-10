use runtime::memory::{InterviewAnswer, MemoryBoundary};
use storage::memory::Lesson;
use storage::{Storage, StorageConfig};

#[test]
fn boundary_snapshot_does_not_change_after_later_promotion() {
    // Given: a boundary captured before any promoted lesson.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let boundary = MemoryBoundary::capture(&config, "p").unwrap();
    // When: another task promotes a lesson.
    let lesson = Lesson {
        id: "l".into(),
        project: "p".into(),
        task_id: "t".into(),
        content: "Limit parallel workers".into(),
        evidence: "test:limit".into(),
    };
    store.handle().append_lesson(&lesson).unwrap();
    store.handle().validate_lesson("l", "test:limit").unwrap();
    store.handle().promote_lesson("l").unwrap();
    // Then: only a new task boundary sees it.
    assert!(boundary.entries().is_empty());
    assert_eq!(
        MemoryBoundary::capture(&config, "p")
            .unwrap()
            .entries()
            .len(),
        1
    );
    assert!(
        MemoryBoundary::capture(&config, "other")
            .unwrap()
            .entries()
            .is_empty()
    );
}

#[test]
fn interview_answer_rejects_model_supplied_promotion() {
    // Given / When / Then: models may propose content, never promotion state.
    assert!(
        serde_json::from_str::<InterviewAnswer>(
            r#"{"content":"x","evidence":"test:x","status":"promoted"}"#
        )
        .is_err()
    );
}
