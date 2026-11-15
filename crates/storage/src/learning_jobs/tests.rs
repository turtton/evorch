use super::*;
mod attempts;
use crate::{Database, Storage, StorageConfig};
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};

const DAY: Duration = Duration::from_secs(86_400);

fn origin(kind: LearningOriginKind, instance: &str) -> LearningOrigin {
    LearningOrigin {
        kind,
        instance: instance.into(),
        pid: 4242,
        start_ticks: Some(77),
        boot_id: Some("boot".into()),
        pid_ns: Some("pid:[1]".into()),
        config_identity: (kind == LearningOriginKind::Core).then(|| "cfg".into()),
    }
}

fn job(task: &str, root: &str, goal: bool, owner: &LearningOrigin) -> NewLearningJob {
    NewLearningJob {
        task_id: task.into(),
        roots: vec![root.into()],
        goal,
        daily_limit: 10,
        origin: owner.clone(),
        project: "project".into(),
        source_run_id: root.into(),
        prompt: format!("prompt for {task}"),
        evidence_json: format!("{{\"task\":\"{task}\"}}"),
    }
}

fn open(dir: &tempfile::TempDir, name: &str) -> (Storage, StorageConfig) {
    let config = StorageConfig {
        db_path: dir.path().join(name),
        ..Default::default()
    };
    (Storage::open(config.clone()).unwrap(), config)
}

fn day(n: u32) -> SystemTime {
    SystemTime::UNIX_EPOCH + DAY * n + Duration::from_secs(60)
}

#[test]
fn admission_stores_claim_roots_job_and_evidence_together_and_spends_the_quota_once() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, config) = open(&dir, "admit.db");
    let writer = storage.handle();
    let owner = origin(LearningOriginKind::Core, "a");
    let mut first = job("t1", "run-1", false, &owner);
    first.daily_limit = 1;
    assert_eq!(
        writer.admit_learning_job(first.clone(), day(5)).unwrap(),
        LearningAdmission::Admitted
    );
    let db = Database::open(&config).unwrap();
    let stored = db.learning_job("t1").unwrap().unwrap();
    assert_eq!(stored.state, LearningJobState::Pending);
    assert_eq!(stored.origin, owner);
    assert_eq!(
        stored.admitted_at_ns,
        crate::system_time_to_ns(day(5)).unwrap()
    );
    let evidence = db.learning_job_evidence("t1").unwrap().unwrap();
    assert_eq!(evidence.prompt, first.prompt);
    assert_eq!(evidence.evidence_json, first.evidence_json);
    let claimed: u32 = db
        .conn
        .query_row(
            "SELECT count(*) FROM learning_claims WHERE utc_day=5",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(claimed, 1);

    // A repeat of the task or of its root, and a spent quota, admit nothing.
    assert_eq!(
        writer.admit_learning_job(first, day(5)).unwrap(),
        LearningAdmission::Duplicate
    );
    let mut same_root = job("t2", "run-1", false, &owner);
    same_root.daily_limit = 1;
    assert_eq!(
        writer.admit_learning_job(same_root, day(5)).unwrap(),
        LearningAdmission::Duplicate
    );
    let mut over = job("t3", "run-3", false, &owner);
    over.daily_limit = 1;
    assert_eq!(
        writer.admit_learning_job(over.clone(), day(5)).unwrap(),
        LearningAdmission::QuotaExhausted
    );
    assert!(db.learning_job("t3").unwrap().is_none());
    // The quota is per UTC day, and the refused attempt kept no claim.
    assert_eq!(
        writer.admit_learning_job(over, day(6)).unwrap(),
        LearningAdmission::Admitted
    );
    // Starting and finishing never spend another slot.
    assert!(start_learning_job(&writer, &config, "t1", "a", day(5)).unwrap());
    assert!(finish_learning_job(&writer, &config, "t1", "a", true, day(5)).unwrap());
    let claimed: u32 = db
        .conn
        .query_row(
            "SELECT count(*) FROM learning_claims WHERE utc_day=5",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(claimed, 1);
}

#[test]
fn a_failure_while_storing_the_job_leaves_no_claim_behind() {
    let db = Database::open_in_memory().unwrap();
    db.conn
        .execute_batch(
            "CREATE TRIGGER refuse_evidence BEFORE INSERT ON learning_job_evidence
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    let owner = origin(LearningOriginKind::Gui, "g");
    let result = write(
        &db.conn,
        Write::Admit(Box::new(job("t1", "run-1", false, &owner)), 5),
    );
    assert!(result.is_err());
    for table in ["learning_claims", "learning_claim_roots", "learning_jobs"] {
        let rows: u32 = db
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "{table} must roll back with the admission");
    }
}

#[test]
fn a_job_starts_once_for_its_owner_and_ending_it_deletes_the_evidence_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, config) = open(&dir, "cas.db");
    let writer = storage.handle();
    let owner = origin(LearningOriginKind::Core, "a");
    writer
        .admit_learning_job(job("t1", "run-1", false, &owner), day(1))
        .unwrap();
    assert!(!start_learning_job(&writer, &config, "t1", "stranger", day(1)).unwrap());
    assert!(!finish_learning_job(&writer, &config, "t1", "a", true, day(1)).unwrap());
    assert!(start_learning_job(&writer, &config, "t1", "a", day(1)).unwrap());
    assert!(!start_learning_job(&writer, &config, "t1", "a", day(1)).unwrap());
    let db = Database::open(&config).unwrap();
    assert!(db.learning_job_evidence("t1").unwrap().is_some());
    assert!(!finish_learning_job(&writer, &config, "t1", "stranger", true, day(1)).unwrap());
    assert!(finish_learning_job(&writer, &config, "t1", "a", false, day(2)).unwrap());
    let ended = db.learning_job("t1").unwrap().unwrap();
    assert_eq!(ended.state, LearningJobState::Failed);
    assert_eq!(
        ended.finished_at_ns,
        Some(crate::system_time_to_ns(day(2)).unwrap())
    );
    assert!(db.learning_job_evidence("t1").unwrap().is_none());
    // An ended job is never started or ended again.
    assert!(!start_learning_job(&writer, &config, "t1", "a", day(2)).unwrap());
    assert!(!abandon_learning_job(&writer, &config, "t1", "a", "late", day(2)).unwrap());
    assert!(
        db.conn
            .execute(
                "UPDATE learning_jobs SET state='Pending' WHERE task_id='t1'",
                []
            )
            .is_err(),
        "the schema itself refuses to move an ended job"
    );
}

#[test]
fn evidence_cannot_change_or_disappear_while_its_job_is_live() {
    let db = Database::open_in_memory().unwrap();
    let owner = origin(LearningOriginKind::Core, "a");
    write(
        &db.conn,
        Write::Admit(Box::new(job("t1", "run-1", false, &owner)), 5),
    )
    .unwrap();
    assert!(
        db.conn
            .execute("UPDATE learning_job_evidence SET prompt='changed'", [])
            .is_err()
    );
    assert!(
        db.conn
            .execute("DELETE FROM learning_job_evidence", [])
            .is_err()
    );
    write(
        &db.conn,
        Write::Attempt(
            Box::new(LearningAttemptWrite {
                key: db.learning_job("t1").unwrap().unwrap().key(),
                change: LearningAttemptChange::Start,
            }),
            6,
        ),
    )
    .unwrap();
    assert!(
        db.conn
            .execute("DELETE FROM learning_job_evidence", [])
            .is_err()
    );
    assert!(
        db.conn
            .execute(
                "UPDATE learning_jobs SET origin_instance='b' WHERE task_id='t1'",
                []
            )
            .is_err(),
        "a started job keeps its origin"
    );
    assert!(db.learning_job_evidence("t1").unwrap().is_some());
}

#[test]
fn tampered_evidence_is_reported_instead_of_being_replayed() {
    let db = Database::open_in_memory().unwrap();
    let owner = origin(LearningOriginKind::Core, "a");
    write(
        &db.conn,
        Write::Admit(Box::new(job("t1", "run-1", false, &owner)), 5),
    )
    .unwrap();
    db.conn
        .execute_batch(
            "DROP TRIGGER learning_job_evidence_immutable;
             UPDATE learning_job_evidence SET evidence_json='{\"forged\":true}';",
        )
        .unwrap();
    assert!(db.learning_job_evidence("t1").is_err());
}

#[test]
fn abandoning_ends_only_the_job_of_the_named_instance_and_removes_its_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, config) = open(&dir, "abandon.db");
    let writer = storage.handle();
    let owner = origin(LearningOriginKind::Gui, "g");
    for (task, root) in [("pending", "run-1"), ("started", "run-2")] {
        writer
            .admit_learning_job(job(task, root, false, &owner), day(1))
            .unwrap();
    }
    start_learning_job(&writer, &config, "started", "g", day(1)).unwrap();
    for task in ["pending", "started"] {
        assert!(!abandon_learning_job(&writer, &config, task, "other", "dead", day(2)).unwrap());
        assert!(abandon_learning_job(&writer, &config, task, "g", "origin died", day(2)).unwrap());
    }
    let db = Database::open(&config).unwrap();
    for task in ["pending", "started"] {
        let job = db.learning_job(task).unwrap().unwrap();
        assert_eq!(job.state, LearningJobState::Abandoned);
        assert_eq!(job.reason.as_deref(), Some("origin_died"));
        assert!(db.learning_job_evidence(task).unwrap().is_none());
    }
    assert!(
        db.learning_jobs(&[LearningJobState::Pending, LearningJobState::Started])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn only_one_of_several_racing_cores_adopts_a_pending_job() {
    let dir = tempfile::tempdir().unwrap();
    let (first, config) = open(&dir, "adopt.db");
    let dead = origin(LearningOriginKind::Core, "dead");
    first
        .handle()
        .admit_learning_job(job("t1", "run-1", false, &dead), day(1))
        .unwrap();
    let revision = Database::open(&config)
        .unwrap()
        .learning_job("t1")
        .unwrap()
        .unwrap()
        .revision;
    let stores: Vec<_> = (0..4)
        .map(|_| Storage::open(config.clone()).unwrap())
        .collect();
    let barrier = Arc::new(Barrier::new(stores.len()));
    let winners: Vec<usize> = std::thread::scope(|scope| {
        let joins: Vec<_> = stores
            .iter()
            .enumerate()
            .map(|(index, store)| {
                let writer = store.handle();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    let to = origin(LearningOriginKind::Core, &format!("successor-{index}"));
                    writer
                        .adopt_learning_job("t1", "dead", revision, to)
                        .unwrap()
                        .then_some(index)
                })
            })
            .collect();
        joins
            .into_iter()
            .filter_map(|j| j.join().unwrap())
            .collect()
    });
    assert_eq!(winners.len(), 1);
    let adopted = Database::open(&config)
        .unwrap()
        .learning_job("t1")
        .unwrap()
        .unwrap();
    assert_eq!(adopted.origin.instance, format!("successor-{}", winners[0]));
    assert_eq!(adopted.state, LearningJobState::Pending);
}

#[test]
fn a_gui_job_or_a_started_job_cannot_be_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, config) = open(&dir, "no-adopt.db");
    let writer = storage.handle();
    let gui = origin(LearningOriginKind::Gui, "g");
    let core = origin(LearningOriginKind::Core, "c");
    writer
        .admit_learning_job(job("gui", "run-1", false, &gui), day(1))
        .unwrap();
    writer
        .admit_learning_job(job("core", "run-2", false, &core), day(1))
        .unwrap();
    start_learning_job(&writer, &config, "core", "c", day(1)).unwrap();
    let db = Database::open(&config).unwrap();
    for (task, from) in [("gui", "g"), ("core", "c")] {
        let revision = db.learning_job(task).unwrap().unwrap().revision;
        let to = origin(LearningOriginKind::Core, "thief");
        assert!(!writer.adopt_learning_job(task, from, revision, to).unwrap());
    }
}

#[test]
fn concurrent_writers_admit_one_job_per_task() {
    let dir = tempfile::tempdir().unwrap();
    let (first, config) = open(&dir, "race.db");
    let stores: Vec<_> = std::iter::once(first)
        .chain((0..3).map(|_| Storage::open(config.clone()).unwrap()))
        .collect();
    let barrier = Arc::new(Barrier::new(stores.len()));
    let admitted: usize = std::thread::scope(|scope| {
        let joins: Vec<_> = stores
            .iter()
            .enumerate()
            .map(|(index, store)| {
                let writer = store.handle();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    let owner = origin(LearningOriginKind::Core, &format!("p{index}"));
                    usize::from(
                        writer
                            .admit_learning_job(job("same", "run-1", false, &owner), day(1))
                            .unwrap()
                            == LearningAdmission::Admitted,
                    )
                })
            })
            .collect();
        joins.into_iter().map(|j| j.join().unwrap()).sum()
    });
    assert_eq!(admitted, 1);
    let jobs = Database::open(&config)
        .unwrap()
        .learning_jobs(&LearningJobState::ALL)
        .unwrap();
    assert_eq!(jobs.len(), 1);
}

// Test adapters retain the old queue tests' owner checks while all writes now use CAS keys.
fn start_learning_job(
    writer: &crate::StorageHandle,
    config: &StorageConfig,
    task: &str,
    owner: &str,
    at: SystemTime,
) -> Result<bool, StorageError> {
    let mut key = Database::open(config)?.learning_job(task)?.unwrap().key();
    key.owner = owner.into();
    writer.write_learning_attempt(
        LearningAttemptWrite {
            key,
            change: LearningAttemptChange::Start,
        },
        at,
    )
}
fn finish_learning_job(
    writer: &crate::StorageHandle,
    config: &StorageConfig,
    task: &str,
    owner: &str,
    ok: bool,
    at: SystemTime,
) -> Result<bool, StorageError> {
    let db = Database::open(config)?;
    let mut key = db.learning_job(task)?.unwrap().key();
    key.owner = owner.into();
    if key.state != LearningJobState::Started || !ok {
        return writer.write_learning_attempt(
            LearningAttemptWrite {
                key,
                change: LearningAttemptChange::Fail(LearningEndReason::PipelineFailed),
            },
            at,
        );
    }
    if !writer.write_learning_attempt(
        LearningAttemptWrite {
            key,
            change: LearningAttemptChange::BindRun {
                run: "extraction".into(),
                review: false,
            },
        },
        at,
    )? {
        return Ok(false);
    }
    let key = db.learning_job(task)?.unwrap().key();
    writer.write_learning_attempt(
        LearningAttemptWrite {
            key,
            change: LearningAttemptChange::ExtractionReady(vec![]),
        },
        at,
    )?;
    let key = db.learning_job(task)?.unwrap().key();
    writer.write_learning_attempt(
        LearningAttemptWrite {
            key,
            change: LearningAttemptChange::Publish,
        },
        at,
    )
}
fn abandon_learning_job(
    writer: &crate::StorageHandle,
    config: &StorageConfig,
    task: &str,
    owner: &str,
    _reason: &str,
    at: SystemTime,
) -> Result<bool, StorageError> {
    let mut key = Database::open(config)?.learning_job(task)?.unwrap().key();
    key.owner = owner.into();
    writer.write_learning_attempt(
        LearningAttemptWrite {
            key,
            change: LearningAttemptChange::Abandon(LearningEndReason::OriginDied),
        },
        at,
    )
}
