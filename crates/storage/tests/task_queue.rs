use std::time::UNIX_EPOCH;
use storage::entity::{TaskRecord, TaskStatus};
use storage::{Database, Storage, StorageConfig};

#[test]
fn dependency_completion_releases_pending_task() {
    // Given: two queued tasks and an edge.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("queue.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    for id in ["a", "b"] {
        store
            .handle()
            .enqueue_task(&TaskRecord {
                id: id.into(),
                session_id: None,
                status: TaskStatus::Pending,
                parent_run_id: None,
                input: None,
                progress: None,
                last_artifact: None,
                failure_reason: None,
                resume_cursor: None,
                attempts: 0,
                heartbeat_at: None,
                created_at: UNIX_EPOCH,
                updated_at: UNIX_EPOCH,
            })
            .unwrap();
    }
    store.handle().link_tasks("a", "b").unwrap();
    let db = Database::open(&config).unwrap();
    assert_eq!(db.task("b").unwrap().unwrap().status, TaskStatus::Blocked);
    assert!(store.handle().start_task("b").is_err());
    assert!(store.handle().link_tasks("b", "a").is_err());
    // When: the blocker completes.
    store.handle().start_task("a").unwrap();
    store.handle().finish_task("a", true).unwrap();
    // Then: dependency remains visible, but the task becomes runnable.
    assert_eq!(db.task("b").unwrap().unwrap().status, TaskStatus::Pending);
    assert_eq!(db.task_dependencies("b").unwrap().blocked_by, vec!["a"]);
    store.handle().start_task("b").unwrap();
}

#[test]
fn task_record_round_trips_all_durable_fields() {
    // Given: a pending task with opaque durable payloads and nanosecond timestamps.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("durable.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let record = TaskRecord {
        id: "durable".into(),
        session_id: None,
        status: TaskStatus::Pending,
        parent_run_id: Some("parent-run".into()),
        input: Some(r#"{"prompt":"hello"}"#.into()),
        progress: Some(r#"{"step":2}"#.into()),
        last_artifact: Some(r#"{"path":"result.txt"}"#.into()),
        failure_reason: Some("retryable error".into()),
        resume_cursor: Some(r#"{"offset":42}"#.into()),
        attempts: 3,
        heartbeat_at: Some(UNIX_EPOCH + std::time::Duration::from_nanos(123)),
        created_at: UNIX_EPOCH,
        updated_at: UNIX_EPOCH,
    };
    // When: the writer creates and commits the task.
    store.handle().enqueue_task(&record).unwrap();
    // Then: reading, including after reopening, preserves every field.
    let db = Database::open(&config).unwrap();
    assert_eq!(db.task("durable").unwrap(), Some(record.clone()));
    store.close();
    assert_eq!(
        Database::open(&config).unwrap().task("durable").unwrap(),
        Some(record)
    );
}
