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
