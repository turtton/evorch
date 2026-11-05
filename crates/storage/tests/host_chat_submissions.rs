//! Fingerprints of accepted chat messages through the single-writer boundary.

use storage::{HostChatSubmission, Storage, StorageConfig};

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
    assert_eq!(found, vec![submission("sub-1", "digest-a", "run-1")]);
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
        vec![submission("sub-1", "digest-a", "run-1")]
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
        vec![submission("sub-1", "digest-a", "run-1")]
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
