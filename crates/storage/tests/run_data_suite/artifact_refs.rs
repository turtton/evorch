//! The artifact reference tables: the writer commands used while running and the
//! `Database` methods the exclusive startup phase uses.

use storage::artifact_refs::{ArtifactRef, GcState, MetaState};
use storage::{Database, Storage, StorageConfig, StorageError};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn config(dir: &tempfile::TempDir) -> StorageConfig {
    StorageConfig {
        db_path: dir.path().join("refs.sqlite3"),
        ..Default::default()
    }
}

fn artifact(n: u32, sha: &str) -> ArtifactRef {
    ArtifactRef {
        artifact_id: format!("artifact-{n:032x}"),
        sha256: sha.into(),
        thread_id: Some("thread-1".into()),
        run_id: Some("run-1".into()),
        recorded_at_ns: i64::from(n),
    }
}

#[test]
fn writer_records_and_removes_references_and_the_database_sees_them() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    handle.record_artifact_ref(artifact(1, SHA_A)).unwrap();
    handle.record_artifact_ref(artifact(2, SHA_A)).unwrap();
    handle.record_artifact_ref(artifact(3, SHA_B)).unwrap();

    let database = Database::open(&config).unwrap();
    assert!(
        database
            .artifact_ref_exists(&artifact(1, SHA_A).artifact_id)
            .unwrap()
    );
    assert!(database.artifact_sha_referenced(SHA_A).unwrap());

    // Removing one of two references to a blob keeps the blob referenced.
    let removed = handle
        .remove_artifact_refs(&[
            artifact(1, SHA_A).artifact_id,
            artifact(9, SHA_A).artifact_id,
        ])
        .unwrap();
    assert_eq!(removed, 1, "only the row that existed is counted");
    assert!(database.artifact_sha_referenced(SHA_A).unwrap());
    handle
        .remove_artifact_refs(&[artifact(2, SHA_A).artifact_id])
        .unwrap();
    assert!(!database.artifact_sha_referenced(SHA_A).unwrap());
    assert!(database.artifact_sha_referenced(SHA_B).unwrap());
}

#[test]
fn recording_the_same_artifact_again_replaces_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    handle.record_artifact_ref(artifact(1, SHA_A)).unwrap();
    handle.record_artifact_ref(artifact(1, SHA_B)).unwrap();
    let database = Database::open(&config).unwrap();
    assert!(!database.artifact_sha_referenced(SHA_A).unwrap());
    assert!(database.artifact_sha_referenced(SHA_B).unwrap());
}

#[test]
fn a_closed_writer_is_an_error_not_a_recorded_reference() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(config(&dir)).unwrap();
    let handle = storage.handle();
    storage.close();
    assert!(matches!(
        handle.record_artifact_ref(artifact(1, SHA_A)),
        Err(StorageError::WriterClosed)
    ));
}

#[test]
fn references_are_listed_in_id_order_after_a_cursor() {
    let database = Database::open_in_memory().unwrap();
    let all = (1..=5).map(|n| artifact(n, SHA_A)).collect::<Vec<_>>();
    database.gc_insert_refs(&all).unwrap();
    let first = database.artifact_refs_after(None, 2).unwrap();
    assert_eq!(first, all[..2]);
    let next = database
        .artifact_refs_after(Some(&first[1].artifact_id), 10)
        .unwrap();
    assert_eq!(next, all[2..]);
    assert!(
        database
            .artifact_refs_after(Some(&all[4].artifact_id), 10)
            .unwrap()
            .is_empty()
    );
    assert!(database.gc_delete_ref(&all[0].artifact_id).unwrap());
    assert!(!database.gc_delete_ref(&all[0].artifact_id).unwrap());
}

#[test]
fn unknown_meta_keeps_the_first_sighting_and_the_latest_error() {
    let database = Database::open_in_memory().unwrap();
    let name = format!("artifact-{:032x}.json", 7);
    let id = name.trim_end_matches(".json");
    assert_eq!(
        database.artifact_meta_state(id).unwrap(),
        MetaState::Missing
    );
    database.gc_unknown_meta_upsert(&name, "first", 10).unwrap();
    database
        .gc_unknown_meta_upsert(&name, "second", 99)
        .unwrap();
    let listed = database.gc_unknown_meta_list(10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].first_seen_ns, listed[0].last_error.as_str()),
        (10, "second")
    );
    assert_eq!(database.gc_unknown_meta_count().unwrap(), 1);
    assert_eq!(
        database.artifact_meta_state(id).unwrap(),
        MetaState::Unknown
    );

    // A reference wins over a stale unknown row.
    database.gc_insert_refs(&[artifact(7, SHA_A)]).unwrap();
    assert_eq!(
        database.artifact_meta_state(id).unwrap(),
        MetaState::Referenced
    );
    assert!(database.gc_unknown_meta_delete(&name).unwrap());
    assert!(!database.gc_unknown_meta_delete(&name).unwrap());
    assert_eq!(database.gc_unknown_meta_count().unwrap(), 0);
}

#[test]
fn gc_state_round_trips_and_starts_incomplete() {
    let database = Database::open_in_memory().unwrap();
    assert_eq!(database.gc_state().unwrap(), GcState::default());
    let state = GcState {
        meta_cursor: Some("m".into()),
        meta_count: 7,
        meta_hash: "00ff".into(),
        blob_cursor: Some("b".into()),
        ref_cursor: Some("r".into()),
        refs_complete: true,
    };
    database.set_gc_state(&state).unwrap();
    assert_eq!(database.gc_state().unwrap(), state);
}
