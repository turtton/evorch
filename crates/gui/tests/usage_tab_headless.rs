//! The footer shows today's recorded cost and opens the Usage tab views.

use std::time::{Duration, SystemTime};

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

/// A week of varied history for screenshots: several models, roles and purposes.
fn history() -> Vec<UsageRequestRecord> {
    let models = ["gpt-5.4", "claude-sonnet-5", "kimi-k3"];
    let roles = ["orchestrator", "worker", "explorer"];
    let purposes = ["agent", "agent", "agent", "compaction", "title"];
    (1..=6_u64)
        .flat_map(|day| (0..12_u64).map(move |index| (day, index)))
        .map(|(day, index)| {
            let at = SystemTime::now() - Duration::from_secs(day * 86_400 + index * 600);
            let model = models[(day + index) as usize % models.len()];
            let mut record = record(
                &format!("old-{day}-{index}"),
                model,
                0.02 * (index + 1) as f64,
            );
            record.at_ns = system_time_to_ns(at).unwrap();
            record.role = Some(roles[index as usize % roles.len()].into());
            record.purpose = Some(purposes[index as usize % purposes.len()].into());
            record.input_tokens = 20_000 + 4_000 * index + 3_000 * day;
            record.cache_read_tokens = record.input_tokens / 2 + 1_000 * day;
            record.cache_write_tokens = 2_000;
            record.output_tokens = 1_500 + 300 * index;
            record.ttft_ms = Some(300 + 40 * index + 25 * day);
            record.duration_ms = 4_000 + 700 * index;
            record
        })
        .collect()
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
    extra: Vec<UsageRequestRecord>,
) -> (gui::headless::HeadlessWorkbench<DemoSource>, String) {
    opened_usage_tab_with(
        directory,
        extra,
        workspace_ui::SidebarState::default(),
        Vec::new(),
    )
}

fn opened_usage_tab_with(
    directory: &tempfile::TempDir,
    extra: Vec<UsageRequestRecord>,
    sidebar: workspace_ui::SidebarState,
    attributions: Vec<storage::usage::RunAttribution>,
) -> (gui::headless::HeadlessWorkbench<DemoSource>, String) {
    let config = StorageConfig {
        db_path: directory.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let mut records = vec![
        record("req-1", "model-a", 1.0),
        record("req-2", "model-b", 0.5),
    ];
    records.extend(extra);
    // Records written in the last hour are today's; the fixtures place history days earlier.
    let recent = system_time_to_ns(SystemTime::now() - Duration::from_secs(3_600)).unwrap();
    let today_cost: f64 = records
        .iter()
        .filter(|record| record.at_ns >= recent)
        .filter_map(|record| record.cost_usd)
        .sum();
    let footer = format!("${today_cost:.2} today");
    assert!(storage.handle().try_attribute_usage_runs(attributions));
    storage.handle().record_usage_requests(records).unwrap();
    storage.close();
    let today = Database::open(&config)
        .unwrap()
        .usage_local_today()
        .unwrap();
    let state =
        gui::app::WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("workbench")
            .with_sidebar(sidebar)
            .with_memory_storage(config);
    let mut harness = gui::headless::HeadlessWorkbench::new(state, [1400.0, 1000.0]);
    step_until(&mut harness, |harness| harness.has_label(&footer));
    harness.click_label(&footer);
    step_until(&mut harness, |harness| harness.has_label("Daily tokens"));
    harness.run();
    (harness, today)
}

#[test]
fn footer_cost_opens_the_overview_and_breakdown() {
    // Given: two requests recorded today in the events database.
    let directory = tempfile::tempdir().unwrap();

    // When: the footer total is clicked, then the Breakdown view is chosen.
    let (mut harness, today) = opened_usage_tab(&directory, Vec::new());
    assert!(harness.has_label("Cost"), "overview stat tile");
    harness.click_label("Breakdown");
    harness.run();

    // Then: the table lists today's day row and the total.
    assert!(harness.has_label(&today));
    assert!(harness.has_label("Total"));
    assert_eq!(harness.count_labels("$1.50"), 2, "day row and total row");
}

#[test]
fn analysis_view_summarises_latency_and_overhead() {
    // Given: a week of requests across models, roles and purposes.
    let directory = tempfile::tempdir().unwrap();
    let (mut harness, _) = opened_usage_tab(&directory, history());

    // When: the Analysis view is chosen.
    harness.click_label("Analysis");
    harness.run();

    // Then: each analysis section is present, including the purpose split.
    for section in [
        "Model efficiency",
        "Cache use",
        "Project × model",
        "Latency",
        "Overhead by purpose",
    ] {
        assert!(harness.has_label(section), "{section}");
    }
    assert!(harness.has_label("compaction"));
}

#[test]
fn live_view_shows_the_pace_and_explains_a_missing_quota() {
    // Given: today's requests and no Codex subscription.
    let directory = tempfile::tempdir().unwrap();
    let (mut harness, _) = opened_usage_tab(&directory, Vec::new());

    // When: the Live view is chosen and its own data arrives.
    harness.click_label("Live");
    step_until(&mut harness, |harness| harness.has_label("Burn rate"));
    harness.run();

    // Then: today's cost and the quota placeholder are shown.
    assert!(harness.has_label("Today"));
    assert!(
        harness.has_label(
            "No Codex quota is available. Sign in to a Codex subscription to compare it."
        )
    );
}

#[test]
fn request_row_opens_its_conversation() {
    // Given: a request whose run belongs to a sidebar thread.
    let directory = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut sidebar = workspace_ui::SidebarState::default();
    sidebar
        .add_project(workspace_ui::ProjectId::new("demo"), "demo", project.path())
        .unwrap();
    sidebar
        .create_thread(
            workspace_ui::ThreadId::new("thread-1"),
            workspace_ui::ProjectId::new("demo"),
            "Fix login flow",
        )
        .unwrap();
    let mut request = record("req-thread", "model-a", 0.25);
    request.run_id = Some("run-1".into());
    let attribution = storage::usage::RunAttribution {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        project_id: Some("demo".into()),
    };
    let (mut harness, _) =
        opened_usage_tab_with(&directory, vec![request], sidebar, vec![attribution]);

    // When: the Requests view lists it and its thread link is clicked.
    harness.click_label("Request log");
    harness.run();
    assert!(harness.has_label("Showing 3 of 3 requests"));
    // The sidebar lists the same title; collapse it so the row link is unique.
    harness.click_label("Collapse threads of demo");
    harness.run();
    harness.click_label("Fix login flow");
    harness.run();

    // Then: that conversation becomes active.
    assert_eq!(
        harness
            .state()
            .sidebar()
            .active_thread
            .as_ref()
            .map(ToString::to_string),
        Some("thread-1".into())
    );
}

#[test]
#[ignore = "writes PNG evidence using an offscreen GPU adapter"]
fn capture_usage_tab_png_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let (mut harness, _) = opened_usage_tab(&directory, history());
    let output =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gui-evidence/usage");
    std::fs::create_dir_all(&output).expect("evidence directory");
    for view in ["Overview", "Breakdown", "Analysis", "Request log", "Live"] {
        harness.click_label(view);
        harness.pointer_move(egui::pos2(1390.0, 990.0));
        if view == "Live" {
            step_until(&mut harness, |harness| harness.has_label("Burn rate"));
        }
        harness.run();
        if let Some(frame) = gui::evidence::capture_or_skip(&mut harness) {
            frame
                .save_png(&output.join(format!(
                    "usage-{}.png",
                    view.to_lowercase().replace(' ', "-")
                )))
                .expect("usage PNG");
        }
    }
}
