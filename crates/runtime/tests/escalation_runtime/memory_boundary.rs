use runtime::memory::MemoryBoundary;
use storage::memory::{Lesson, LessonScope};
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
        scope: LessonScope::Project,
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
fn boundary_routes_lessons_by_scope() {
    // Given: promoted lessons of every scope, recorded by two projects.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    for (id, project, scope) in [
        ("own-project", "p", LessonScope::Project),
        ("other-project", "q", LessonScope::Project),
        ("other-user", "q", LessonScope::User),
        ("own-harness", "p", LessonScope::Harness),
    ] {
        store
            .handle()
            .append_lesson(&Lesson {
                id: id.into(),
                project: project.into(),
                task_id: "t".into(),
                content: format!("lesson {id}"),
                evidence: format!("test:{id}"),
                scope,
            })
            .unwrap();
        store
            .handle()
            .validate_lesson(id, &format!("test:{id}"))
            .unwrap();
        store.handle().promote_lesson(id).unwrap();
    }
    // When: a task in project p captures its boundary.
    let boundary = MemoryBoundary::capture(&config, "p").unwrap();
    // Then: it sees its own project lessons and every user lesson, never harness ones.
    let mut ids: Vec<_> = boundary
        .entries()
        .iter()
        .map(|entry| (entry.lesson.id.as_str(), entry.lesson.scope))
        .collect();
    ids.sort_unstable_by_key(|(id, _)| *id);
    assert_eq!(
        ids,
        [
            ("other-user", LessonScope::User),
            ("own-project", LessonScope::Project)
        ]
    );
}
