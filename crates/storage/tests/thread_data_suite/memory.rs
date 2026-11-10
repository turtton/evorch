use storage::memory::{Lesson, LessonScope, MemoryStatus};
use storage::{Database, Storage, StorageConfig};

#[test]
fn candidates_are_searchable_and_history_survives_promotion() {
    // Given: a single writer and a candidate lesson.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let lesson = Lesson {
        id: "lesson-1".into(),
        project: "p".into(),
        task_id: "t".into(),
        content: "Use bounded concurrency".into(),
        evidence: "test:bounded".into(),
        scope: LessonScope::Project,
    };
    // When: append, validate, and deterministically promote.
    store.handle().append_lesson(&lesson).unwrap();
    store
        .handle()
        .validate_lesson("lesson-1", "test:bounded")
        .unwrap();
    store.handle().promote_lesson("lesson-1").unwrap();
    // Then: current projection is searchable, all three records remain.
    let db = Database::open(&config).unwrap();
    let found = db
        .search_memory("p", "bounded", Some(MemoryStatus::Promoted))
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].lesson, lesson);
    assert_eq!(db.memory_history("lesson-1").unwrap().len(), 3);
}

#[test]
fn candidate_cannot_promote_without_validation() {
    // Given: an unvalidated candidate.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config).unwrap();
    store
        .handle()
        .append_lesson(&Lesson {
            id: "l".into(),
            project: "p".into(),
            task_id: "t".into(),
            content: "Bound workers".into(),
            evidence: "test:x".into(),
            scope: LessonScope::Project,
        })
        .unwrap();
    // When / Then: promotion fails closed.
    assert!(store.handle().promote_lesson("l").is_err());
}

#[test]
fn ledger_rejects_updates_and_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    store
        .handle()
        .append_lesson(&Lesson {
            id: "l".into(),
            project: "p".into(),
            task_id: "t".into(),
            content: "Bound workers".into(),
            evidence: "test:x".into(),
            scope: LessonScope::Project,
        })
        .unwrap();
    store.close();
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    assert!(
        conn.execute("UPDATE memory_ledger SET content='overwritten'", [])
            .is_err()
    );
    assert!(conn.execute("DELETE FROM memory_ledger", []).is_err());
    assert_eq!(
        Database::open(&config)
            .unwrap()
            .memory_history("l")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn rejected_lessons_remain_in_history_but_not_promoted_search() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    store
        .handle()
        .append_lesson(&Lesson {
            id: "l".into(),
            project: "p".into(),
            task_id: "t".into(),
            content: "Bound workers".into(),
            evidence: "test:x".into(),
            scope: LessonScope::Project,
        })
        .unwrap();
    assert!(store.handle().validate_lesson("l", "unrelated").is_err());
    store.handle().reject_lesson("l").unwrap();
    assert!(store.handle().promote_lesson("l").is_err());
    let db = Database::open(&config).unwrap();
    assert_eq!(db.memory_history("l").unwrap().len(), 2);
    assert!(
        db.search_memory("p", "workers", Some(MemoryStatus::Promoted))
            .unwrap()
            .is_empty()
    );
    assert!(
        db.search_memory("other", "workers", None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn lesson_scope_survives_every_transition() {
    // Given: a harness-scoped candidate.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let lesson = Lesson {
        id: "harness-1".into(),
        project: "p".into(),
        task_id: "t".into(),
        content: "Shell polls need a bounded yield".into(),
        evidence: "test:yield".into(),
        scope: LessonScope::Harness,
    };
    storage.handle().append_lesson(&lesson).unwrap();
    // When: it is validated and promoted.
    storage
        .handle()
        .validate_lesson("harness-1", "test:yield")
        .unwrap();
    storage.handle().promote_lesson("harness-1").unwrap();
    // Then: every ledger row and the projection keep the harness scope,
    // and the project boundary query never returns it.
    let database = Database::open(&config).unwrap();
    let history = database.memory_history("harness-1").unwrap();
    assert_eq!(history.len(), 3);
    assert!(history.iter().all(|entry| entry.lesson == lesson));
    assert_eq!(
        database
            .search_memory("p", "", Some(MemoryStatus::Promoted))
            .unwrap()[0]
            .lesson
            .scope,
        LessonScope::Harness
    );
    assert!(database.boundary_memory("p").unwrap().is_empty());
    // And: a re-append that only changes scope is a conflicting duplicate.
    assert!(
        storage
            .handle()
            .append_lesson(&Lesson {
                scope: LessonScope::Project,
                ..lesson
            })
            .is_err()
    );
}
