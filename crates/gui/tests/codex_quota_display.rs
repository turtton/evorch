use egui_kittest::{Harness, kittest::Queryable};
use gui::fixture::DemoSource;
use gui::model::telemetry::quota::{QuotaBackend, QuotaState};
use gui::model::{tasks::TasksModel, telemetry::TelemetryOverlay};
use providers::provider::codex::quota::QuotaError;
use providers::provider::codex::quota::{CodexQuota, QuotaSnapshot, QuotaSource, QuotaWindow};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use std::time::Instant;

struct FakeQuota {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl QuotaBackend for FakeQuota {
    async fn fetch(&mut self) -> Result<QuotaSnapshot, QuotaError> {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            1 => {
                let mut cached = snapshot(true);
                cached.last_error = Some(QuotaError::Timeout);
                Ok(cached)
            }
            _ => Ok(snapshot(false)),
        }
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(if self.calls.load(Ordering::SeqCst) == 2 {
            120
        } else {
            60
        })
    }
}

fn poll_until(state: &mut QuotaState, now: Instant, ready: impl Fn(&QuotaState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready(state) {
        assert!(Instant::now() < deadline, "fake quota worker completed");
        state.poll(now);
        std::thread::yield_now();
    }
}

#[test]
fn polling_keeps_cache_on_failure_and_recovers_at_next_interval() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut state = QuotaState::with_backend(Box::new(FakeQuota {
        calls: calls.clone(),
    }));
    let now = Instant::now();
    poll_until(&mut state, now, |state| state.snapshot.is_some());
    state.poll(now + Duration::from_secs(59));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    poll_until(&mut state, now + Duration::from_secs(60), |state| {
        state.error.is_some()
    });
    assert!(state.snapshot.as_ref().expect("cached quota").stale);
    state.poll(now + Duration::from_secs(179));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    poll_until(&mut state, now + Duration::from_secs(180), |state| {
        state.error.is_none()
    });
    assert!(!state.snapshot.as_ref().expect("fresh quota").stale);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

fn workbench() -> gui::headless::HeadlessWorkbench<DemoSource> {
    workbench_with_calls(0)
}

fn workbench_with_calls(calls: usize) -> gui::headless::HeadlessWorkbench<DemoSource> {
    let state =
        gui::app::WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("workbench")
            .with_quota_backend(Box::new(FakeQuota {
                calls: Arc::new(AtomicUsize::new(calls)),
            }));
    let mut harness = gui::headless::HeadlessWorkbench::new(state, [1200.0, 800.0]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while harness.state().telemetry().quota.snapshot.is_none() {
        assert!(Instant::now() < deadline, "frame loop polls fake quota");
        harness.step();
        std::thread::yield_now();
    }
    harness.click_label("Agents");
    harness.run();
    harness
}

#[test]
fn frame_loop_polls_injected_source_and_displays_quota() {
    let harness = workbench();
    assert!(harness.has_label("Codex quota · Plan: plus"));
}

#[test]
#[ignore = "writes PNG evidence using an offscreen GPU adapter"]
fn capture_codex_quota_png_evidence() {
    for (name, calls) in [("codex-quota", 0), ("codex-quota-stale", 1)] {
        let mut harness = workbench_with_calls(calls);
        assert!(harness.has_label("Codex quota · Plan: plus"));
        if let Some(frame) = gui::evidence::capture_or_skip(&mut harness) {
            let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/gui-evidence/codex-quota");
            std::fs::create_dir_all(&directory).expect("evidence directory");
            frame
                .save_png(&directory.join(format!("{name}.png")))
                .expect("quota PNG");
        }
    }
}

fn snapshot(stale: bool) -> QuotaSnapshot {
    let reset = "2026-09-13T12:00:00Z".parse().expect("timestamp");
    QuotaSnapshot {
        quota: CodexQuota {
            plan: Some("plus".into()),
            primary: Some(QuotaWindow {
                used_percent: 25.0,
                remaining_percent: 75.0,
                window_duration: Duration::from_secs(18000),
                resets_at: reset,
            }),
            secondary: Some(QuotaWindow {
                used_percent: 60.0,
                remaining_percent: 40.0,
                window_duration: Duration::from_secs(604800),
                resets_at: reset,
            }),
            code_review: None,
        },
        source: QuotaSource::AppServer,
        stale,
        fetched_at: reset,
        last_error: None,
    }
}

#[test]
fn quota_snapshot_renders_both_windows_and_plan() {
    // Given: a quota snapshot and the real Agents pane.
    let mut telemetry = TelemetryOverlay::new();
    telemetry.quota.accept(Ok(snapshot(false)));
    let tasks = TasksModel::new(DemoSource(Vec::new()));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(900.0, 400.0))
        .build_ui(move |ui| {
            gui::panes::agents::agents_pane(ui, &tasks, &telemetry);
        });
    // When: the pane renders headlessly.
    harness.run();
    // Then: both windows include usage and their reset time, with the plan.
    harness.get_by_label("Codex quota · Plan: plus");
    harness.get_by_label("5h: 25.0% used · resets 2026-09-13 12:00 UTC");
    harness.get_by_label("7d: 60.0% used · resets 2026-09-13 12:00 UTC");
}

#[test]
fn stale_indicator_shown_when_refresh_failed() {
    // Given: successful quota followed by a failed refresh.
    let mut telemetry = TelemetryOverlay::new();
    let mut cached = snapshot(true);
    cached.last_error = Some(QuotaError::Timeout);
    telemetry.quota.accept(Ok(cached));
    let tasks = TasksModel::new(DemoSource(Vec::new()));
    let mut harness = Harness::builder().build_ui(move |ui| {
        gui::panes::agents::agents_pane(ui, &tasks, &telemetry);
    });
    // When: rendering cached data after the failure.
    harness.run();
    // Then: stale is explicit and the last successful usage remains visible.
    harness.get_by_label("Stale · quota refresh failed");
    harness.get_by_label("Quota error: quota request timed out");
    harness.get_by_label("5h: 25.0% used · resets 2026-09-13 12:00 UTC");
}

#[test]
fn first_failure_shows_unavailable_without_fabricated_usage() {
    let mut telemetry = TelemetryOverlay::new();
    telemetry.quota.accept(Err(QuotaError::Timeout));
    let tasks = TasksModel::new(DemoSource(Vec::new()));
    let mut harness = Harness::builder().build_ui(move |ui| {
        gui::panes::agents::agents_pane(ui, &tasks, &telemetry);
    });
    harness.run();
    harness.get_by_label("Codex quota unavailable · refresh failed");
    harness.get_by_label("Quota error: quota request timed out");
    assert!(harness.query_by_label("Codex quota · Plan: plus").is_none());
}

#[test]
fn reauth_required_shows_relogin_message() {
    for cached in [false, true] {
        // Given: authentication is rejected, with or without cached usage.
        let mut telemetry = TelemetryOverlay::new();
        if cached {
            telemetry.quota.accept(Ok(snapshot(false)));
        }
        telemetry
            .quota
            .accept(Err(QuotaError::ReauthenticationRequired));
        let tasks = TasksModel::new(DemoSource(Vec::new()));
        let mut harness = Harness::builder().build_ui(move |ui| {
            gui::panes::agents::agents_pane(ui, &tasks, &telemetry);
        });
        // When: the real Agents pane renders the authentication failure.
        harness.run();
        // Then: re-login is actionable and cached usage is not discarded.
        harness
            .get_by_label("Quota error: quota authentication expired or rejected; re-login needed");
        if cached {
            harness.get_by_label("Stale · quota refresh failed");
            harness.get_by_label("5h: 25.0% used · resets 2026-09-13 12:00 UTC");
        } else {
            harness.get_by_label("Codex quota unavailable · refresh failed");
        }
    }
}
