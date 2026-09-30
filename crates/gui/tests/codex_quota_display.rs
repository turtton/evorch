use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use gui::fixture::DemoSource;
use gui::model::telemetry::quota::{QuotaBackend, QuotaState};

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
    harness.run();
    harness
}

#[test]
fn frame_loop_polls_injected_source_and_displays_quota() {
    let harness = workbench();
    assert!(harness.has_label("Codex · 75% 5h · 40% wk"));
}

#[test]
#[ignore = "writes PNG evidence using an offscreen GPU adapter"]
fn capture_codex_quota_png_evidence() {
    for (name, calls) in [("codex-quota", 0), ("codex-quota-stale", 1)] {
        let mut harness = workbench_with_calls(calls);
        let label = if calls == 1 {
            "Codex · 75% 5h · 40% wk · stale"
        } else {
            "Codex · 75% 5h · 40% wk"
        };
        assert!(harness.has_label(label));
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

fn quota_harness(state: QuotaState) -> Harness<'static> {
    Harness::builder()
        .with_size(egui::vec2(900.0, 400.0))
        .build_ui(move |ui| {
            ui.ctx().global_style_mut(|style| {
                style.interaction.tooltip_delay = 0.0;
                style.interaction.show_tooltips_only_when_still = false;
            });
            gui::panes::quota_footer::quota_footer(ui, &state);
        })
}

#[test]
fn quota_snapshot_renders_compact_remaining_windows_with_hover_details() {
    let mut state = QuotaState::default();
    state.accept(Ok(snapshot(false)));
    let mut harness = quota_harness(state);
    harness.run();
    harness.get_by_label("Codex · 75% 5h · 40% wk").hover();
    harness.run_steps(3);
    harness.get_by_label("Codex · plus · remaining quota");
    harness.get_by_label("5h: 75.0% remaining · 25.0% used · resets 2026-09-13 12:00 UTC");
    harness.get_by_label("wk: 40.0% remaining · 60.0% used · resets 2026-09-13 12:00 UTC");
}

#[test]
fn stale_indicator_keeps_cached_remaining_quota_and_error_details() {
    let mut state = QuotaState::default();
    state.accept(Ok(snapshot(false)));
    state.accept(Err(QuotaError::Timeout));
    let mut harness = quota_harness(state);
    harness.run();
    harness
        .get_by_label("Codex · 75% 5h · 40% wk · stale")
        .hover();
    harness.run_steps(3);
    harness.get_by_label("Stale · quota refresh failed");
    harness.get_by_label("Quota error: quota request timed out");
}

#[test]
fn first_failure_shows_unavailable_without_fabricated_usage() {
    let mut state = QuotaState::default();
    state.accept(Err(QuotaError::Timeout));
    let mut harness = quota_harness(state);
    harness.run();
    harness.get_by_label("Codex · unavailable").hover();
    harness.run_steps(3);
    harness.get_by_label("quota request timed out");
    assert!(harness.query_by_label("Codex · 75% 5h · 40% wk").is_none());
}

#[test]
fn reauth_required_shows_relogin_message() {
    for cached in [false, true] {
        let mut state = QuotaState::default();
        if cached {
            state.accept(Ok(snapshot(false)));
        }
        state.accept(Err(QuotaError::ReauthenticationRequired));
        let mut harness = quota_harness(state);
        harness.run();
        let label = if cached {
            "Codex · 75% 5h · 40% wk · stale"
        } else {
            "Codex · unavailable"
        };
        harness.get_by_label(label).hover();
        harness.run_steps(3);
        let error = if cached {
            "Quota error: quota authentication expired or rejected; re-login needed"
        } else {
            "quota authentication expired or rejected; re-login needed"
        };
        harness.get_by_label(error);
    }
}

#[test]
fn weekly_only_plan_labels_primary_window_by_its_duration() {
    let mut state = QuotaState::default();
    let mut weekly = snapshot(false);
    weekly.quota.primary.as_mut().unwrap().window_duration = Duration::from_secs(604800);
    weekly.quota.secondary = None;
    state.accept(Ok(weekly));
    let mut harness = quota_harness(state);
    harness.run();
    harness.get_by_label("Codex");
    harness.get_by_label("wk");
    let bar = harness.get_by_role(egui::accesskit::Role::ProgressIndicator);
    assert_eq!(bar.accesskit_node().numeric_value(), Some(75.0));
    bar.hover();
    harness.run_steps(3);
    harness.get_by_label_contains("wk: 75.0% remaining");
    assert!(harness.query_by_label("Codex · 75% 5h · 40% wk").is_none());
}
