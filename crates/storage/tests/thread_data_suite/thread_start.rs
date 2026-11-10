//! Thread start states through the single-writer boundary (ADR 0036 §2).

use std::time::{Duration, SystemTime};

use storage::thread_start::{
    MAX_THREAD_START_BYTES, NotReplayableReason, ThreadStartCapture, ThreadStartSnapshot,
    ThreadStartState, UntrackedEntry, UntrackedKind,
};
use storage::{Database, Storage, StorageConfig, system_time_to_ns};

const SECRET: &str = "sk-test-evorch-9f8e7d6c5b4a3f2e1d";

fn fixture() -> (tempfile::TempDir, Storage, Database) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("thread-start.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let database = Database::open(&config).unwrap();
    (dir, storage, database)
}

fn entry(path: &str, kind: UntrackedKind, executable: bool, content: &[u8]) -> UntrackedEntry {
    UntrackedEntry {
        path: path.into(),
        kind,
        executable,
        content: content.to_vec(),
    }
}

fn snapshot(diff: &[u8], untracked: Vec<UntrackedEntry>) -> ThreadStartSnapshot {
    ThreadStartSnapshot {
        repo_prefix: "crates/app/".into(),
        head_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        head_ref: Some("refs/heads/main".into()),
        diff: diff.to_vec(),
        untracked,
    }
}

fn state(thread_id: &str, captured_at_ns: i64, capture: ThreadStartCapture) -> ThreadStartState {
    ThreadStartState {
        thread_id: thread_id.into(),
        root_run_id: format!("run-{captured_at_ns}"),
        project_root: "/work/repo/crates/app".into(),
        captured_at_ns,
        capture,
    }
}

fn days_ago(days: u64) -> i64 {
    system_time_to_ns(SystemTime::now() - Duration::from_secs(days * 86_400)).unwrap()
}

#[test]
fn replayable_state_round_trips_with_every_untracked_kind() {
    // Given: a replayable start with a binary diff, a plain, an executable,
    // a binary and a symlink untracked entry.
    let (_dir, storage, database) = fixture();
    let start = state(
        "thread",
        7,
        ThreadStartCapture::Replayable(snapshot(
            b"diff --git a/x b/x\nGIT binary patch\n\x00\xff",
            vec![
                entry("bin/run.sh", UntrackedKind::File, true, b"#!/bin/sh\n"),
                entry("data.bin", UntrackedKind::File, false, &[0, 159, 146, 150]),
                entry("link", UntrackedKind::Symlink, false, b"../target"),
                entry("notes.md", UntrackedKind::File, false, b"draft"),
            ],
        )),
    );

    // When: it is saved and read back.
    let inserted = storage
        .handle()
        .save_thread_start_state(start.clone())
        .unwrap();

    // Then: every byte, kind and mode survives.
    assert!(inserted);
    assert_eq!(database.thread_start_state("thread").unwrap(), Some(start));
    assert_eq!(database.thread_start_state("missing").unwrap(), None);
}

#[test]
fn detached_start_round_trips_without_a_head_ref() {
    // Given: a replayable start recorded while HEAD was detached.
    let (_dir, storage, database) = fixture();
    let start = state(
        "detached",
        3,
        ThreadStartCapture::Replayable(ThreadStartSnapshot {
            head_ref: None,
            ..snapshot(b"", vec![])
        }),
    );

    // When: it is saved and read back.
    storage
        .handle()
        .save_thread_start_state(start.clone())
        .unwrap();

    // Then: the missing head ref survives as `None`.
    assert_eq!(
        database.thread_start_state("detached").unwrap(),
        Some(start)
    );
}

#[test]
fn first_state_of_a_thread_wins() {
    // Given: a thread whose start was already recorded.
    let (_dir, storage, database) = fixture();
    let first = state(
        "thread",
        1,
        ThreadStartCapture::Replayable(snapshot(b"first", vec![])),
    );
    storage
        .handle()
        .save_thread_start_state(first.clone())
        .unwrap();

    // When: a later run of the same thread saves another state.
    let later = state(
        "thread",
        2,
        ThreadStartCapture::Replayable(snapshot(
            b"later",
            vec![entry("new", UntrackedKind::File, false, b"x")],
        )),
    );
    let inserted = storage.handle().save_thread_start_state(later).unwrap();

    // Then: the save is ignored without an error.
    assert!(!inserted);
    assert_eq!(database.thread_start_state("thread").unwrap(), Some(first));
}

#[test]
fn not_replayable_state_keeps_only_its_reason() {
    // Given: one not-replayable start per reason.
    let (_dir, storage, database) = fixture();
    let reasons = [
        NotReplayableReason::NotGitRepository,
        NotReplayableReason::NoCommit,
        NotReplayableReason::TooLarge,
        NotReplayableReason::ContainsSecret,
        NotReplayableReason::TimedOut,
        NotReplayableReason::CaptureFailed,
    ];
    for (index, reason) in reasons.into_iter().enumerate() {
        let start = state(
            &format!("thread-{index}"),
            index as i64,
            ThreadStartCapture::NotReplayable(reason),
        );

        // When: it is saved and read back.
        storage
            .handle()
            .save_thread_start_state(start.clone())
            .unwrap();

        // Then: it carries the reason and nothing else.
        assert_eq!(
            database.thread_start_state(&start.thread_id).unwrap(),
            Some(start)
        );
    }
}

#[test]
fn schema_rejects_content_on_not_replayable_rows() {
    // Given: a not-replayable start persisted through the writer.
    let (dir, storage, _database) = fixture();
    storage
        .handle()
        .save_thread_start_state(state(
            "thread",
            1,
            ThreadStartCapture::NotReplayable(NotReplayableReason::TooLarge),
        ))
        .unwrap();
    storage.close();
    let connection = rusqlite::Connection::open(dir.path().join("thread-start.sqlite3")).unwrap();

    // When: raw SQL tries to attach content to it or to store a diff with a reason.
    let untracked = connection.execute(
        "INSERT INTO thread_start_untracked VALUES ('thread', 'a', 'file', 0, x'00')",
        [],
    );
    let mixed = connection.execute(
        "INSERT INTO thread_start_states VALUES ('other', 'run-1', '/p', NULL, NULL, x'00', 1, 0, 'too_large')",
        [],
    );

    // Then: both are rejected.
    assert!(untracked.is_err());
    assert!(mixed.is_err());
}

#[test]
fn secret_bearing_content_is_stored_as_not_replayable() {
    // Given: starts whose diff, untracked content or untracked path holds a secret.
    let (_dir, storage, database) = fixture();
    let cases = [
        snapshot(format!("+key = {SECRET}\n").as_bytes(), vec![]),
        snapshot(
            b"",
            vec![entry(
                ".env",
                UntrackedKind::File,
                false,
                format!("KEY={SECRET}").as_bytes(),
            )],
        ),
        snapshot(
            b"",
            vec![entry(
                &format!("keys/{SECRET}"),
                UntrackedKind::File,
                false,
                b"",
            )],
        ),
    ];
    for (index, snapshot) in cases.into_iter().enumerate() {
        let thread_id = format!("thread-{index}");

        // When: it is saved.
        let inserted = storage
            .handle()
            .save_thread_start_state(state(
                &thread_id,
                1,
                ThreadStartCapture::Replayable(snapshot),
            ))
            .unwrap();

        // Then: only the contains_secret reason reaches the database.
        assert!(inserted);
        assert_eq!(
            database
                .thread_start_state(&thread_id)
                .unwrap()
                .unwrap()
                .capture,
            ThreadStartCapture::NotReplayable(NotReplayableReason::ContainsSecret)
        );
    }
}

#[test]
fn content_over_the_limit_is_rejected() {
    // Given: a start whose diff and untracked content together exceed 8 MiB by one byte.
    let (_dir, storage, database) = fixture();
    let half = MAX_THREAD_START_BYTES / 2;
    let over = state(
        "over",
        1,
        ThreadStartCapture::Replayable(snapshot(
            &vec![b'a'; half],
            vec![entry(
                "big",
                UntrackedKind::File,
                false,
                &vec![b'b'; half + 1],
            )],
        )),
    );

    // When: it is saved, and a start exactly at the limit is saved too.
    let rejected = storage.handle().save_thread_start_state(over);
    let at_limit = state(
        "at-limit",
        1,
        ThreadStartCapture::Replayable(snapshot(
            &vec![b'a'; half],
            vec![entry("big", UntrackedKind::File, false, &vec![b'b'; half])],
        )),
    );
    let accepted = storage.handle().save_thread_start_state(at_limit);

    // Then: only the start over the limit is refused and nothing of it is stored.
    assert!(rejected.is_err());
    assert_eq!(database.thread_start_state("over").unwrap(), None);
    assert_eq!(accepted, Ok(true));
}

#[test]
fn untracked_paths_must_stay_inside_the_repository() {
    // Given: starts with an absolute, a parent-escaping and a duplicated path.
    let (_dir, storage, database) = fixture();
    for (index, paths) in [vec!["/etc/passwd"], vec!["a/../../b"], vec!["same", "same"]]
        .into_iter()
        .enumerate()
    {
        let thread_id = format!("thread-{index}");
        let untracked = paths
            .into_iter()
            .map(|path| entry(path, UntrackedKind::File, false, b""))
            .collect();

        // When: it is saved.
        let result = storage.handle().save_thread_start_state(state(
            &thread_id,
            1,
            ThreadStartCapture::Replayable(snapshot(b"", untracked)),
        ));

        // Then: it is rejected.
        assert!(result.is_err());
        assert_eq!(database.thread_start_state(&thread_id).unwrap(), None);
    }
}

#[test]
fn list_since_returns_newest_first_without_contents() {
    // Given: three starts captured at different times.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    handle
        .save_thread_start_state(state(
            "old",
            10,
            ThreadStartCapture::Replayable(snapshot(b"", vec![])),
        ))
        .unwrap();
    handle
        .save_thread_start_state(state(
            "middle",
            20,
            ThreadStartCapture::NotReplayable(NotReplayableReason::NotGitRepository),
        ))
        .unwrap();
    handle
        .save_thread_start_state(state(
            "new",
            30,
            ThreadStartCapture::Replayable(snapshot(b"diff", vec![])),
        ))
        .unwrap();

    // When: listing starts captured since the middle one.
    let listed = database.thread_start_states_since(20).unwrap();

    // Then: the two newer starts come newest first with their replayability.
    assert_eq!(
        listed
            .iter()
            .map(|summary| (
                summary.thread_id.as_str(),
                summary.root_run_id.as_str(),
                summary.captured_at_ns,
                summary.not_replayable
            ))
            .collect::<Vec<_>>(),
        vec![
            ("new", "run-30", 30, None),
            (
                "middle",
                "run-20",
                20,
                Some(NotReplayableReason::NotGitRepository)
            ),
        ]
    );
    assert_eq!(listed[0].project_root, "/work/repo/crates/app");
}

#[test]
fn maintenance_deletes_only_states_older_than_retention_with_their_entries() {
    // Given: starts captured 15 days ago and 13 days ago, each with an untracked entry.
    let (dir, storage, database) = fixture();
    let handle = storage.handle();
    for (thread_id, captured_at_ns) in [("expired", days_ago(15)), ("kept", days_ago(13))] {
        handle
            .save_thread_start_state(state(
                thread_id,
                captured_at_ns,
                ThreadStartCapture::Replayable(snapshot(
                    b"",
                    vec![entry("a", UntrackedKind::File, false, b"x")],
                )),
            ))
            .unwrap();
    }

    // When: the periodic maintenance runs.
    handle.checkpoint_now().unwrap();

    // Then: only the expired start is gone, together with its untracked entry.
    assert_eq!(database.thread_start_state("expired").unwrap(), None);
    assert!(database.thread_start_state("kept").unwrap().is_some());
    storage.close();
    let connection = rusqlite::Connection::open(dir.path().join("thread-start.sqlite3")).unwrap();
    let orphans: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM thread_start_untracked WHERE thread_id = 'expired'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0);
}

#[test]
fn zero_retention_days_keeps_every_state() {
    // Given: retention disabled and a start captured long ago.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("thread-start.sqlite3"),
        thread_start_retention_days: 0,
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .save_thread_start_state(state(
            "ancient",
            days_ago(365),
            ThreadStartCapture::NotReplayable(NotReplayableReason::TimedOut),
        ))
        .unwrap();

    // When: the periodic maintenance runs.
    storage.handle().checkpoint_now().unwrap();

    // Then: the start is kept.
    assert!(
        Database::open(&config)
            .unwrap()
            .thread_start_state("ancient")
            .unwrap()
            .is_some()
    );
}

#[test]
fn thread_start_run_ids_stay_reserved() {
    // Given: a recorded start whose root run has no other persisted record.
    let (_dir, storage, database) = fixture();
    storage
        .handle()
        .save_thread_start_state(state(
            "thread",
            42,
            ThreadStartCapture::NotReplayable(NotReplayableReason::CaptureFailed),
        ))
        .unwrap();

    // When: reading the reserved run IDs.
    let ids = database.persisted_run_ids().unwrap();

    // Then: the start's root run is among them.
    assert_eq!(ids, vec!["run-42"]);
}
