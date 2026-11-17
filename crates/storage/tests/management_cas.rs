//! Conditional updates of the management area (`StorageHandle::update_memory` and
//! `update_improvement`): the revision check, the state check and the write are one
//! transaction on the single writer.
//!
//! Waiting is by `Barrier` and by the writer's own replies; nothing here sleeps or
//! carries a time limit (the runner's slow-timeout catches a hang).

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use rusqlite::Connection;
use storage::improvement::{
    ImprovementCandidate, ImprovementRecordOutcome, ImprovementSeverity, ImprovementSource,
    ImprovementStatus, ImprovementWritePolicy, NewImprovementCandidate,
};
use storage::management::{
    CasOutcome, CasRefusal, Denial, ImprovementOp, ImprovementUpdate, MemoryOp, improvement_denial,
    improvement_revision, memory_denial,
};
use storage::memory::{Lesson, LessonScope, MemoryEntry, MemoryStatus};
use storage::{Database, Storage, StorageConfig, StorageError, StorageHandle};

struct Fixture {
    _dir: tempfile::TempDir,
    _storage: Storage,
    handle: StorageHandle,
    db_path: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("management.db");
        let storage = Storage::open(StorageConfig {
            db_path: db_path.clone(),
            ..StorageConfig::default()
        })
        .unwrap();
        let handle = storage.handle();
        Self {
            _dir: dir,
            _storage: storage,
            handle,
            db_path,
        }
    }

    fn reader(&self) -> Database {
        Database::open_read_only(&self.db_path).unwrap()
    }

    /// A second connection to the same file, for what the writer cannot be told to do.
    fn side_connection(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn lesson(&self, id: &str) -> Lesson {
        let lesson = Lesson {
            id: id.into(),
            project: "p".into(),
            task_id: "task".into(),
            content: format!("content of {id}"),
            evidence: format!("evidence of {id}"),
            scope: LessonScope::Project,
        };
        self.handle.append_lesson(&lesson).unwrap();
        lesson
    }

    fn memory_revision(&self, id: &str) -> u64 {
        self.reader().memory_revision(id).unwrap()
    }

    fn memory_entry(&self, id: &str) -> MemoryEntry {
        self.reader().memory_entry(id).unwrap().unwrap()
    }

    fn candidate(&self, id: &str) -> NewImprovementCandidate {
        NewImprovementCandidate {
            id: id.into(),
            source: ImprovementSource::Diagnostic,
            code: "storage.busy".into(),
            severity: ImprovementSeverity::Warning,
            title: "Reduce writer contention".into(),
            evidence: r#"{"occurrences":3}"#.into(),
            dedup_key: format!("key-{id}"),
            run_id: Some("run-1".into()),
        }
    }

    fn record(
        &self,
        new: NewImprovementCandidate,
        max_candidates: u32,
    ) -> ImprovementRecordOutcome {
        self.handle
            .record_improvement_candidate(
                "p",
                new,
                ImprovementWritePolicy {
                    duplicate_cooldown: Duration::from_secs(3600),
                    daily_limit: 1000,
                    run_error_daily_limit: 1000,
                    max_candidates,
                },
            )
            .unwrap()
    }

    fn improvement(&self, id: &str) -> ImprovementCandidate {
        self.reader().improvement_candidate(id).unwrap().unwrap()
    }

    fn improvement_revision(&self, id: &str) -> u64 {
        improvement_revision(&self.improvement(id))
    }
}

fn refused(outcome: CasOutcome) -> CasRefusal {
    match outcome {
        CasOutcome::Refused(refusal) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn applied_revision(outcome: CasOutcome) -> u64 {
    match outcome {
        CasOutcome::Applied { revision } => revision,
        other => panic!("expected an applied update, got {other:?}"),
    }
}

/// Runs both closures at the same instant (a barrier, not a delay) and returns their
/// results; the writer decides the order.
fn race<A: Send + 'static, B: Send + 'static>(
    first: impl FnOnce() -> A + Send + 'static,
    second: impl FnOnce() -> B + Send + 'static,
) -> (A, B) {
    let gate = Arc::new(Barrier::new(2));
    let (a_gate, b_gate) = (gate.clone(), gate);
    let a = thread::spawn(move || {
        a_gate.wait();
        first()
    });
    let b = thread::spawn(move || {
        b_gate.wait();
        second()
    });
    (a.join().unwrap(), b.join().unwrap())
}

#[test]
fn two_updates_on_one_revision_let_exactly_one_win() {
    let fixture = Fixture::new();
    for round in 0..20 {
        let id = format!("race-{round}");
        fixture.lesson(&id);
        let revision = fixture.memory_revision(&id);
        let (validate, reject) = race(
            {
                let (handle, id) = (fixture.handle.clone(), id.clone());
                move || {
                    handle
                        .update_memory(&id, revision, MemoryOp::Validate)
                        .unwrap()
                }
            },
            {
                let (handle, id) = (fixture.handle.clone(), id.clone());
                move || {
                    handle
                        .update_memory(&id, revision, MemoryOp::Reject)
                        .unwrap()
                }
            },
        );
        let outcomes = [validate, reject];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| outcome.is_applied())
                .count(),
            1,
            "round {round}: {outcomes:?}"
        );
        let loser = outcomes
            .iter()
            .find(|outcome| !outcome.is_applied())
            .unwrap();
        assert_eq!(
            *loser,
            CasOutcome::Refused(CasRefusal::StaleRevision {
                current: revision + 1
            })
        );
        assert_eq!(fixture.memory_revision(&id), revision + 1);
        // The winner's result is what the lesson is: nothing was overwritten.
        let status = fixture.memory_entry(&id).status;
        let expected = if validate.is_applied() {
            MemoryStatus::Validated
        } else {
            MemoryStatus::Rejected
        };
        assert_eq!(status, expected, "round {round}");
    }
}

#[test]
fn two_improvement_updates_on_one_revision_let_exactly_one_win() {
    let fixture = Fixture::new();
    for round in 0..20 {
        let id = format!("race-{round}");
        assert_eq!(
            fixture.record(fixture.candidate(&id), 1000),
            ImprovementRecordOutcome::Stored { id: id.clone() }
        );
        let revision = fixture.improvement_revision(&id);
        let (reviewed, dismissed) = race(
            {
                let (handle, id) = (fixture.handle.clone(), id.clone());
                move || {
                    handle
                        .update_improvement(&id, revision, ImprovementUpdate::MarkReviewed)
                        .unwrap()
                }
            },
            {
                let (handle, id) = (fixture.handle.clone(), id.clone());
                move || {
                    handle
                        .update_improvement(&id, revision, ImprovementUpdate::Dismiss)
                        .unwrap()
                }
            },
        );
        assert_eq!(
            [reviewed, dismissed]
                .iter()
                .filter(|outcome| outcome.is_applied())
                .count(),
            1,
            "round {round}: {reviewed:?} / {dismissed:?}"
        );
        let winner_status = if reviewed.is_applied() {
            ImprovementStatus::Reviewed
        } else {
            ImprovementStatus::Dismissed
        };
        assert_eq!(fixture.improvement(&id).status, winner_status);
    }
}

#[test]
fn a_favorite_cannot_be_rejected_and_a_stale_reject_is_stale_first() {
    let fixture = Fixture::new();
    fixture.lesson("fav");
    let initial = fixture.memory_revision("fav");
    let favorited = applied_revision(
        fixture
            .handle
            .update_memory("fav", initial, MemoryOp::SetFavorite)
            .unwrap(),
    );
    assert_eq!(favorited, initial + 1);
    assert_eq!(favorited, fixture.memory_revision("fav"));

    // Fresh revision: the state forbids it, and nothing is written.
    assert_eq!(
        refused(
            fixture
                .handle
                .update_memory("fav", favorited, MemoryOp::Reject)
                .unwrap()
        ),
        CasRefusal::Denied(Denial::FavoriteMustBeCleared)
    );
    assert_eq!(fixture.memory_revision("fav"), favorited);
    assert_eq!(fixture.memory_entry("fav").status, MemoryStatus::Candidate);

    // The revision from before the favorite is stale; staleness is judged first.
    assert_eq!(
        refused(
            fixture
                .handle
                .update_memory("fav", initial, MemoryOp::Reject)
                .unwrap()
        ),
        CasRefusal::StaleRevision { current: favorited }
    );

    // Clearing the favorite makes the reject allowed again.
    let cleared = applied_revision(
        fixture
            .handle
            .update_memory("fav", favorited, MemoryOp::ClearFavorite)
            .unwrap(),
    );
    applied_revision(
        fixture
            .handle
            .update_memory("fav", cleared, MemoryOp::Reject)
            .unwrap(),
    );
    assert_eq!(fixture.memory_entry("fav").status, MemoryStatus::Rejected);
}

#[test]
fn racing_a_favorite_against_a_reject_never_leaves_a_favorite_rejected() {
    let fixture = Fixture::new();
    for round in 0..20 {
        let id = format!("fav-race-{round}");
        fixture.lesson(&id);
        let revision = fixture.memory_revision(&id);
        let (favorite, reject) = race(
            {
                let (handle, id) = (fixture.handle.clone(), id.clone());
                move || {
                    handle
                        .update_memory(&id, revision, MemoryOp::SetFavorite)
                        .unwrap()
                }
            },
            {
                let (handle, id) = (fixture.handle.clone(), id.clone());
                move || {
                    handle
                        .update_memory(&id, revision, MemoryOp::Reject)
                        .unwrap()
                }
            },
        );
        assert_eq!(
            [favorite, reject]
                .iter()
                .filter(|outcome| outcome.is_applied())
                .count(),
            1,
            "round {round}: {favorite:?} / {reject:?}"
        );
        let entry = fixture.memory_entry(&id);
        assert!(
            !(entry.status == MemoryStatus::Rejected && entry.favorite),
            "round {round}: a rejected favorite"
        );
        assert_eq!(entry.favorite, favorite.is_applied());
    }
}

#[test]
fn a_learning_write_makes_an_older_manual_expectation_stale() {
    let fixture = Fixture::new();
    fixture.lesson("learn");
    let seen = fixture.memory_revision("learn");

    // Learning validates the lesson after the person looked at it.
    fixture
        .handle
        .validate_lesson("learn", "evidence of learn")
        .unwrap();
    let current = fixture.memory_revision("learn");
    assert_eq!(current, seen + 1);

    // The person's reject was judged against the old state; it must not apply.
    assert_eq!(
        refused(
            fixture
                .handle
                .update_memory("learn", seen, MemoryOp::Reject)
                .unwrap()
        ),
        CasRefusal::StaleRevision { current }
    );
    assert_eq!(
        fixture.memory_entry("learn").status,
        MemoryStatus::Validated
    );

    // Promotion by learning invalidates a manual expectation as well.
    fixture.handle.promote_lesson("learn").unwrap();
    assert_eq!(
        refused(
            fixture
                .handle
                .update_memory("learn", current, MemoryOp::SetFavorite)
                .unwrap()
        ),
        CasRefusal::StaleRevision {
            current: current + 1
        }
    );
    assert!(!fixture.memory_entry("learn").favorite);
}

#[test]
fn a_learning_write_racing_a_manual_update_leaves_one_consistent_state() {
    let fixture = Fixture::new();
    for round in 0..20 {
        let id = format!("learn-race-{round}");
        let lesson = fixture.lesson(&id);
        let revision = fixture.memory_revision(&id);
        let (learning, manual) = race(
            {
                let (handle, id, evidence) =
                    (fixture.handle.clone(), id.clone(), lesson.evidence.clone());
                move || handle.validate_lesson(&id, &evidence)
            },
            {
                let (handle, id) = (fixture.handle.clone(), id.clone());
                move || {
                    handle
                        .update_memory(&id, revision, MemoryOp::Reject)
                        .unwrap()
                }
            },
        );
        let entry = fixture.memory_entry(&id);
        if manual.is_applied() {
            // The manual reject ran first; learning then found a rejected lesson and
            // was refused, so it did not resurrect it.
            assert!(learning.is_err(), "round {round}");
            assert_eq!(entry.status, MemoryStatus::Rejected);
        } else {
            // Learning ran first; the manual reject saw a newer revision.
            assert_eq!(
                manual,
                CasOutcome::Refused(CasRefusal::StaleRevision {
                    current: revision + 1
                })
            );
            learning.unwrap();
            assert_eq!(entry.status, MemoryStatus::Validated);
        }
        assert_eq!(fixture.memory_revision(&id), revision + 1);
    }
}

#[test]
fn a_folded_duplicate_makes_an_older_improvement_expectation_stale() {
    let fixture = Fixture::new();
    fixture.record(fixture.candidate("dup"), 1000);
    let seen = fixture.improvement_revision("dup");

    // The same fault occurs again: intake folds it into the candidate.
    let mut again = fixture.candidate("dup-again");
    again.dedup_key = fixture.candidate("dup").dedup_key;
    assert_eq!(
        fixture.record(again, 1000),
        ImprovementRecordOutcome::Duplicate {
            existing_id: "dup".into()
        }
    );
    let current = fixture.improvement_revision("dup");
    assert_ne!(current, seen);

    assert_eq!(
        refused(
            fixture
                .handle
                .update_improvement("dup", seen, ImprovementUpdate::MarkReviewed)
                .unwrap()
        ),
        CasRefusal::StaleRevision { current }
    );
    assert_eq!(fixture.improvement("dup").status, ImprovementStatus::New);

    let revision = applied_revision(
        fixture
            .handle
            .update_improvement("dup", current, ImprovementUpdate::MarkReviewed)
            .unwrap(),
    );
    assert_eq!(revision, fixture.improvement_revision("dup"));
}

#[test]
fn a_missing_entity_is_not_found_whatever_the_revision() {
    let fixture = Fixture::new();
    for expected in [0, 1, 99] {
        assert_eq!(
            refused(
                fixture
                    .handle
                    .update_memory("nothing", expected, MemoryOp::Reject)
                    .unwrap()
            ),
            CasRefusal::NotFound
        );
        assert_eq!(
            refused(
                fixture
                    .handle
                    .update_improvement("nothing", expected, ImprovementUpdate::Dismiss)
                    .unwrap()
            ),
            CasRefusal::NotFound
        );
    }
    assert_eq!(fixture.memory_revision("nothing"), 0);
}

#[test]
fn an_evicted_candidate_is_not_found_and_the_writer_goes_on() {
    let fixture = Fixture::new();
    fixture.record(fixture.candidate("old"), 1);
    let seen = fixture.improvement_revision("old");
    // The retention cap of one evicts the older candidate.
    fixture.record(fixture.candidate("new"), 1);
    assert!(
        fixture
            .reader()
            .improvement_candidate("old")
            .unwrap()
            .is_none()
    );

    assert_eq!(
        refused(
            fixture
                .handle
                .update_improvement("old", seen, ImprovementUpdate::Dismiss)
                .unwrap()
        ),
        CasRefusal::NotFound
    );
    let revision = fixture.improvement_revision("new");
    applied_revision(
        fixture
            .handle
            .update_improvement("new", revision, ImprovementUpdate::Dismiss)
            .unwrap(),
    );
}

#[test]
fn the_three_refusals_are_distinct() {
    let fixture = Fixture::new();
    fixture.lesson("three");
    let revision = fixture.memory_revision("three");
    let not_found = refused(
        fixture
            .handle
            .update_memory("absent", revision, MemoryOp::Validate)
            .unwrap(),
    );
    let stale = refused(
        fixture
            .handle
            .update_memory("three", revision + 5, MemoryOp::Validate)
            .unwrap(),
    );
    let denied = refused(
        fixture
            .handle
            .update_memory("three", revision, MemoryOp::Promote)
            .unwrap(),
    );
    assert_eq!(not_found, CasRefusal::NotFound);
    assert_eq!(stale, CasRefusal::StaleRevision { current: revision });
    assert_eq!(denied, CasRefusal::Denied(Denial::NotApplicableInState));
    // None of them wrote.
    assert_eq!(fixture.memory_revision("three"), revision);
    assert_eq!(
        fixture.memory_entry("three").status,
        MemoryStatus::Candidate
    );
}

#[test]
fn what_the_rules_deny_is_what_the_update_refuses() {
    let fixture = Fixture::new();
    // Every state a lesson can be in: (status, favorite).
    let states = [
        (MemoryStatus::Candidate, false),
        (MemoryStatus::Candidate, true),
        (MemoryStatus::Validated, false),
        (MemoryStatus::Validated, true),
        (MemoryStatus::Promoted, false),
        (MemoryStatus::Promoted, true),
        (MemoryStatus::Rejected, false),
    ];
    for (state_index, (status, favorite)) in states.into_iter().enumerate() {
        for op in MemoryOp::ALL {
            let id = format!("rule-{state_index}-{op:?}");
            let lesson = fixture.lesson(&id);
            if status != MemoryStatus::Candidate {
                fixture
                    .handle
                    .validate_lesson(&id, &lesson.evidence)
                    .unwrap();
            }
            if matches!(status, MemoryStatus::Promoted) {
                fixture.handle.promote_lesson(&id).unwrap();
            }
            if favorite {
                fixture.handle.set_lesson_favorite(&id, true).unwrap();
            }
            if status == MemoryStatus::Rejected {
                fixture.handle.reject_lesson(&id).unwrap();
            }
            let before = fixture.memory_entry(&id);
            assert_eq!((before.status, before.favorite), (status, favorite));
            let revision = fixture.memory_revision(&id);

            let outcome = fixture.handle.update_memory(&id, revision, op).unwrap();
            match memory_denial(op, status, favorite) {
                Some(denial) => {
                    assert_eq!(
                        outcome,
                        CasOutcome::Refused(CasRefusal::Denied(denial)),
                        "{op:?} on {status:?} favorite={favorite}"
                    );
                    assert_eq!(fixture.memory_revision(&id), revision);
                }
                None => {
                    assert_eq!(
                        applied_revision(outcome),
                        fixture.memory_revision(&id),
                        "{op:?} on {status:?} favorite={favorite}"
                    );
                }
            }
        }
    }

    // The improvement rules, the same way.
    for (index, status) in [
        ImprovementStatus::New,
        ImprovementStatus::Reviewed,
        ImprovementStatus::Dismissed,
    ]
    .into_iter()
    .enumerate()
    {
        for update in [
            ImprovementUpdate::MarkReviewed,
            ImprovementUpdate::Dismiss,
            ImprovementUpdate::AttachDraft {
                draft_path: "draft.md".into(),
            },
        ] {
            let id = format!("imp-rule-{index}-{:?}", update.op());
            fixture.record(fixture.candidate(&id), 1000);
            if status != ImprovementStatus::New {
                assert!(fixture.handle.set_improvement_status(&id, status).unwrap());
            }
            let revision = fixture.improvement_revision(&id);
            let outcome = fixture
                .handle
                .update_improvement(&id, revision, update.clone())
                .unwrap();
            match improvement_denial(update.op(), status) {
                Some(denial) => {
                    assert_eq!(outcome, CasOutcome::Refused(CasRefusal::Denied(denial)));
                    assert_eq!(fixture.improvement_revision(&id), revision);
                }
                None => assert_eq!(applied_revision(outcome), fixture.improvement_revision(&id)),
            }
        }
    }
    assert_eq!(ImprovementOp::ALL.len(), 3);
}

#[test]
fn an_update_that_changes_nothing_keeps_the_revision() {
    let fixture = Fixture::new();
    fixture.record(fixture.candidate("draft"), 1000);
    let revision = fixture.improvement_revision("draft");
    let attach = || ImprovementUpdate::AttachDraft {
        draft_path: "draft-1.md".into(),
    };

    let after = applied_revision(
        fixture
            .handle
            .update_improvement("draft", revision, attach())
            .unwrap(),
    );
    assert_ne!(after, revision);
    assert_eq!(after, fixture.improvement_revision("draft"));

    // Attaching the same reference again is allowed and writes nothing.
    assert_eq!(
        fixture
            .handle
            .update_improvement("draft", after, attach())
            .unwrap(),
        CasOutcome::Unchanged { revision: after }
    );
    assert_eq!(fixture.improvement_revision("draft"), after);

    // A different reference moves the revision again.
    let third = applied_revision(
        fixture
            .handle
            .update_improvement(
                "draft",
                after,
                ImprovementUpdate::AttachDraft {
                    draft_path: "draft-2.md".into(),
                },
            )
            .unwrap(),
    );
    assert_ne!(third, after);
}

#[test]
fn a_failed_write_rolls_back_and_the_writer_keeps_working() {
    let fixture = Fixture::new();

    // Lesson: a trigger makes the ledger insert of a favorite fail inside the update.
    fixture.lesson("boom");
    let revision = fixture.memory_revision("boom");
    // A trigger visible to the writer's connection must live in the file.
    let side = fixture.side_connection();
    side.execute_batch(
        "CREATE TRIGGER refuse_favorites BEFORE INSERT ON memory_ledger
         WHEN NEW.kind IN ('favorite', 'unfavorite')
         BEGIN SELECT RAISE(ABORT, 'refused by test'); END;",
    )
    .unwrap();
    let failed = fixture
        .handle
        .update_memory("boom", revision, MemoryOp::SetFavorite);
    assert!(matches!(failed, Err(StorageError::Sqlite(_))), "{failed:?}");
    assert_eq!(fixture.memory_revision("boom"), revision);
    assert!(!fixture.memory_entry("boom").favorite);

    // The writer still answers, and the same update works once the cause is gone.
    side.execute_batch("DROP TRIGGER refuse_favorites;")
        .unwrap();
    let after = applied_revision(
        fixture
            .handle
            .update_memory("boom", revision, MemoryOp::SetFavorite)
            .unwrap(),
    );
    assert_eq!(after, revision + 1);
    assert!(fixture.memory_entry("boom").favorite);

    // Improvement: a trigger on the status update fails the whole update.
    fixture.record(fixture.candidate("imp-boom"), 1000);
    let imp_revision = fixture.improvement_revision("imp-boom");
    side.execute_batch(
        "CREATE TRIGGER refuse_status BEFORE UPDATE ON improvement_candidates
         BEGIN SELECT RAISE(ABORT, 'refused by test'); END;",
    )
    .unwrap();
    let failed =
        fixture
            .handle
            .update_improvement("imp-boom", imp_revision, ImprovementUpdate::Dismiss);
    assert!(matches!(failed, Err(StorageError::Sqlite(_))), "{failed:?}");
    assert_eq!(fixture.improvement_revision("imp-boom"), imp_revision);
    assert_eq!(
        fixture.improvement("imp-boom").status,
        ImprovementStatus::New
    );
    side.execute_batch("DROP TRIGGER refuse_status;").unwrap();
    applied_revision(
        fixture
            .handle
            .update_improvement("imp-boom", imp_revision, ImprovementUpdate::Dismiss)
            .unwrap(),
    );
    assert_eq!(
        fixture.improvement("imp-boom").status,
        ImprovementStatus::Dismissed
    );
}

#[test]
fn an_invalid_draft_reference_is_a_storage_error_not_a_refusal() {
    let fixture = Fixture::new();
    fixture.record(fixture.candidate("empty"), 1000);
    let revision = fixture.improvement_revision("empty");
    let failed = fixture.handle.update_improvement(
        "empty",
        revision,
        ImprovementUpdate::AttachDraft {
            draft_path: "  ".into(),
        },
    );
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(fixture.improvement_revision("empty"), revision);
    // The same handle is still good.
    applied_revision(
        fixture
            .handle
            .update_improvement("empty", revision, ImprovementUpdate::MarkReviewed)
            .unwrap(),
    );
}
