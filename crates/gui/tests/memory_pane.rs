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
    harness.get_by_role(Role::ComboBox).click();
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
