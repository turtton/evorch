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
        .record_host_chat_submission(&submission("sub-1", "digest-a", "run-1"), None)
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
        .record_host_chat_submission(&submission("sub-1", "digest-a", "run-1"), None)
        .unwrap();

    let again = handle
        .record_host_chat_submission(&submission("sub-1", "digest-b", "run-2"), None)
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
            .record_host_chat_submission(&submission("sub-1", "digest-a", "run-1"), None)
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
        assert!(handle.record_host_chat_submission(&bad, None).is_err());
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
        .record_host_chat_submission(&submission("sub-1", "digest-a", "run-1"), None)
        .unwrap();
    handle
        .record_host_chat_submission(&submission("sub-2", "digest-b", "run-2"), None)
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
        .record_host_chat_submission(&submission("sub-1", "digest-a", "run-1"), None)
        .unwrap_err();

    assert!(error.to_string().contains("tombstoned"), "{error}");
}

#[test]
fn tombstoned_runs_are_listed_even_when_nothing_else_is_left() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("submissions.sqlite3");
    let storage = Storage::open(StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    let handle = storage.handle();
    tombstone(&path, "run-1");

    let listed = handle
        .tombstoned_runs(&["run-0".into(), "run-1".into(), "run-2".into()])
        .unwrap();

    assert_eq!(listed, vec!["run-1".to_owned()]);
}

// Edit data (ADR 0045 E7): previews for lists, the words for one id.

fn accepted(handle: &storage::StorageHandle, id: &str, run: &str, text: &str) {
    handle
        .append_host_input_accepted(
            &[],
            &[],
            None,
            &storage::HostInputReceipt {
                provenance: storage::HostInputProvenance::User,
                prompt_id: id.into(),
                run_id: run.into(),
                root_run_id: run.into(),
                text: text.into(),
                status: storage::HostInputStatus::Accepted,
                reason: None,
                accepted_at_ns: 10,
                status_at_ns: 10,
                evidence_id: None,
            },
        )
        .unwrap();
}

fn reader(path: &std::path::Path) -> storage::Database {
    storage::Database::open(&StorageConfig {
        db_path: path.to_path_buf(),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn edit_data_survives_reopening_and_a_message_without_attachments_keeps_no_second_copy() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("submissions.sqlite3"),
        ..Default::default()
    };
    {
        let storage = Storage::open(config.clone()).unwrap();
        let handle = storage.handle();
        // Receipt text carries the attachment paths; the edit data keeps the words as typed.
        accepted(&handle, "with-files", "run-1", "look\n\n/files/a.png");
        accepted(&handle, "plain", "run-1", "just words");
        handle
            .record_host_chat_submission(
                &submission("with-files", "d1", "run-1"),
                storage::HostChatSubmissionEdit::new("look", 1, 2).as_ref(),
            )
            .unwrap();
        handle
            .record_host_chat_submission(
                &submission("plain", "d2", "run-1"),
                storage::HostChatSubmissionEdit::new("just words", 0, 0).as_ref(),
            )
            .unwrap();
    }

    let reopened = reader(&config.db_path);
    let previews = reopened
        .host_input_previews(&["plain".into(), "with-files".into()])
        .unwrap();
    assert_eq!(
        previews
            .iter()
            .map(|p| (p.preview.as_str(), p.image_count, p.attachment_count))
            .collect::<Vec<_>>(),
        vec![("just words", 0, 0), ("look", 1, 2)]
    );
    let files = reopened.host_input_draft("with-files").unwrap().unwrap();
    assert_eq!(files.text.as_deref(), Some("look"));
    assert_eq!((files.image_count, files.attachment_count), (1, 2));
    assert_eq!(files.status, storage::HostInputStatus::Accepted);
    // Only the attachments' words are stored apart: the plain message reads the receipt.
    let stored: Vec<Option<String>> = Connection::open(&config.db_path)
        .unwrap()
        .prepare("SELECT edit_original_text FROM host_chat_submissions ORDER BY submission_id /* plain, with-files */")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(stored, vec![None, Some("look".to_owned())]);
    let plain = reopened.host_input_draft("plain").unwrap().unwrap();
    assert_eq!(plain.text.as_deref(), Some("just words"));
}

#[test]
fn a_message_with_only_images_keeps_an_empty_body_apart_from_a_missing_one() {
    let (dir, storage) = storage();
    let handle = storage.handle();
    accepted(&handle, "images", "run-1", "\n\n/files/a.png");
    handle
        .record_host_chat_submission(
            &submission("images", "d1", "run-1"),
            storage::HostChatSubmissionEdit::new("", 1, 1).as_ref(),
        )
        .unwrap();
    // An older row, and a row with attachments whose words were never kept.
    accepted(&handle, "old", "run-1", "old words");
    handle
        .record_host_chat_submission(&submission("old", "d2", "run-1"), None)
        .unwrap();
    accepted(&handle, "lost", "run-1", "words\n\n/files/b.png");
    handle
        .record_host_chat_submission(&submission("lost", "d3", "run-1"), None)
        .unwrap();
    let path = dir.path().join("submissions.sqlite3");
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE host_chat_submissions SET edit_preview = 'words', edit_preview_truncated = 0, \
             edit_image_count = 0, edit_attachment_count = 1 WHERE submission_id = 'lost'",
            [],
        )
        .unwrap();

    let db = reader(&path);

    assert_eq!(
        db.host_input_draft("images")
            .unwrap()
            .unwrap()
            .text
            .as_deref(),
        Some("")
    );
    // Without edit data, the receipt's text is never taken for the words.
    assert_eq!(db.host_input_draft("old").unwrap(), None);
    assert_eq!(db.host_input_draft("lost").unwrap(), None);
    assert_eq!(
        db.host_input_previews(&["old".into(), "images".into()])
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn edit_data_of_a_deleted_run_is_not_returned() {
    let (dir, storage) = storage();
    let handle = storage.handle();
    accepted(&handle, "sub-1", "run-1", "words");
    handle
        .record_host_chat_submission(
            &submission("sub-1", "d1", "run-1"),
            storage::HostChatSubmissionEdit::new("words", 0, 0).as_ref(),
        )
        .unwrap();
    let path = dir.path().join("submissions.sqlite3");
    tombstone(&path, "run-1");

    let db = reader(&path);

    assert_eq!(db.host_input_draft("sub-1").unwrap(), None);
    assert!(
        db.host_input_previews(&["sub-1".into()])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_preview_is_cut_at_a_character_and_edit_data_must_agree_with_itself() {
    let words = format!("{}あ", "a".repeat(1023));
    let edit = storage::HostChatSubmissionEdit::new(&words, 0, 0).unwrap();
    assert_eq!(edit.preview, "a".repeat(1023));
    assert!(edit.preview_truncated);
    assert!(storage::HostChatSubmissionEdit::new(&"x".repeat(256 * 1024 + 1), 0, 0).is_none());

    let (_dir, storage) = storage();
    let handle = storage.handle();
    let mut more_images_than_files = storage::HostChatSubmissionEdit::new("x", 1, 1).unwrap();
    more_images_than_files.image_count = 2;
    let mut words_without_files = storage::HostChatSubmissionEdit::new("x", 0, 0).unwrap();
    words_without_files.original_text = Some("x".into());
    for bad in [more_images_than_files, words_without_files] {
        assert!(
            handle
                .record_host_chat_submission(&submission("sub-1", "d", "run-1"), Some(&bad))
                .is_err()
        );
    }
    assert!(
        handle
            .host_chat_submissions(&["sub-1".into()])
            .unwrap()
            .is_empty()
    );
}
