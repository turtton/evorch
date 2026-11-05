//! Fingerprints of accepted chat messages through the single-writer boundary.

use rusqlite::Connection;
use storage::{HostChatSubmission, HostChatSubmissionState, Storage, StorageConfig};

fn storage() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(StorageConfig {
        db_path: dir.path().join("submissions.sqlite3"),
        ..Default::default()
    })
    .unwrap();
    (dir, storage)
}

fn submission(id: &str, fingerprint: &str, run: &str) -> HostChatSubmission {
    HostChatSubmission {
        submission_id: id.into(),
        fingerprint: fingerprint.into(),
        run_id: run.into(),
        provenance_json: r#"{"actor":"human","surface":"web"}"#.into(),
        accepted_at_ns: 7,
    }
}

#[test]
fn a_recorded_submission_is_read_back_whole() {
    let (_dir, storage) = storage();
    let handle = storage.handle();

    let stored = handle
        .record_host_chat_submission(&submission("sub-1", "digest-a", "run-1"))
        .unwrap();
    let found = handle
        .host_chat_submissions(&["missing".into(), "sub-1".into()])
        .unwrap();

    assert!(stored);
    assert_eq!(
        found,
        vec![HostChatSubmissionState::Live(submission(
            "sub-1", "digest-a", "run-1"
        ))]
    );
}

#[test]
fn the_first_record_for_an_id_stays() {
    let (_dir, storage) = storage();
    let handle = storage.handle();
    handle
        .record_host_chat_submission(&submission("sub-1", "digest-a", "run-1"))
        .unwrap();

    let again = handle
        .record_host_chat_submission(&submission("sub-1", "digest-b", "run-2"))
        .unwrap();

    assert!(!again);
    assert_eq!(
        handle.host_chat_submissions(&["sub-1".into()]).unwrap(),
        vec![HostChatSubmissionState::Live(submission(
            "sub-1", "digest-a", "run-1"
        ))]
    );
}

#[test]
fn a_record_survives_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("submissions.sqlite3"),
        ..Default::default()
    };
    {
        let storage = Storage::open(config.clone()).unwrap();
        storage
            .handle()
            .record_host_chat_submission(&submission("sub-1", "digest-a", "run-1"))
            .unwrap();
    }

    let reopened = Storage::open(config).unwrap();

    assert_eq!(
        reopened
            .handle()
            .host_chat_submissions(&["sub-1".into()])
            .unwrap(),
        vec![HostChatSubmissionState::Live(submission(
            "sub-1", "digest-a", "run-1"
        ))]
    );
}

#[test]
fn an_incomplete_record_is_refused() {
    let (_dir, storage) = storage();
    let handle = storage.handle();

    for bad in [
        submission("", "digest", "run-1"),
        submission("sub-1", "", "run-1"),
        submission("sub-1", "digest", ""),
        submission("sub-1", &"x".repeat(257), "run-1"),
    ] {
        assert!(handle.record_host_chat_submission(&bad).is_err());
    }
    assert!(
        handle
            .host_chat_submissions(&["sub-1".into()])
            .unwrap()
            .is_empty()
    );
}

fn tombstone(path: &std::path::Path, run: &str) {
    Connection::open(path)
        .unwrap()
        .execute(
            "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES (?1, 't', 'f')",
            [run],
        )
        .unwrap();
}

#[test]
fn a_submission_of_a_deleted_run_is_reported_deleted_without_its_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("submissions.sqlite3");
    let storage = Storage::open(StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    let handle = storage.handle();
    handle
        .record_host_chat_submission(&submission("sub-1", "digest-a", "run-1"))
        .unwrap();
    handle
        .record_host_chat_submission(&submission("sub-2", "digest-b", "run-2"))
        .unwrap();

    tombstone(&path, "run-1");
    let found = handle
        .host_chat_submissions(&["sub-1".into(), "sub-2".into()])
        .unwrap();

    assert_eq!(
        found,
        vec![
            HostChatSubmissionState::Deleted("sub-1".into()),
            HostChatSubmissionState::Live(submission("sub-2", "digest-b", "run-2")),
        ]
    );
    let reader = storage::Database::open(&StorageConfig {
        db_path: path,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        reader.host_chat_submissions(&["sub-1".into()]).unwrap(),
        found[..1]
    );
}

#[test]
fn a_deleted_run_accepts_no_new_submission() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("submissions.sqlite3");
    let storage = Storage::open(StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    tombstone(&path, "run-1");

    let error = storage
        .handle()
        .record_host_chat_submission(&submission("sub-1", "digest-a", "run-1"))
        .unwrap_err();

    assert!(error.to_string().contains("tombstoned"), "{error}");
}
