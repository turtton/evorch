//! The footer shows today's recorded cost and opens the Usage tab breakdown.

use std::time::SystemTime;

use gui::fixture::DemoSource;
use storage::usage::{UsageRequestRecord, UsageStatus};
use storage::{Database, Storage, StorageConfig, system_time_to_ns};

fn record(request_id: &str, model: &str, cost_usd: f64) -> UsageRequestRecord {
    UsageRequestRecord {
        request_id: request_id.into(),
        at_ns: system_time_to_ns(SystemTime::now()).unwrap(),
        provider: "openai".into(),
        profile: Some("main".into()),
        model: model.into(),
        run_id: None,
        parent_run_id: None,
        role: Some("worker".into()),
        purpose: Some("agent".into()),
        status: UsageStatus::Ok,
        failure: None,
        finish_reason: Some("stop".into()),
        input_tokens: 1_000,
        output_tokens: 200,
        cache_read_tokens: 600,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        ttft_ms: Some(150),
        duration_ms: 900,
        cost_usd: Some(cost_usd),
    }
}

/// Steps frames until `ready`; the load thread wakes the context when done.
/// A hang is reported by the nextest watchdog rather than a test deadline.
fn step_until(
    harness: &mut gui::headless::HeadlessWorkbench<DemoSource>,
    ready: impl Fn(&gui::headless::HeadlessWorkbench<DemoSource>) -> bool,
) {
    while !ready(harness) {
        harness.step();
        std::thread::yield_now();
    }
}

fn opened_usage_tab(
    directory: &tempfile::TempDir,
) -> (gui::headless::HeadlessWorkbench<DemoSource>, String) {
    let config = StorageConfig {
        db_path: directory.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .record_usage_requests(vec![
            record("req-1", "model-a", 1.0),
            record("req-2", "model-b", 0.5),
        ])
        .unwrap();
    storage.close();
    let today = Database::open(&config)
        .unwrap()
        .usage_local_today()
        .unwrap();
    let state =
        gui::app::WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("workbench")
            .with_memory_storage(config);
    let mut harness = gui::headless::HeadlessWorkbench::new(state, [1400.0, 900.0]);
    step_until(&mut harness, |harness| harness.has_label("$1.50 today"));
    harness.click_label("$1.50 today");
    step_until(&mut harness, |harness| harness.has_label(&today));
    harness.run();
    (harness, today)
}

#[test]
fn footer_cost_opens_the_usage_breakdown() {
    // Given: two requests recorded today in the events database.
    let directory = tempfile::tempdir().unwrap();

    // When: the footer total arrives and is clicked.
    let (harness, today) = opened_usage_tab(&directory);

    // Then: the Usage tab lists today's day row and the total.
    assert!(harness.has_label(&today));
    assert!(harness.has_label("Total"));
    assert_eq!(harness.count_labels("$1.50"), 2, "day row and total row");
}

#[test]
#[ignore = "writes PNG evidence using an offscreen GPU adapter"]
fn capture_usage_tab_png_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let (mut harness, _) = opened_usage_tab(&directory);
    if let Some(frame) = gui::evidence::capture_or_skip(&mut harness) {
        let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/gui-evidence/usage");
        std::fs::create_dir_all(&output).expect("evidence directory");
        frame
            .save_png(&output.join("usage-breakdown.png"))
            .expect("usage PNG");
    }
}
