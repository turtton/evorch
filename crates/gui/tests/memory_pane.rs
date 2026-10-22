use egui_kittest::{Harness, kittest::Queryable};
use gui::panes::memory::MemoryPane;
use storage::memory::{Lesson, MemoryStatus};
use storage::{Storage, StorageConfig};

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
