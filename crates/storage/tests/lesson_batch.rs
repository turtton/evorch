use storage::memory::{Lesson, MemoryStatus};
use storage::{Database, Storage, StorageConfig};

fn lesson(id: &str) -> Lesson {
    Lesson {
        id: id.into(),
        project: "p".into(),
        task_id: "t".into(),
        content: "Bound concurrency".into(),
        evidence: "test:bound".into(),
    }
}

#[test]
fn batch_rolls_back_when_second_lesson_is_invalid() {
    // Given: real storage and a batch with invalid trailing evidence.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let mut invalid = lesson("reviewer");
    invalid.evidence.clear();
    // When: submit the batch.
    assert!(
        store
            .handle()
            .append_lessons(&[lesson("worker"), invalid])
            .is_err()
    );
    // Then: neither ledger nor projection contains a partial interview.
    let db = Database::open(&config).unwrap();
    assert!(db.memory_history("worker").unwrap().is_empty());
    assert!(db.search_memory("p", "", None).unwrap().is_empty());
}

#[test]
fn replay_preserves_promoted_status_and_history() {
    // Given: a completed batch whose worker lesson was promoted.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let lessons = [lesson("worker"), lesson("reviewer")];
    let writer = store.handle();
    writer.append_lessons(&lessons).unwrap();
    writer.validate_lesson("worker", "test:bound").unwrap();
    writer.promote_lesson("worker").unwrap();
    // When: retry the same append.
    writer.append_lessons(&lessons).unwrap();
    // Then: no extra history or state regression occurs.
    let db = Database::open(&config).unwrap();
    assert_eq!(db.memory_history("worker").unwrap().len(), 3);
    assert_eq!(db.memory_history("reviewer").unwrap().len(), 1);
    assert_eq!(
        db.search_memory("p", "", Some(MemoryStatus::Promoted))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn conflicting_replay_rolls_back_new_members() {
    // Given: an existing lesson and a conflicting duplicate after a new member.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    store.handle().append_lesson(&lesson("reviewer")).unwrap();
    let mut conflict = lesson("reviewer");
    conflict.content = "Different lesson".into();
    // When: append the conflicting batch.
    assert!(
        store
            .handle()
            .append_lessons(&[lesson("worker"), conflict])
            .is_err()
    );
    // Then: the new member rolls back and the original remains unchanged.
    let db = Database::open(&config).unwrap();
    assert!(db.memory_history("worker").unwrap().is_empty());
    assert_eq!(
        db.memory_history("reviewer").unwrap()[0].lesson,
        lesson("reviewer")
    );
}

#[test]
fn concurrent_replays_append_one_batch() {
    // Given: multiple producers sharing a writer and identical lessons.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    // When: producers append concurrently.
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let writer = store.handle();
                scope.spawn(move || writer.append_lessons(&[lesson("worker"), lesson("reviewer")]))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    });
    // Then: each lesson has exactly one ledger record.
    let db = Database::open(&config).unwrap();
    assert_eq!(db.memory_history("worker").unwrap().len(), 1);
    assert_eq!(db.memory_history("reviewer").unwrap().len(), 1);
}
