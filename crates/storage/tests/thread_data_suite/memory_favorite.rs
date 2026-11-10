//! The favorite flag is the user's approval mark: independent of the lesson status,
//! never reshuffling what task prompts receive, and protecting the lesson from rejection.

use rusqlite::Connection;
use storage::memory::{FavoriteChange, Lesson, LessonScope, MemoryFilter, MemoryStatus};
use storage::{Database, Storage, StorageConfig, StorageError, StorageHandle};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    config: StorageConfig,
    store: Storage,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    Fixture {
        _dir: dir,
        config,
        store,
    }
}

impl Fixture {
    fn handle(&self) -> StorageHandle {
        self.store.handle()
    }

    fn db(&self) -> Database {
        Database::open(&self.config).unwrap()
    }

    /// Appends a project lesson and walks it to `status`.
    fn lesson_at(&self, id: &str, status: MemoryStatus) {
        let handle = self.handle();
        handle
            .append_lesson(&lesson(id, &format!("content of {id}")))
            .unwrap();
        match status {
            MemoryStatus::Candidate => {}
            MemoryStatus::Validated => handle.validate_lesson(id, "test:ev").unwrap(),
            MemoryStatus::Promoted => {
                handle.validate_lesson(id, "test:ev").unwrap();
                handle.promote_lesson(id).unwrap();
            }
            MemoryStatus::Rejected => handle.reject_lesson(id).unwrap(),
        }
    }

    fn favorite(&self, id: &str) -> bool {
        self.db().memory_entry(id).unwrap().unwrap().favorite
    }

    fn status(&self, id: &str) -> MemoryStatus {
        self.db().memory_entry(id).unwrap().unwrap().status
    }

    fn ledger_len(&self) -> i64 {
        Connection::open(&self.config.db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM memory_ledger", [], |row| row.get(0))
            .unwrap()
    }

    fn ledger_seq(&self, id: &str) -> i64 {
        Connection::open(&self.config.db_path)
            .unwrap()
            .query_row(
                "SELECT ledger_seq FROM memory_entries WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    }
}

fn lesson(id: &str, content: &str) -> Lesson {
    Lesson {
        id: id.into(),
        project: "p".into(),
        task_id: "t".into(),
        content: content.into(),
        evidence: "test:ev".into(),
        scope: LessonScope::Project,
    }
}

fn is_favorite_error(result: Result<(), StorageError>) -> bool {
    matches!(result, Err(StorageError::LessonFavorite { .. }))
}

#[test]
fn favorite_is_allowed_in_every_status_except_rejected() {
    // Given: one lesson per status.
    let fx = fixture();
    let cases = [
        (MemoryStatus::Candidate, true),
        (MemoryStatus::Validated, true),
        (MemoryStatus::Promoted, true),
        (MemoryStatus::Rejected, false),
    ];
    for (status, allowed) in cases {
        let id = format!("l-{}", status.as_str());
        fx.lesson_at(&id, status);
        let ledger = fx.ledger_len();

        // When: the favorite flag is set and cleared.
        let set = fx.handle().set_lesson_favorite(&id, true);
        let clear = fx.handle().set_lesson_favorite(&id, false);

        // Then: it works on every status but Rejected, and never changes the status.
        assert_eq!(set.is_ok(), allowed, "set on {status:?}");
        assert_eq!(clear.is_ok(), allowed, "clear on {status:?}");
        assert_eq!(fx.status(&id), status);
        assert!(!fx.favorite(&id));
        assert_eq!(fx.ledger_len(), ledger + if allowed { 2 } else { 0 });
    }
}

#[test]
fn unknown_lesson_cannot_be_favorited() {
    let fx = fixture();
    assert!(fx.handle().set_lesson_favorite("missing", true).is_err());
    assert_eq!(fx.ledger_len(), 0);
}

#[test]
fn favorite_survives_validation_and_promotion() {
    // Given: a favorite candidate.
    let fx = fixture();
    fx.lesson_at("l", MemoryStatus::Candidate);
    fx.handle().set_lesson_favorite("l", true).unwrap();

    // When / Then: each transition keeps the mark.
    fx.handle().validate_lesson("l", "test:ev").unwrap();
    assert_eq!(
        (fx.status("l"), fx.favorite("l")),
        (MemoryStatus::Validated, true)
    );
    fx.handle().promote_lesson("l").unwrap();
    assert_eq!(
        (fx.status("l"), fx.favorite("l")),
        (MemoryStatus::Promoted, true)
    );
    let boundary = fx.db().boundary_memory("p").unwrap();
    assert_eq!(boundary.len(), 1);
    assert!(boundary[0].favorite);
}

#[test]
fn favorite_blocks_reject_until_cleared() {
    // Given: a favorite lesson in each rejectable status.
    for status in [
        MemoryStatus::Candidate,
        MemoryStatus::Validated,
        MemoryStatus::Promoted,
    ] {
        let fx = fixture();
        fx.lesson_at("l", status);
        fx.handle().set_lesson_favorite("l", true).unwrap();
        let ledger = fx.ledger_len();
        let seq = fx.ledger_seq("l");

        // When: rejecting it.
        let result = fx.handle().reject_lesson("l");

        // Then: refused, and neither ledger nor projection changed.
        assert!(is_favorite_error(result), "{status:?}");
        assert_eq!((fx.status("l"), fx.favorite("l")), (status, true));
        assert_eq!((fx.ledger_len(), fx.ledger_seq("l")), (ledger, seq));

        // When: the mark is cleared first.
        fx.handle().set_lesson_favorite("l", false).unwrap();
        fx.handle().reject_lesson("l").unwrap();

        // Then: the lesson is rejected and unmarked.
        assert_eq!(
            (fx.status("l"), fx.favorite("l")),
            (MemoryStatus::Rejected, false)
        );
    }
}

#[test]
fn rejected_lesson_cannot_be_favorited_again() {
    let fx = fixture();
    fx.lesson_at("l", MemoryStatus::Candidate);
    fx.handle().reject_lesson("l").unwrap();
    let ledger = fx.ledger_len();
    assert!(fx.handle().set_lesson_favorite("l", true).is_err());
    assert!(fx.handle().set_lesson_favorite("l", false).is_err());
    assert_eq!(fx.ledger_len(), ledger);
    assert!(!fx.favorite("l"));
}

#[test]
fn flag_changes_are_ledger_history_and_repeating_a_value_is_a_no_op() {
    // Given: a promoted lesson.
    let fx = fixture();
    fx.lesson_at("l", MemoryStatus::Promoted);
    let lesson_rows = fx.ledger_len();

    // When: the flag is set twice, cleared twice, then set once more.
    fx.handle().set_lesson_favorite("l", true).unwrap();
    fx.handle().set_lesson_favorite("l", true).unwrap();
    fx.handle().set_lesson_favorite("l", false).unwrap();
    fx.handle().set_lesson_favorite("l", false).unwrap();
    fx.handle().set_lesson_favorite("l", true).unwrap();

    // Then: only real changes are recorded, in order, as their own kind of row.
    let db = fx.db();
    assert_eq!(fx.ledger_len(), lesson_rows + 3);
    assert_eq!(
        db.favorite_history("l").unwrap(),
        vec![
            FavoriteChange {
                favorite: true,
                status: MemoryStatus::Promoted
            },
            FavoriteChange {
                favorite: false,
                status: MemoryStatus::Promoted
            },
            FavoriteChange {
                favorite: true,
                status: MemoryStatus::Promoted
            },
        ]
    );
    // And: the lesson history still lists only status transitions.
    let statuses: Vec<_> = db
        .memory_history("l")
        .unwrap()
        .iter()
        .map(|e| e.status)
        .collect();
    assert_eq!(
        statuses,
        [
            MemoryStatus::Candidate,
            MemoryStatus::Validated,
            MemoryStatus::Promoted
        ]
    );
}

#[test]
fn lesson_history_rows_carry_the_flag_as_of_that_row() {
    // Given: a candidate favorited before it is validated and promoted.
    let fx = fixture();
    fx.lesson_at("l", MemoryStatus::Candidate);
    fx.handle().set_lesson_favorite("l", true).unwrap();
    fx.handle().validate_lesson("l", "test:ev").unwrap();
    fx.handle().set_lesson_favorite("l", false).unwrap();
    fx.handle().promote_lesson("l").unwrap();

    // When / Then: history shows the flag in force at each status row.
    let flags: Vec<_> = fx
        .db()
        .memory_history("l")
        .unwrap()
        .iter()
        .map(|entry| (entry.status, entry.favorite))
        .collect();
    assert_eq!(
        flags,
        [
            (MemoryStatus::Candidate, false),
            (MemoryStatus::Validated, true),
            (MemoryStatus::Promoted, false),
        ]
    );
}

#[test]
fn favorite_persists_across_reopen_and_is_filterable() {
    // Given: two lessons, one favorite.
    let fx = fixture();
    fx.lesson_at("fav", MemoryStatus::Validated);
    fx.lesson_at("plain", MemoryStatus::Validated);
    fx.handle().set_lesson_favorite("fav", true).unwrap();
    let Fixture {
        _dir,
        config,
        store,
    } = fx;
    drop(store);

    // When: the database is opened again.
    let db = Database::open(&config).unwrap();

    // Then: the mark is still there and the filter splits the lessons.
    let ids = |favorite| -> Vec<String> {
        db.search_memory_filtered(
            "p",
            "",
            MemoryFilter {
                status: None,
                favorite,
            },
        )
        .unwrap()
        .into_iter()
        .map(|entry| entry.lesson.id)
        .collect()
    };
    assert_eq!(ids(Some(true)), ["fav"]);
    assert_eq!(ids(Some(false)), ["plain"]);
    assert_eq!(ids(None).len(), 2);
    let both = MemoryFilter {
        status: Some(MemoryStatus::Promoted),
        favorite: Some(true),
    };
    assert!(db.search_memory_filtered("p", "", both).unwrap().is_empty());
}

#[test]
fn full_text_search_still_finds_a_lesson_after_its_flag_changes() {
    // Given: a lesson that is searchable by a distinctive word.
    let fx = fixture();
    fx.handle()
        .append_lesson(&lesson("l", "prefer quokka-safe retries"))
        .unwrap();

    // When: the flag is set and cleared (each updates the projection row).
    fx.handle().set_lesson_favorite("l", true).unwrap();
    let marked = fx.db().search_memory("p", "quokka", None).unwrap();
    fx.handle().set_lesson_favorite("l", false).unwrap();
    let cleared = fx.db().search_memory("p", "quokka", None).unwrap();

    // Then: full-text search finds it exactly once at each point, with the right flag.
    assert_eq!(marked.len(), 1);
    assert!(marked[0].favorite);
    assert_eq!(cleared.len(), 1);
    assert!(!cleared[0].favorite);
}

#[test]
fn favorite_changes_neither_reorder_nor_reselect_what_tasks_receive() {
    // Given: three promoted lessons, oldest first, plus a project-scoped candidate.
    let fx = fixture();
    for id in ["a", "b", "c"] {
        fx.lesson_at(id, MemoryStatus::Promoted);
    }
    fx.lesson_at("d", MemoryStatus::Candidate);
    let ids = |entries: Vec<storage::memory::MemoryEntry>| -> Vec<String> {
        entries.into_iter().map(|entry| entry.lesson.id).collect()
    };
    let injected_before = ids(fx.db().boundary_memory("p").unwrap());
    let listed_before = ids(fx.db().search_memory("p", "", None).unwrap());
    let seqs_before: Vec<_> = ["a", "b", "c", "d"].map(|id| fx.ledger_seq(id)).into();
    assert_eq!(injected_before, ["c", "b", "a"]);

    // When: the oldest promoted lesson and the candidate are marked, then the mark moves.
    fx.handle().set_lesson_favorite("a", true).unwrap();
    fx.handle().set_lesson_favorite("d", true).unwrap();
    fx.handle().set_lesson_favorite("c", true).unwrap();
    fx.handle().set_lesson_favorite("c", false).unwrap();

    // Then: injection order and membership, listing order and projection sequence
    // numbers are all exactly as before.
    assert_eq!(ids(fx.db().boundary_memory("p").unwrap()), injected_before);
    assert_eq!(
        ids(fx.db().search_memory("p", "", None).unwrap()),
        listed_before
    );
    let seqs_after: Vec<_> = ["a", "b", "c", "d"].map(|id| fx.ledger_seq(id)).into();
    assert_eq!(seqs_after, seqs_before);
}

#[test]
fn reappending_a_lesson_keeps_the_mark_and_a_conflicting_batch_changes_nothing() {
    // Given: a favorite lesson.
    let fx = fixture();
    fx.handle().append_lesson(&lesson("l", "original")).unwrap();
    fx.handle().set_lesson_favorite("l", true).unwrap();
    let ledger = fx.ledger_len();

    // When: the identical lesson is appended again.
    fx.handle().append_lesson(&lesson("l", "original")).unwrap();

    // Then: nothing changed.
    assert!(fx.favorite("l"));
    assert_eq!(fx.ledger_len(), ledger);

    // When: a batch holding a new lesson and a conflicting copy fails.
    let batch = [lesson("fresh", "new"), lesson("l", "different")];
    assert!(fx.handle().append_lessons(&batch).is_err());

    // Then: ledger and projection are untouched, mark included.
    assert!(fx.favorite("l"));
    assert_eq!(fx.ledger_len(), ledger);
    assert!(fx.db().memory_entry("fresh").unwrap().is_none());
}

#[test]
fn database_refuses_rejected_favorites_and_non_boolean_marks() {
    // Given: a rejected lesson and a candidate, written through the API.
    let fx = fixture();
    fx.lesson_at("rejected", MemoryStatus::Rejected);
    fx.lesson_at("candidate", MemoryStatus::Candidate);
    fx.lesson_at("marked", MemoryStatus::Candidate);
    fx.handle().set_lesson_favorite("marked", true).unwrap();
    let raw = Connection::open(&fx.config.db_path).unwrap();

    // When / Then: raw SQL cannot mark a rejected lesson, reject a favorite, or store
    // any value but 0 and 1.
    assert!(
        raw.execute(
            "UPDATE memory_entries SET favorite = 1 WHERE id = 'rejected'",
            []
        )
        .is_err()
    );
    raw.execute(
        "UPDATE memory_entries SET favorite = 1 WHERE id = 'candidate'",
        [],
    )
    .unwrap();
    assert!(
        raw.execute(
            "UPDATE memory_entries SET status = 'rejected' WHERE id = 'candidate'",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE memory_entries SET favorite = 2 WHERE id = 'candidate'",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM memory_ledger WHERE kind = 'favorite'", [])
            .is_err()
    );
}
