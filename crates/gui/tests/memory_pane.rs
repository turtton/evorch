use egui_kittest::{Harness, kittest::Queryable};
use gui::panes::memory::MemoryPane;
use storage::memory::{Lesson, MemoryStatus};
use storage::{Storage, StorageConfig};

#[test]
fn memory_pane_search_and_filter_use_persisted_projection() {
    // Given: a persisted candidate and a connected Memory pane.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("memory.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    store
        .handle()
        .append_lesson(&Lesson {
            id: "l".into(),
            project: "p".into(),
            task_id: "t".into(),
            content: "Bound concurrency".into(),
            evidence: "test:bound".into(),
        })
        .unwrap();
    let mut pane = MemoryPane::default();
    pane.config = Some(config);
    pane.query = "concurrency".into();
    pane.status = Some(MemoryStatus::Candidate);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(700.0, 500.0))
        .build_ui_state(|ui, pane: &mut MemoryPane| pane.render(ui, Some("p")), pane);
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
