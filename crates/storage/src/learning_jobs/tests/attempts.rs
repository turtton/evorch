use super::*;
use crate::learning_jobs::LearningAttemptChange as C;
use crate::memory::{Lesson, LessonScope, MemoryStatus};

struct Fixture {
    _dir: tempfile::TempDir,
    storage: Storage,
    config: StorageConfig,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (storage, config) = open(&dir, "attempts.db");
        Self {
            _dir: dir,
            storage,
            config,
        }
    }
    fn db(&self) -> Database {
        Database::open(&self.config).unwrap()
    }
    fn job(&self) -> LearningJob {
        self.db().learning_job("job").unwrap().unwrap()
    }
    fn admit(&self) {
        self.storage
            .handle()
            .admit_learning_job(
                job(
                    "job",
                    "root",
                    false,
                    &origin(LearningOriginKind::Core, "owner"),
                ),
                day(1),
            )
            .unwrap();
    }
    fn change(&self, change: C) -> LearningJob {
        let key = self.job().key();
        assert!(
            self.storage
                .handle()
                .write_learning_attempt(LearningAttemptWrite { key, change }, day(2),)
                .unwrap()
        );
        self.job()
    }
    fn start(&self) {
        self.change(C::Start);
        self.change(C::BindRun {
            run: format!("extract-{}", self.job().attempt),
            review: false,
        });
    }
    fn lessons(&self, count: usize) -> Vec<Lesson> {
        (1..=count)
            .map(|slot| Lesson {
                id: format!("project:job:{slot}"),
                task_id: "job".into(),
                project: "project".into(),
                content: format!("bounded actionable lesson {slot}"),
                evidence: "[\"root#1\"]".into(),
                scope: LessonScope::Project,
            })
            .collect()
    }
    fn extracted(&self, count: usize) {
        self.change(C::ExtractionReady(self.lessons(count)));
    }
    fn reviewed(&self) {
        self.change(C::BindRun {
            run: "review".into(),
            review: true,
        });
        self.change(C::ReviewReady(vec![
            LearningReviewDecision {
                status: MemoryStatus::Promoted,
                rationale: "supported".into(),
            },
            LearningReviewDecision {
                status: MemoryStatus::Rejected,
                rationale: "wrong scope".into(),
            },
        ]));
    }
    fn assert_clean(&self) {
        let db = self.db();
        assert!(db.learning_job_evidence("job").unwrap().is_none());
        assert_eq!(
            db.conn
                .query_row("SELECT count(*) FROM learning_job_staging", [], |r| r
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.conn
                .query_row("SELECT count(*) FROM learning_job_publications", [], |r| {
                    r.get::<_, u32>(0)
                })
                .unwrap(),
            1
        );
    }
}

#[test]
fn learning_publication_is_atomic_private_and_once_including_ack_readback() {
    let f = Fixture::new();
    f.admit();
    f.start();
    f.extracted(2);
    assert!(
        f.db()
            .search_memory("project", "", None)
            .unwrap()
            .is_empty()
    );
    f.reviewed();
    assert!(
        f.db()
            .search_memory("project", "", None)
            .unwrap()
            .is_empty()
    );
    let command = LearningAttemptWrite {
        key: f.job().key(),
        change: C::Publish,
    };
    f.storage
        .handle()
        .write_learning_attempt(command.clone(), day(2))
        .unwrap();
    // Simulate a lost ACK: resolve the SAME operation, never Fail or start again.
    assert!(
        f.db()
            .learning_attempt_acknowledged(&command.key, &command.change)
            .unwrap()
    );
    assert!(
        !f.storage
            .handle()
            .write_learning_attempt(command.clone(), day(2))
            .unwrap()
    );
    assert!(
        !f.storage
            .handle()
            .write_learning_attempt(
                LearningAttemptWrite {
                    key: command.key,
                    change: C::Interrupt {
                        suspension_id: "late".into()
                    }
                },
                day(2),
            )
            .unwrap()
    );
    let entries = f.db().search_memory("project", "", None).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(f.db().memory_history("project:job:1").unwrap().len(), 3);
    assert_eq!(
        f.db()
            .memory_entry("project:job:1")
            .unwrap()
            .unwrap()
            .status,
        MemoryStatus::Promoted
    );
    assert_eq!(
        f.db()
            .memory_entry("project:job:2")
            .unwrap()
            .unwrap()
            .status,
        MemoryStatus::Rejected
    );
    assert_eq!(f.job().state, LearningJobState::Finished);
    f.assert_clean();
}

#[test]
fn learning_interrupt_fences_every_late_write_and_limit_terminates_without_publication() {
    let f = Fixture::new();
    f.admit();
    let original = f.db().learning_job_evidence("job").unwrap();
    for attempt in 1..=MAX_LEARNING_ATTEMPTS {
        f.start();
        f.extracted(2);
        f.reviewed();
        let old = f.job().key();
        let id = format!("suspend-{attempt}");
        let interrupted = f.change(C::Interrupt {
            suspension_id: id.clone(),
        });
        for change in [
            C::BindRun {
                run: "late".into(),
                review: false,
            },
            C::ExtractionReady(f.lessons(2)),
            C::ReviewReady(vec![]),
            C::Publish,
            C::Fail(LearningEndReason::PipelineFailed),
            C::Abandon(LearningEndReason::OriginDied),
        ] {
            assert!(
                !f.storage
                    .handle()
                    .write_learning_attempt(
                        LearningAttemptWrite {
                            key: old.clone(),
                            change
                        },
                        day(2)
                    )
                    .unwrap()
            );
            assert_eq!(f.job(), interrupted);
            assert_eq!(f.db().learning_job_evidence("job").unwrap(), original);
        }
        let mismatch = C::Requeue {
            suspension_id: "wrong".into(),
            to: origin(LearningOriginKind::Core, "owner"),
        };
        assert!(
            !f.storage
                .handle()
                .write_learning_attempt(
                    LearningAttemptWrite {
                        key: interrupted.key(),
                        change: mismatch
                    },
                    day(2)
                )
                .unwrap()
        );
        let requeue = LearningAttemptWrite {
            key: interrupted.key(),
            change: C::Requeue {
                suspension_id: id,
                to: origin(LearningOriginKind::Core, "owner"),
            },
        };
        assert!(
            f.storage
                .handle()
                .write_learning_attempt(requeue.clone(), day(2))
                .unwrap()
        );
        assert!(
            !f.storage
                .handle()
                .write_learning_attempt(requeue, day(2))
                .unwrap()
        );
        assert_eq!(f.job().attempt, attempt);
        assert!(
            f.db()
                .search_memory("project", "", None)
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(f.job().reason.as_deref(), Some("attempt_limit"));
    assert_eq!(f.job().state, LearningJobState::Abandoned);
    assert_eq!(
        f.db()
            .conn
            .query_row("SELECT count(*) FROM learning_claims", [], |r| r
                .get::<_, u32>(0))
            .unwrap(),
        1
    );
    f.assert_clean();
}

#[test]
fn learning_noninterrupted_failure_and_crash_publish_only_complete_extractions() {
    for reason in [
        LearningEndReason::PipelineFailed,
        LearningEndReason::OriginDied,
    ] {
        for extracted in [false, true] {
            let f = Fixture::new();
            f.admit();
            f.start();
            if extracted {
                f.extracted(2);
                f.reviewed();
            }
            let key = f.job().key();
            let change = if reason == LearningEndReason::OriginDied {
                C::Abandon(reason)
            } else {
                C::Fail(reason)
            };
            let command = LearningAttemptWrite { key, change };
            assert!(
                f.storage
                    .handle()
                    .write_learning_attempt(command.clone(), day(2))
                    .unwrap()
            );
            assert!(
                !f.storage
                    .handle()
                    .write_learning_attempt(command, day(2))
                    .unwrap()
            );
            let entries = f.db().search_memory("project", "", None).unwrap();
            assert_eq!(entries.len(), if extracted { 2 } else { 0 });
            assert!(
                entries
                    .iter()
                    .all(|entry| entry.status == MemoryStatus::Candidate)
            );
            assert_eq!(f.job().reason.as_deref(), Some(reason.as_str()));
            f.assert_clean();
        }
    }
}

#[test]
fn learning_secret_guard_rolls_back_the_whole_batch_but_not_terminal_cleanup() {
    let f = Fixture::new();
    f.admit();
    f.start();
    let mut lessons = f.lessons(2);
    lessons[1].content = format!("sk-{}", "a".repeat(32));
    f.change(C::ExtractionReady(lessons));
    f.reviewed();
    let before = f.job();
    assert!(
        f.storage
            .handle()
            .write_learning_attempt(
                LearningAttemptWrite {
                    key: before.key(),
                    change: C::Publish
                },
                day(2),
            )
            .is_err()
    );
    assert_eq!(f.job(), before);
    assert!(
        f.db()
            .search_memory("project", "", None)
            .unwrap()
            .is_empty()
    );
    f.change(C::Fail(LearningEndReason::PipelineFailed));
    assert_eq!(f.job().publication.as_deref(), Some("None"));
    assert!(
        f.db()
            .search_memory("project", "", None)
            .unwrap()
            .is_empty()
    );
    f.assert_clean();
}

#[test]
fn learning_terminal_reasons_leave_no_recovery_payload_or_provider_work() {
    for reason in [
        LearningEndReason::NoResumeAuthority,
        LearningEndReason::UnconfirmedWrite,
    ] {
        let f = Fixture::new();
        f.admit();
        f.start();
        f.extracted(2);
        f.change(C::Interrupt {
            suspension_id: "planned".into(),
        });
        f.change(C::Abandon(reason));
        assert_eq!(f.job().reason.as_deref(), Some(reason.as_str()));
        assert!(
            f.db()
                .search_memory("project", "", None)
                .unwrap()
                .is_empty()
        );
        f.assert_clean();
    }
}

/// The children independently open SQLite writers; stdin/stdout are the barrier.
#[test]
fn learning_attempt_process_peer() {
    let Ok(path) = std::env::var("EVORCH_LEARNING_RACE_DB") else {
        return;
    };
    let action = std::env::var("EVORCH_LEARNING_RACE_ACTION").unwrap();
    let config = StorageConfig {
        db_path: path.into(),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let key = Database::open(&config)
        .unwrap()
        .learning_job("job")
        .unwrap()
        .unwrap()
        .key();
    println!("READY");
    use std::io::{BufRead, Write};
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
    let change = match action.as_str() {
        "publish" => C::Publish,
        "interrupt" => C::Interrupt {
            suspension_id: "race".into(),
        },
        "requeue" => C::Requeue {
            suspension_id: "race".into(),
            to: origin(LearningOriginKind::Core, "owner"),
        },
        _ => panic!("unknown race action"),
    };
    let result = storage
        .handle()
        .write_learning_attempt(LearningAttemptWrite { key, change }, day(2))
        .unwrap();
    println!("APPLIED={result}");
}

fn race(f: &Fixture, actions: [&str; 2]) -> usize {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    let mut peers: Vec<_> = actions
        .into_iter()
        .map(|action| {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "learning_jobs::tests::attempts::learning_attempt_process_peer",
                    "--nocapture",
                ])
                .env("EVORCH_LEARNING_RACE_DB", &f.config.db_path)
                .env("EVORCH_LEARNING_RACE_ACTION", action)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut output = BufReader::new(child.stdout.take().unwrap());
            loop {
                let mut line = String::new();
                assert_ne!(
                    output.read_line(&mut line).unwrap(),
                    0,
                    "peer exited before barrier"
                );
                if line.trim() == "READY" {
                    break;
                }
            }
            (child, output)
        })
        .collect();
    for (child, _) in &mut peers {
        writeln!(child.stdin.take().unwrap(), "go").unwrap();
    }
    peers
        .into_iter()
        .map(|(mut child, output)| {
            let mut result = false;
            for line in output.lines() {
                result |= line.unwrap().trim() == "APPLIED=true";
            }
            assert!(child.wait().unwrap().success());
            usize::from(result)
        })
        .sum()
}

#[test]
fn learning_two_process_publication_vs_suspend_and_duplicate_requeue_have_one_winner() {
    let f = Fixture::new();
    f.admit();
    f.start();
    f.extracted(2);
    f.reviewed();
    assert_eq!(race(&f, ["publish", "interrupt"]), 1);
    match f.job().state {
        LearningJobState::Finished => f.assert_clean(),
        LearningJobState::Interrupted => {
            assert!(
                f.db()
                    .search_memory("project", "", None)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(race(&f, ["requeue", "requeue"]), 1);
            assert_eq!(f.job().state, LearningJobState::Pending);
            assert_eq!(f.job().attempt, 1);
        }
        other => panic!("unexpected race result {other:?}"),
    }
    // Exercise duplicate requeue independently of which side won publication.
    let f = Fixture::new();
    f.admit();
    f.start();
    f.change(C::Interrupt {
        suspension_id: "race".into(),
    });
    assert_eq!(race(&f, ["requeue", "requeue"]), 1);
}

#[test]
fn learning_publication_collision_preserves_favorite_and_cleans_evidence() {
    let f = Fixture::new();
    f.admit();
    f.start();
    f.extracted(1);
    let existing = f.lessons(1).pop().unwrap();
    let writer = f.storage.handle();
    writer.append_lesson(&existing).unwrap();
    writer
        .validate_lesson(&existing.id, &existing.evidence)
        .unwrap();
    writer.promote_lesson(&existing.id).unwrap();
    writer.set_lesson_favorite(&existing.id, true).unwrap();
    let before = f.db().memory_entry(&existing.id).unwrap();
    f.change(C::BindRun {
        run: "review".into(),
        review: true,
    });
    f.change(C::ReviewReady(vec![LearningReviewDecision {
        status: MemoryStatus::Rejected,
        rationale: "do not mutate preexisting favorite".into(),
    }]));
    let key = f.job().key();
    assert!(
        writer
            .write_learning_attempt(
                LearningAttemptWrite {
                    key,
                    change: C::Publish
                },
                day(2)
            )
            .is_err()
    );
    f.change(C::Fail(LearningEndReason::PipelineFailed));
    assert_eq!(f.db().memory_entry(&existing.id).unwrap(), before);
    f.assert_clean();
    assert_eq!(f.job().publication.as_deref(), Some("None"));
}
