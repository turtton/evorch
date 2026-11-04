use egui::accesskit::Role;
use egui_kittest::{Harness, kittest::Queryable};
use gui::panes::memory::MemoryPane;
use storage::memory::{Lesson, MemoryStatus};
use storage::{Database, Storage, StorageConfig};

fn repo(root: &std::path::Path, origin: &str) {
    std::fs::create_dir_all(root).unwrap();
    for args in [vec!["init"], vec!["remote", "add", "origin", origin]] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn memory_pane_search_and_filter_use_persisted_projection() {
    // Given: a candidate learned under the repository slug, not the checkout directory name.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("checkout");
    repo(&root, "https://github.com/owner/name.git");
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    store
        .handle()
        .append_lesson(&Lesson {
            id: "l".into(),
            project: "owner/name".into(),
            task_id: "t".into(),
            content: "Bound concurrency".into(),
            evidence: "test:bound".into(),
            scope: storage::memory::LessonScope::Project,
        })
        .unwrap();
    let mut pane = MemoryPane::default();
    pane.config = Some(config);
    pane.query = "concurrency".into();
    pane.status = Some(MemoryStatus::Candidate);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(700.0, 500.0))
        .build_ui_state(
            move |ui, pane: &mut MemoryPane| pane.render(ui, Some(&root)),
            pane,
        );
    // When: render the filtered pane.
    harness.run();
    // Then: persisted lesson and explicit state are visible.
    harness.get_by_label("Bound concurrency");
    if let Ok(path) = std::env::var("EVORCH_MEMORY_CAPTURE") {
        harness.render().unwrap().save(path).unwrap();
    }
    harness.get_by_label("1 lessons (up to 100)");
    harness.get_by_label("Refresh").click();
    harness.run();
    harness.get_by_label("Bound concurrency");
}

#[test]
fn memory_pane_opens_on_promoted_lessons() {
    // Given: a promoted lesson and a candidate lesson for the same project.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("checkout");
    repo(&root, "https://github.com/owner/name.git");
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let handle = store.handle();
    for (id, content) in [("p", "Always prefer tabs"), ("c", "Bound concurrency")] {
        handle
            .append_lesson(&Lesson {
                id: id.into(),
                project: "owner/name".into(),
                task_id: "t".into(),
                content: content.into(),
                evidence: "test:default".into(),
                scope: storage::memory::LessonScope::Project,
            })
            .unwrap();
    }
    handle.validate_lesson("p", "test:default").unwrap();
    handle.promote_lesson("p").unwrap();
    let mut pane = MemoryPane::default();
    pane.config = Some(config);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(700.0, 500.0))
        .build_ui_state(
            move |ui, pane: &mut MemoryPane| pane.render(ui, Some(&root)),
            pane,
        );

    // When: the default pane renders.
    harness.run();
    // Then: only the lesson task prompts receive is listed.
    harness.get_by_label("Always prefer tabs");
    assert!(harness.query_by_label("Bound concurrency").is_none());
    harness.get_by_label("1 lessons (up to 100)");

    // When: the status filter is widened to all statuses.
    harness
        .get_all_by_role(Role::ComboBox)
        .next()
        .unwrap()
        .click();
    harness.run();
    harness.get_by_label("All statuses").click();
    harness.run();
    // Then: the candidate is listed too.
    harness.get_by_label("Bound concurrency");
    harness.get_by_label("Always prefer tabs");
}

#[test]
fn memory_pane_rejects_a_lesson_only_after_confirmation() {
    // Given: a promoted lesson the pane can write through the storage writer.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("checkout");
    repo(&root, "https://github.com/owner/name.git");
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let handle = store.handle();
    handle
        .append_lesson(&Lesson {
            id: "l".into(),
            project: "owner/name".into(),
            task_id: "t".into(),
            content: "Always prefer tabs".into(),
            evidence: "test:tabs".into(),
            scope: storage::memory::LessonScope::User,
        })
        .unwrap();
    handle.validate_lesson("l", "test:tabs").unwrap();
    handle.promote_lesson("l").unwrap();
    let mut pane = MemoryPane::default();
    pane.config = Some(config.clone());
    pane.handle = Some(handle);
    // All statuses, so the rejected lesson stays listed after the refresh.
    pane.status = None;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(700.0, 500.0))
        .build_ui_state(
            move |ui, pane: &mut MemoryPane| pane.render(ui, Some(&root)),
            pane,
        );
    let status = || {
        Database::open(&config)
            .unwrap()
            .memory_history("l")
            .unwrap()
            .last()
            .unwrap()
            .status
    };
    harness.run();

    // When: Reject is clicked and then cancelled.
    harness.get_by_label("Reject").click();
    harness.run();
    harness.get_by_label("Cancel").click();
    harness.run();
    // Then: nothing is written.
    assert_eq!(status(), MemoryStatus::Promoted);

    // When: Reject is clicked and confirmed.
    harness.get_by_label("Reject").click();
    harness.run();
    harness.get_by_label("Confirm reject").click();
    harness.run();
    // Then: the lesson is rejected and the pane no longer offers rejecting it.
    assert_eq!(status(), MemoryStatus::Rejected);
    harness.get_by_label("rejected");
    assert!(harness.query_by_label("Reject").is_none());
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Storage,
    config: StorageConfig,
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        repo(&root, "https://github.com/owner/name.git");
        let config = StorageConfig {
            db_path: dir.path().join("memory.db"),
            ..Default::default()
        };
        let store = Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            store,
            config,
            root,
        }
    }

    /// A promoted lesson of the repository.
    fn promoted(&self, id: &str, content: &str) {
        let handle = self.store.handle();
        handle
            .append_lesson(&Lesson {
                id: id.into(),
                project: "owner/name".into(),
                task_id: "t".into(),
                content: content.into(),
                evidence: "test:ev".into(),
                scope: storage::memory::LessonScope::Project,
            })
            .unwrap();
        handle.validate_lesson(id, "test:ev").unwrap();
        handle.promote_lesson(id).unwrap();
    }

    /// A pane over every status; `writable` decides whether it can write.
    fn harness(&self, writable: bool) -> Harness<'static, MemoryPane> {
        let mut pane = MemoryPane::default();
        pane.config = Some(self.config.clone());
        pane.handle = writable.then(|| self.store.handle());
        pane.status = None;
        let root = self.root.clone();
        Harness::builder()
            .with_size(egui::vec2(700.0, 500.0))
            .build_ui_state(
                move |ui, pane: &mut MemoryPane| pane.render(ui, Some(&root)),
                pane,
            )
    }

    fn entry(&self, id: &str) -> storage::memory::MemoryEntry {
        Database::open(&self.config)
            .unwrap()
            .memory_entry(id)
            .unwrap()
            .unwrap()
    }
}

#[test]
fn memory_pane_toggles_approval_independently_of_status() {
    // Given: an unconfirmed promoted lesson in a writable pane.
    let fx = Fixture::new();
    fx.promoted("l", "Always prefer tabs");
    let mut harness = fx.harness(true);
    harness.run();
    harness.get_by_label("Unconfirmed");
    harness.get_by_label("Reject");

    // When: it is approved.
    harness.get_by_label("Approve").click();
    harness.run();

    // Then: the approval is stored without touching the status, the badge says so,
    // and the pane offers no Reject any more.
    let entry = fx.entry("l");
    assert_eq!(
        (entry.status, entry.favorite),
        (MemoryStatus::Promoted, true)
    );
    harness.get_by_label("Approved");
    harness.get_by_label("promoted");
    assert!(harness.query_by_label("Unconfirmed").is_none());
    assert!(harness.query_by_label("Reject").is_none());

    // When: the approval is cleared.
    harness.get_by_label("Clear approval").click();
    harness.run();

    // Then: Reject is back and the lesson is unconfirmed again.
    assert!(!fx.entry("l").favorite);
    harness.get_by_label("Unconfirmed");
    harness.get_by_label("Reject");
}

#[test]
fn memory_pane_drops_a_pending_reject_confirmation_when_approval_changes() {
    // Given: a Reject awaiting confirmation.
    let fx = Fixture::new();
    fx.promoted("l", "Always prefer tabs");
    let mut harness = fx.harness(true);
    harness.run();
    harness.get_by_label("Reject").click();
    harness.run();
    harness.get_by_label("Confirm reject");

    // When: the lesson is approved and the approval cleared again.
    harness.get_by_label("Approve").click();
    harness.run();
    assert!(harness.query_by_label("Confirm reject").is_none());
    harness.get_by_label("Clear approval").click();
    harness.run();

    // Then: the old confirmation did not come back; rejecting needs a fresh click.
    assert!(harness.query_by_label("Confirm reject").is_none());
    harness.get_by_label("Reject");
    assert_eq!(fx.entry("l").status, MemoryStatus::Promoted);
}

#[test]
fn memory_pane_refresh_drops_a_pending_reject_confirmation() {
    let fx = Fixture::new();
    fx.promoted("l", "Always prefer tabs");
    let mut harness = fx.harness(true);
    harness.run();
    harness.get_by_label("Reject").click();
    harness.run();
    harness.get_by_label("Confirm reject");

    harness.get_by_label("Refresh").click();
    harness.run();

    assert!(harness.query_by_label("Confirm reject").is_none());
    harness.get_by_label("Reject");
}

#[test]
fn memory_pane_shows_storage_refusal_when_a_stale_reject_hits_a_favorite() {
    // Given: a confirmation shown for a lesson that someone then approved elsewhere.
    let fx = Fixture::new();
    fx.promoted("l", "Always prefer tabs");
    let mut harness = fx.harness(true);
    harness.run();
    harness.get_by_label("Reject").click();
    harness.run();
    fx.store.handle().set_lesson_favorite("l", true).unwrap();

    // When: the stale confirmation is used.
    harness.get_by_label("Confirm reject").click();
    harness.run();
    harness.run();

    // Then: storage refused, the reason stays visible, and the re-read pane shows the
    // approved lesson without any Reject.
    let entry = fx.entry("l");
    assert_eq!(
        (entry.status, entry.favorite),
        (MemoryStatus::Promoted, true)
    );
    harness.get_by_label_contains("is a favorite");
    harness.get_by_label("Approved");
    assert!(harness.query_by_label("Reject").is_none());
    assert!(harness.query_by_label("Confirm reject").is_none());
}

#[test]
fn memory_pane_without_a_writer_shows_approval_but_cannot_change_it() {
    let fx = Fixture::new();
    fx.promoted("fav", "Approved lesson");
    fx.promoted("plain", "Plain lesson");
    fx.store.handle().set_lesson_favorite("fav", true).unwrap();
    let mut harness = fx.harness(false);
    harness.run();

    harness.get_by_label("Approved");
    harness.get_by_label("Unconfirmed");
    for control in ["Approve", "Clear approval", "Reject"] {
        assert!(harness.query_by_label(control).is_none(), "{control}");
    }
}

#[test]
fn memory_pane_filters_by_approval() {
    // Given: one approved and one unconfirmed lesson.
    let fx = Fixture::new();
    fx.promoted("fav", "Approved lesson");
    fx.promoted("plain", "Plain lesson");
    fx.store.handle().set_lesson_favorite("fav", true).unwrap();
    let mut harness = fx.harness(false);
    harness.run();
    harness.get_by_label("2 lessons (up to 100)");

    // When: the approval filter is set to Approved, then to Unconfirmed.
    for (choice, shown, hidden) in [
        ("Approved", "Approved lesson", "Plain lesson"),
        ("Unconfirmed", "Plain lesson", "Approved lesson"),
        ("Any approval", "Plain lesson", "Approved lesson"),
    ] {
        harness
            .get_all_by_role(Role::ComboBox)
            .nth(1)
            .unwrap()
            .click();
        harness.run();
        harness.get_by_role_and_label(Role::Button, choice).click();
        harness.run();

        // Then: the list narrows to the matching lessons.
        harness.get_by_label(shown);
        if choice == "Any approval" {
            harness.get_by_label(hidden);
            harness.get_by_label("2 lessons (up to 100)");
        } else {
            assert!(harness.query_by_label(hidden).is_none(), "{choice}");
            harness.get_by_label("1 lessons (up to 100)");
        }
    }
}
