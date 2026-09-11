use egui_kittest::{Harness, kittest::Queryable};
use gui::panes::arena::ArenaPane;

#[test]
fn arena_promotion_requires_a_second_explicit_click() {
    // Given: persisted successful evaluation and the real pane.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = storage::StorageConfig {
        db_path: dir.path().join("arena.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).expect("store");
    let spec = arena::ArenaSpec {
        id: "run".into(),
        project: "p".into(),
        task: arena::TaskSpec {
            id: "task".into(),
            prompt: "task".into(),
            expected_output: "ok".into(),
        },
        configs: ["a", "b"]
            .into_iter()
            .map(|id| arena::ArenaConfig {
                id: id.into(),
                profile: "local".into(),
                model: "mock".into(),
                attribution: arena::Attribution::Worker,
                variant: Default::default(),
            })
            .collect(),
        max_output_tokens: 16,
        total_token_budget: 200,
        timeout_ms: 1000,
    };
    for id in ["a", "b"] {
        store
            .handle()
            .append_eval_trace(&storage::eval::EvalTrace {
                id: format!("run/{id}"),
                arena_id: "run".into(),
                project: "p".into(),
                task_id: "task".into(),
                task_spec: serde_json::to_string(&spec).expect("manifest"),
                config_id: id.into(),
                profile: "local".into(),
                model: "mock".into(),
                attribution: storage::eval::Attribution::Worker,
                execution: None,
                output: "ok".into(),
                input_tokens: 3,
                output_tokens: 1,
                elapsed_ms: 5,
                failure: None,
            })
            .expect("trace");
    }
    let pane = ArenaPane::default();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 600.0))
        .build_ui_state(
            move |ui, pane: &mut ArenaPane| pane.render(ui, Some((&config, "p"))),
            pane,
        );
    harness.run();
    if let Ok(path) = std::env::var("EVORCH_ARENA_CAPTURE") {
        harness
            .render()
            .expect("render")
            .save(format!("{path}.comparison.png"))
            .expect("capture");
    }
    // When: the user requests promotion without confirming yet.
    harness.get_by_label("Propose a").click();
    harness.run();
    // Then: a confirmation is visible but no exported candidate exists.
    assert!(harness.query_by_label("Copy routing candidate").is_none());
    if let Ok(path) = std::env::var("EVORCH_ARENA_CAPTURE") {
        harness
            .render()
            .expect("render")
            .save(format!("{path}.confirmation.png"))
            .expect("capture");
    }
    harness.get_by_label("Confirm candidate").click();
    harness.run();
    harness.get_by_label("Copy routing candidate");
    harness.get_by_label("Active routing is unchanged.");
    if let Ok(path) = std::env::var("EVORCH_ARENA_CAPTURE") {
        harness
            .render()
            .expect("render")
            .save(path)
            .expect("capture");
    }
}
