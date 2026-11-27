//! Evaluation samples through the single-writer boundary (ADR 0036 §1).

use std::time::{Duration, SystemTime};

use storage::eval_sample::{
    CHECK_VERSION, CallRange, Check, CheckScope, EvalSample, MAX_CHECKS, SampleKind, SourceRef,
};
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
    EvalSample::new(
        thread_id.into(),
        problem.into(),
        focus_roles.iter().map(|role| (*role).to_owned()).collect(),
        at_ns,
    )
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
        updated_at_ns: 20,
        ..EvalSample::new(
            "thread".into(),
            "skips review".into(),
            vec!["roles.reviewer".into(), "worker".into()],
            10,
        )
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

fn checked(checks: Vec<Check>) -> EvalSample {
    EvalSample {
        kind: SampleKind::Regression,
        source_refs: vec![
            SourceRef::Run {
                run_id: "run-thread".into(),
            },
            SourceRef::Event { event_id: 7 },
        ],
        tags: vec!["tool-misuse".into()],
        checks,
        ..sample("thread", "reads the same file again", &["worker"], 1)
    }
}

fn read_checks() -> Vec<Check> {
    vec![
        Check::SuccessfulCalls {
            tool: "read".into(),
            min: Some(1),
            max: Some(3),
            scope: CheckScope::Roles(vec!["worker".into()]),
            calls: CallRange::IncludingChildren,
        },
        Check::ToolErrors {
            tool: "shell".into(),
            max: 0,
            scope: CheckScope::Tree,
            calls: CallRange::Direct,
        },
        Check::Diagnostics {
            code: "NoProgress".into(),
            max: 0,
            scope: CheckScope::Tree,
        },
    ]
}

#[test]
fn kind_references_tags_and_checks_round_trip() {
    // Given: a replayable thread.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    handle
        .save_thread_start_state(start("thread", 1, replayable()))
        .unwrap();

    // When: a sample with all of the new fields is saved.
    let saved = checked(read_checks());
    handle.save_eval_sample(saved.clone()).unwrap();

    // Then: it reads back unchanged, and its content hash ignores timestamps.
    let loaded = database.eval_sample("thread").unwrap().unwrap();
    assert_eq!(loaded, saved);
    assert_eq!(loaded.check_version, CHECK_VERSION);
    let later = EvalSample {
        updated_at_ns: 99,
        ..saved.clone()
    };
    assert_eq!(loaded.content_hash(), later.content_hash());
    let other = EvalSample {
        tags: vec!["other".into()],
        ..saved
    };
    assert_ne!(loaded.content_hash(), other.content_hash());
}

#[test]
fn samples_without_the_new_fields_read_back_as_failures_without_checks() {
    // Given: a row saved by an older build, with no spec.
    let (dir, storage, database) = fixture();
    let handle = storage.handle();
    handle
        .save_thread_start_state(start("thread", 1, replayable()))
        .unwrap();
    handle
        .save_eval_sample(sample("thread", "slow", &[], 1))
        .unwrap();
    storage.close();
    let connection = rusqlite::Connection::open(dir.path().join("eval-sample.sqlite3")).unwrap();
    connection
        .execute("UPDATE eval_samples SET spec = '{}'", [])
        .unwrap();

    // When: it is read.
    let loaded = database.eval_sample("thread").unwrap().unwrap();

    // Then: it is a failure sample with no checks at the current version.
    assert_eq!(loaded.kind, SampleKind::Failure);
    assert!(loaded.checks.is_empty());
    assert_eq!(loaded.check_version, CHECK_VERSION);
}

#[test]
fn invalid_specs_are_rejected_and_nothing_is_stored() {
    // Given: a replayable thread.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    handle
        .save_thread_start_state(start("thread", 1, replayable()))
        .unwrap();
    let tool_errors = |tool: &str, scope| Check::ToolErrors {
        tool: tool.into(),
        max: 0,
        scope,
        calls: CallRange::Direct,
    };
    let successes = |min, max| Check::SuccessfulCalls {
        tool: "read".into(),
        min,
        max,
        scope: CheckScope::Tree,
        calls: CallRange::Direct,
    };
    let rejected = [
        // too many checks: one over the limit
        checked(vec![tool_errors("read", CheckScope::Tree); MAX_CHECKS + 1]),
        // a success check with no bound, a zero minimum and an inverted range
        checked(vec![successes(None, None)]),
        checked(vec![successes(Some(0), None)]),
        checked(vec![successes(Some(3), Some(2))]),
        // empty names and roles, and a diagnostic code no producer raises
        checked(vec![tool_errors(" ", CheckScope::Tree)]),
        checked(vec![tool_errors("read", CheckScope::Roles(Vec::new()))]),
        checked(vec![Check::Diagnostics {
            code: "NoSuchCode".into(),
            max: 0,
            scope: CheckScope::Tree,
        }]),
        // a secret in a tag, a source reference or a check
        EvalSample {
            tags: vec![SECRET.into()],
            ..checked(Vec::new())
        },
        EvalSample {
            source_refs: vec![SourceRef::Improvement {
                candidate_id: SECRET.into(),
            }],
            ..checked(Vec::new())
        },
        checked(vec![tool_errors(SECRET, CheckScope::Tree)]),
        // an empty tag, an event id that cannot exist and an unknown check version
        EvalSample {
            tags: vec![String::new()],
            ..checked(Vec::new())
        },
        EvalSample {
            source_refs: vec![SourceRef::Event { event_id: 0 }],
            ..checked(Vec::new())
        },
        EvalSample {
            check_version: CHECK_VERSION + 1,
            ..checked(Vec::new())
        },
        // metadata over 64 KiB, which is refused instead of cut
        EvalSample {
            tags: (0..70)
                .map(|index| format!("{index}{}", "x".repeat(1_000)))
                .collect(),
            ..checked(Vec::new())
        },
    ];

    // When: each is saved.
    let results: Vec<bool> = rejected
        .into_iter()
        .map(|sample| handle.save_eval_sample(sample).is_err())
        .collect();

    // Then: every one is refused and no sample exists.
    assert!(results.iter().all(|refused| *refused), "{results:?}");
    assert_eq!(database.eval_sample("thread").unwrap(), None);
}

#[test]
fn the_check_and_size_limits_are_inclusive() {
    // Given: a replayable thread.
    let (_dir, storage, database) = fixture();
    let handle = storage.handle();
    handle
        .save_thread_start_state(start("thread", 1, replayable()))
        .unwrap();
    let check = Check::ToolErrors {
        tool: "read".into(),
        max: 0,
        scope: CheckScope::Tree,
        calls: CallRange::Direct,
    };

    // When: a sample with exactly the most checks is saved.
    handle
        .save_eval_sample(checked(vec![check; MAX_CHECKS]))
        .unwrap();

    // Then: it is stored.
    assert_eq!(
        database
            .eval_sample("thread")
            .unwrap()
            .unwrap()
            .checks
            .len(),
        MAX_CHECKS
    );
}
