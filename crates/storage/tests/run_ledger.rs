//! Run ledger persistence through the public storage API.

use rusqlite::Connection;
use storage::{Database, Storage, StorageConfig, StorageError};
use tempfile::TempDir;

fn config(temp: &TempDir) -> StorageConfig {
    StorageConfig {
        db_path: temp.path().join("ledger.db"),
        ..StorageConfig::default()
    }
}

#[test]
fn append_then_read_round_trips_in_seq_order() {
    // Given: a writer and interleaved runs.
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    // When: entries are appended through the writer.
    let first = handle.append_run_ledger("a", "first").unwrap();
    let other = handle.append_run_ledger("b", "other").unwrap();
    let last = handle.append_run_ledger("a", "last").unwrap();
    // Then: run filtering and global sequence order preserve the payloads.
    let db = Database::open(&config).unwrap();
    let entries = db.run_ledger("a").unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|e| (e.seq, e.run_id.as_str(), e.body.as_str()))
            .collect::<Vec<_>>(),
        vec![(first, "a", "first"), (last, "a", "last")]
    );
    assert!(entries.iter().all(|e| e.created_at_ns > 0));
    assert_eq!(
        db.run_ledger_all()
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect::<Vec<_>>(),
        vec![first, other, last]
    );
    assert!(first < other && other < last);
    assert!(db.run_ledger("missing").unwrap().is_empty());
}

#[test]
fn update_and_delete_are_rejected_by_triggers() {
    // Given: a persisted ledger row and a raw SQLite connection.
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .append_run_ledger("a", "immutable")
        .unwrap();
    storage.close();
    let conn = Connection::open(&config.db_path).unwrap();
    // When: either prohibited mutation is attempted.
    for sql in [
        "UPDATE run_ledger SET body = 'changed'",
        "DELETE FROM run_ledger",
    ] {
        let error = conn.execute(sql, []).unwrap_err();
        // Then: SQLite reports the trigger constraint, not an unrelated SQL error.
        assert!(
            matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER)
        );
    }
}

#[test]
fn replace_into_existing_seq_is_rejected() {
    // Given: a public-API row and a connection with recursive triggers disabled.
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    let storage = Storage::open(config.clone()).unwrap();
    let seq = storage
        .handle()
        .append_run_ledger("a", "immutable")
        .unwrap();
    storage.close();
    let before = Database::open(&config).unwrap().run_ledger_all().unwrap();
    let conn = Connection::open(&config.db_path).unwrap();
    conn.pragma_update(None, "recursive_triggers", 0).unwrap();
    // When: either REPLACE spelling attempts to overwrite the existing sequence.
    for sql in [
        "INSERT OR REPLACE INTO run_ledger VALUES (?1, 'b', 'changed', 1)",
        "REPLACE INTO run_ledger VALUES (?1, 'b', 'changed', 1)",
    ] {
        let result = conn.execute(sql, [i64::try_from(seq).unwrap()]);
        // Then: the insert trigger aborts and every original field is preserved.
        let error = result.unwrap_err();
        assert!(
            matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER)
        );
        assert_eq!(
            Database::open(&config).unwrap().run_ledger_all().unwrap(),
            before
        );
    }
}

#[test]
fn replace_into_new_seq_behaves_like_insert_or_is_rejected() {
    // Given: an existing row; fresh sequences are allowed because no row is replaced.
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    let storage = Storage::open(config.clone()).unwrap();
    let seq = storage.handle().append_run_ledger("a", "original").unwrap();
    storage.close();
    let before = Database::open(&config).unwrap().run_ledger_all().unwrap();
    let conn = Connection::open(&config.db_path).unwrap();
    conn.pragma_update(None, "recursive_triggers", 0).unwrap();
    // When: REPLACE targets a fresh explicit sequence.
    let inserted = conn
        .execute(
            "INSERT OR REPLACE INTO run_ledger VALUES (?1, 'b', 'new', 1)",
            [i64::try_from(seq + 1).unwrap()],
        )
        .unwrap();
    // Then: it appends the new row without changing the original.
    let entries = Database::open(&config).unwrap().run_ledger_all().unwrap();
    assert_eq!(inserted, 1);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0], before[0]);
    assert_eq!(
        (
            entries[1].seq,
            entries[1].run_id.as_str(),
            entries[1].body.as_str(),
            entries[1].created_at_ns
        ),
        (seq + 1, "b", "new", 1)
    );
}

#[test]
fn entries_survive_writer_close_and_reopen() {
    // Given: an entry persisted by a closed writer.
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    let storage = Storage::open(config.clone()).unwrap();
    let first = storage.handle().append_run_ledger("a", "before").unwrap();
    storage.close();
    let before = Database::open(&config).unwrap().run_ledger("a").unwrap();
    // When: a new writer appends to the same database.
    let storage = Storage::open(config.clone()).unwrap();
    let second = storage.handle().append_run_ledger("a", "after").unwrap();
    storage.close();
    // Then: the old row survives exactly and sequence allocation continues.
    let entries = Database::open(&config).unwrap().run_ledger("a").unwrap();
    assert_eq!(entries[0], before[0]);
    assert_eq!(entries[1].body, "after");
    assert_eq!(entries[1].seq, second);
    assert!(second > first);
}

#[test]
fn empty_and_oversize_bodies_rejected() {
    // Given: bodies outside the byte-length bounds, including multibyte text.
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    let storage = Storage::open(config.clone()).unwrap();
    for body in [String::new(), "x".repeat(8193), "é".repeat(4097)] {
        // When: an invalid body is appended.
        let result = storage.handle().append_run_ledger("a", &body);
        // Then: a typed length error is returned without inserting a row.
        assert_eq!(
            result,
            Err(StorageError::InvalidRunLedgerBody { bytes: body.len() })
        );
    }
    assert!(
        Database::open(&config)
            .unwrap()
            .run_ledger_all()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn maximum_size_body_is_accepted() {
    // Given: exactly 8 KiB of UTF-8 text.
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    let storage = Storage::open(config.clone()).unwrap();
    let body = "é".repeat(4096);
    // When: the boundary-sized body is appended.
    storage.handle().append_run_ledger("a", &body).unwrap();
    // Then: all bytes round-trip.
    assert_eq!(
        Database::open(&config).unwrap().run_ledger("a").unwrap()[0].body,
        body
    );
}

#[test]
fn secret_body_is_rejected() {
    // Given: a credential-shaped body.
    let temp = TempDir::new().unwrap();
    let storage = Storage::open(config(&temp)).unwrap();
    // When: attempting to persist the credential.
    let result = storage
        .handle()
        .append_run_ledger("a", "sk-test-evorch-9f8e7d6c5b4a3f2e1d");
    // Then: the ledger/body guard rejects it.
    assert!(matches!(
        result,
        Err(StorageError::SecretDetected {
            entity: "ledger",
            field: "body",
            ..
        })
    ));
}

#[test]
fn suspended_writer_rejects_ledger_append() {
    // Given: a database that already exceeds its configured size limit.
    let temp = TempDir::new().unwrap();
    let mut config = config(&temp);
    config.hard_limits.max_db_bytes = 1;
    let storage = Storage::open(config.clone()).unwrap();
    // When: appending while suspended.
    let result = storage.handle().append_run_ledger("a", "body");
    // Then: no ledger entry is persisted.
    assert!(result.is_err());
    assert!(
        Database::open(&config)
            .unwrap()
            .run_ledger_all()
            .unwrap()
            .is_empty()
    );
}
