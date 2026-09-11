use storage::{Database, Storage, StorageConfig, memory::Lesson};

#[test]
fn findings_are_append_only_and_do_not_become_lessons() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let db = Database::open(&config).unwrap();
    let writer = Storage::open(config.clone()).unwrap();
    let finding = Lesson {
        id: "f".into(),
        project: "p".into(),
        task_id: "t".into(),
        content: "observed".into(),
        evidence: "test:x".into(),
    };
    writer.handle().append_finding(&finding).unwrap();
    writer.handle().append_finding(&finding).unwrap();
    assert_eq!(db.findings("p").unwrap(), [finding.clone(), finding]);
    assert!(db.search_memory("p", "", None).unwrap().is_empty());
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    assert!(
        conn.execute("DELETE FROM memory_ledger WHERE kind='finding'", [])
            .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE memory_ledger SET content='changed' WHERE kind='finding'",
            []
        )
        .is_err()
    );
}

#[test]
fn finding_is_rejected_when_writer_is_suspended() {
    // Given: an exhausted storage budget.
    let dir = tempfile::tempdir().unwrap();
    let mut config = StorageConfig {
        db_path: dir.path().join("limited.db"),
        ..Default::default()
    };
    config.hard_limits.max_db_bytes = 0;
    let writer = Storage::open(config.clone()).unwrap();
    let finding = Lesson {
        id: "f".into(),
        project: "p".into(),
        task_id: "t".into(),
        content: "observed".into(),
        evidence: "test:x".into(),
    };
    // When / Then: findings obey the same suspension as lessons.
    assert!(writer.handle().append_finding(&finding).is_err());
    assert!(
        Database::open(&config)
            .unwrap()
            .findings("p")
            .unwrap()
            .is_empty()
    );
}
