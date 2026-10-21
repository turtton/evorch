//! ADR 0037: explicit offline reclamation safety checks.
use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner, compact_owner_checkpoints};

fn fixture() -> (tempfile::TempDir, OwnerPermit) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owners.db");
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    Registry::open(&path).unwrap().start(&owner).unwrap();
    (
        directory,
        OwnerPermit {
            registry_path: path,
            thread_id: owner.thread_id,
            lease: owner.lease,
            run_id: Some("parent".into()),
        },
    )
}
fn payload(permit: &OwnerPermit) -> String {
    rusqlite::Connection::open(&permit.registry_path)
        .unwrap()
        .query_row(
            "SELECT payload FROM owner_checkpoints WHERE thread_id='thread'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}
#[test]
fn offline_cleanup_refuses_missing_ack_sockets_and_live_readers_then_vacuums() {
    let (directory, permit) = fixture();
    let connection = rusqlite::Connection::open(&permit.registry_path).unwrap();
    let secret = "legacy-unredacted-marker";
    connection
        .execute(
            "INSERT INTO owner_checkpoints VALUES ('thread','1',?1)",
            [format!("[\"{secret}{}\"]", "x".repeat(2_000_000))],
        )
        .unwrap();
    let before = std::fs::metadata(&permit.registry_path).unwrap().len();
    assert!(compact_owner_checkpoints(directory.path(), false).is_err());
    std::fs::write(directory.path().join("owner.sock"), "stale-or-live").unwrap();
    assert!(compact_owner_checkpoints(directory.path(), true).is_err());
    std::fs::remove_file(directory.path().join("owner.sock")).unwrap();
    let guard = permit.mutation_guard().unwrap();
    assert!(compact_owner_checkpoints(directory.path(), true).is_err());
    assert!(payload(&permit).contains(secret));
    drop(guard);
    drop(connection);
    assert_eq!(
        compact_owner_checkpoints(directory.path(), true).unwrap(),
        1
    );
    assert_eq!(payload(&permit), "[]");
    assert!(std::fs::metadata(&permit.registry_path).unwrap().len() < before / 2);
    let bytes = std::fs::read(&permit.registry_path).unwrap();
    assert!(
        !bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
    );
    assert_eq!(
        compact_owner_checkpoints(directory.path(), true).unwrap(),
        1
    );
}

#[test]
fn offline_cleanup_never_creates_a_db_and_refuses_wal_or_a_live_writer() {
    let empty = tempfile::tempdir().unwrap();
    assert!(compact_owner_checkpoints(empty.path(), true).is_err());
    assert!(!empty.path().join("owners.db").exists());

    let (directory, permit) = fixture();
    let connection = rusqlite::Connection::open(&permit.registry_path).unwrap();
    connection
        .execute(
            "INSERT INTO owner_checkpoints VALUES ('thread','1','[\"legacy\"]')",
            [],
        )
        .unwrap();
    let owner_before = Registry::open_existing(&permit.registry_path)
        .unwrap()
        .attach("thread")
        .unwrap();
    let schema_before: String = connection
        .query_row(
            "SELECT group_concat(sql) FROM sqlite_schema ORDER BY name",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(compact_owner_checkpoints(directory.path(), true).is_err());
    connection
        .execute_batch("ROLLBACK; PRAGMA journal_mode=WAL")
        .unwrap();
    assert!(compact_owner_checkpoints(directory.path(), true).is_err());
    assert_eq!(payload(&permit), "[\"legacy\"]");
    connection
        .execute_batch("PRAGMA journal_mode=DELETE")
        .unwrap();
    compact_owner_checkpoints(directory.path(), true).unwrap();
    assert_eq!(
        Registry::open_existing(&permit.registry_path)
            .unwrap()
            .attach("thread")
            .unwrap(),
        owner_before
    );
    let schema_after: String = connection
        .query_row(
            "SELECT group_concat(sql) FROM sqlite_schema ORDER BY name",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(schema_before, schema_after);
}
