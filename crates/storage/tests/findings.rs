use storage::{Database, StorageConfig, memory::Lesson};

#[test]
fn findings_are_append_only_and_do_not_become_lessons() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let db = Database::open(&config).unwrap();
    let finding = Lesson {
        id: "f".into(),
        project: "p".into(),
        task_id: "t".into(),
        content: "observed".into(),
        evidence: "test:x".into(),
    };
    db.append_finding(&finding).unwrap();
    db.append_finding(&finding).unwrap();
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
