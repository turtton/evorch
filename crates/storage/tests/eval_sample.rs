//! Evaluation samples through the single-writer boundary (ADR 0036 §1).

use std::time::{Duration, SystemTime};

use storage::eval_sample::EvalSample;
use storage::thread_start::{
    NotReplayableReason, ThreadStartCapture, ThreadStartSnapshot, ThreadStartState,
};
use storage::{Database, Storage, StorageConfig, system_time_to_ns};

const SECRET: &str = "sk-test-evorch-9f8e7d6c5b4a3f2e1d";

fn fixture() -> (tempfile::TempDir, Storage, Database) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("eval-sample.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let database = Database::open(&config).unwrap();
    (dir, storage, database)
}

fn replayable() -> ThreadStartCapture {
    ThreadStartCapture::Replayable(ThreadStartSnapshot {
        repo_prefix: String::new(),
        head_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        head_ref: Some("refs/heads/main".into()),
        diff: Vec::new(),
        untracked: Vec::new(),
    })
}

fn start(thread_id: &str, captured_at_ns: i64, capture: ThreadStartCapture) -> ThreadStartState {
    ThreadStartState {
        thread_id: thread_id.into(),
        root_run_id: format!("run-{thread_id}"),
        project_root: "/work/repo".into(),
        captured_at_ns,
        capture,
    }
}

fn sample(thread_id: &str, problem: &str, focus_roles: &[&str], at_ns: i64) -> EvalSample {
    EvalSample {
        thread_id: thread_id.into(),
        problem: problem.into(),
        focus_roles: focus_roles.iter().map(|role| (*role).to_owned()).collect(),
        created_at_ns: at_ns,
        updated_at_ns: at_ns,
    }
}

fn days_ago(days: u64) -> i64 {
    system_time_to_ns(SystemTime::now() - Duration::from_secs(days * 86_400)).unwrap()
}

#[test]
fn saving_again_replaces_the_problem_and_focus_but_keeps_the_creation_time() {
    // Given: a replayable thread pinned as a sample.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    handle
        .save_thread_start_state(start("thread", 1, replayable()))
        .unwrap();
    handle
        .save_eval_sample(sample("thread", "loops on tests", &["worker"], 10))
        .unwrap();

    // When: the sample is saved again with a new problem and focus.
    handle
        .save_eval_sample(sample(
            "thread",
            "skips review",
            &["roles.reviewer", "worker"],
            20,
        ))
        .unwrap();

    // Then: one sample holds the new problem and focus and the first creation time.
    let expected = EvalSample {
        thread_id: "thread".into(),
        problem: "skips review".into(),
        focus_roles: vec!["roles.reviewer".into(), "worker".into()],
        created_at_ns: 10,
        updated_at_ns: 20,
    };
    assert_eq!(
        database.eval_sample("thread").unwrap(),
        Some(expected.clone())
    );
    assert_eq!(database.eval_samples().unwrap(), vec![expected]);
}

#[test]
fn samples_require_a_replayable_start_state() {
    // Given: a thread that could not be captured and a thread with no start at all.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    handle
        .save_thread_start_state(start(
            "dirty",
            1,
            ThreadStartCapture::NotReplayable(NotReplayableReason::TooLarge),
        ))
        .unwrap();

    // When: either is pinned as a sample.
    let not_replayable = handle.save_eval_sample(sample("dirty", "slow", &[], 1));
    let missing = handle.save_eval_sample(sample("unknown", "slow", &[], 1));

    // Then: both are rejected and nothing is stored.
    assert!(not_replayable.is_err());
    assert!(missing.is_err());
    assert!(database.eval_samples().unwrap().is_empty());
}

#[test]
fn empty_or_secret_bearing_problems_are_rejected() {
    // Given: a replayable thread.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    handle
        .save_thread_start_state(start("thread", 1, replayable()))
        .unwrap();

    // When: it is pinned with a blank problem or one carrying a secret.
    let blank = handle.save_eval_sample(sample("thread", "  ", &[], 1));
    let secret = handle.save_eval_sample(sample("thread", &format!("leaks {SECRET}"), &[], 1));

    // Then: both are rejected.
    assert!(blank.is_err());
    assert!(secret.is_err());
    assert_eq!(database.eval_sample("thread").unwrap(), None);
}

#[test]
fn deleting_reports_whether_a_sample_existed() {
    // Given: a pinned sample.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    handle
        .save_thread_start_state(start("thread", 1, replayable()))
        .unwrap();
    handle
        .save_eval_sample(sample("thread", "slow", &[], 1))
        .unwrap();

    // When: it is deleted twice.
    let first = handle.delete_eval_sample("thread").unwrap();
    let second = handle.delete_eval_sample("thread").unwrap();

    // Then: only the first deletion found it, and the start state remains.
    assert!(first);
    assert!(!second);
    assert_eq!(database.eval_sample("thread").unwrap(), None);
    assert!(database.thread_start_state("thread").unwrap().is_some());
}

#[test]
fn retention_keeps_start_states_pinned_by_a_sample() {
    // Given: two starts past retention, one of them pinned as a sample.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    for thread_id in ["pinned", "unpinned"] {
        handle
            .save_thread_start_state(start(thread_id, days_ago(30), replayable()))
            .unwrap();
    }
    handle
        .save_eval_sample(sample("pinned", "slow", &["worker"], 1))
        .unwrap();

    // When: the periodic maintenance runs.
    handle.checkpoint_now().unwrap();

    // Then: only the unpinned start expires.
    assert!(database.thread_start_state("pinned").unwrap().is_some());
    assert_eq!(database.thread_start_state("unpinned").unwrap(), None);

    // When: the sample is removed and maintenance runs again.
    handle.delete_eval_sample("pinned").unwrap();
    handle.checkpoint_now().unwrap();

    // Then: the start expires as well.
    assert_eq!(database.thread_start_state("pinned").unwrap(), None);
}

#[test]
fn schema_keeps_a_pinned_start_state_from_being_deleted() {
    // Given: a pinned sample.
    let (dir, storage, _database) = fixture();
    let handle = storage.handle();
    handle
        .save_thread_start_state(start("thread", 1, replayable()))
        .unwrap();
    handle
        .save_eval_sample(sample("thread", "slow", &[], 1))
        .unwrap();
    storage.close();
    let connection = rusqlite::Connection::open(dir.path().join("eval-sample.sqlite3")).unwrap();
    connection.pragma_update(None, "foreign_keys", 1).unwrap();

    // When: raw SQL deletes its start state or stores focus roles that are not a JSON array.
    let delete = connection.execute(
        "DELETE FROM thread_start_states WHERE thread_id = 'thread'",
        [],
    );
    let focus = connection.execute(
        "UPDATE eval_samples SET focus_roles = '\"worker\"' WHERE thread_id = 'thread'",
        [],
    );

    // Then: both are rejected.
    assert!(delete.is_err());
    assert!(focus.is_err());
}
