use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use gui::model::{
    kimi_quota::KimiQuotaState,
    provider_settings::ProviderSettingsModel,
    telemetry::quota::{QuotaBackend, QuotaState},
};
use providers::provider::{
    codex::quota::QuotaError,
    kimi_quota::{KimiQuotaSnapshot, KimiQuotaWindow},
};
use std::time::{Duration, Instant};

fn snapshot() -> KimiQuotaSnapshot {
    let now = "2026-10-01T12:00:00Z".parse().unwrap();
    KimiQuotaSnapshot {
        windows: vec![KimiQuotaWindow {
            label: "wk".into(),
            used_percent: 25.0,
            remaining_percent: 75.0,
            resets_at: Some(now),
        }],
        stale: false,
        fetched_at: now,
        last_error: None,
    }
}

#[test]
fn kimi_footer_uses_bars_and_preserves_last_good_quota_on_failure() {
    let mut state = QuotaState::default();
    state.accept(Ok(snapshot()));
    state.accept(Err(QuotaError::Timeout));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(600.0, 400.0))
        .build_ui(move |ui| {
            ui.ctx().global_style_mut(|style| {
                style.interaction.tooltip_delay = 0.0;
                style.interaction.show_tooltips_only_when_still = false;
            });
            ui.horizontal(|ui| gui::panes::quota_footer::kimi_quota_footer(ui, &state));
        });
    harness.run();
    harness.get_by_label("Kimi · stale");
    let bar = harness.get_by_role(egui::accesskit::Role::ProgressIndicator);
    assert_eq!(bar.accesskit_node().numeric_value(), Some(75.0));
    bar.hover();
    harness.run_steps(3);
    harness.get_by_label("wk: 75.0% remaining · 25.0% used · resets 2026-10-01 12:00 UTC");
    harness.get_by_label("Stale · quota refresh failed");
}

struct FakeKimi;
#[async_trait::async_trait]
impl QuotaBackend<KimiQuotaSnapshot> for FakeKimi {
    async fn fetch(&mut self) -> Result<KimiQuotaSnapshot, QuotaError> {
        Ok(snapshot())
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }
}

#[test]
fn shared_worker_polls_kimi_without_blocking() {
    let mut state = KimiQuotaState::default();
    state.state = QuotaState::with_backend(Box::new(FakeKimi));
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.state.snapshot.is_none() {
        assert!(Instant::now() < deadline);
        state.state.poll(Instant::now());
        std::thread::yield_now();
    }
}

#[test]
fn account_changes_and_removal_clear_cached_kimi_quota() {
    let mut config = config::Config::default();
    config.providers.insert(
        "kimi".into(),
        config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::KimiSubscription,
            credential: config::CredentialRefConfig::Keyring {
                service: "evorch".into(),
                account: "first".into(),
            },
            base_url: "http://localhost:1/coding/v1".into(),
            ..Default::default()
        },
    );
    let mut state = KimiQuotaState::default();
    state.configure(&ProviderSettingsModel::seed_from_config(&config), None);
    assert!(state.configured());
    state
        .state
        .subscriptions
        .get_mut("kimi")
        .unwrap()
        .accept(Ok(snapshot()));
    config.providers.get_mut("kimi").unwrap().credential = config::CredentialRefConfig::Keyring {
        service: "evorch".into(),
        account: "second".into(),
    };
    state.configure(&ProviderSettingsModel::seed_from_config(&config), None);
    assert!(state.state.subscriptions["kimi"].snapshot.is_none());
    state
        .state
        .subscriptions
        .get_mut("kimi")
        .unwrap()
        .accept(Ok(snapshot()));
    state.configure(&ProviderSettingsModel::default(), None);
    assert!(!state.configured());
    assert!(state.state.subscriptions.is_empty());
}

fn subscriptions_config(names: &[&str]) -> config::Config {
    let mut config = config::Config::default();
    for name in names {
        config.providers.insert(
            (*name).into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::KimiSubscription,
                credential: config::CredentialRefConfig::Keyring {
                    service: "evorch".into(),
                    account: (*name).into(),
                },
                base_url: "http://localhost:1/coding/v1".into(),
                ..Default::default()
            },
        );
    }
    config
}

fn subscriptions_harness(state: KimiQuotaState) -> Harness<'static> {
    Harness::builder()
        .with_size(egui::vec2(900.0, 600.0))
        .build_ui(move |ui| {
            ui.horizontal(|ui| gui::panes::quota_footer::kimi_quota_footer(ui, &state.state));
        })
}

#[test]
fn multiple_kimi_subscriptions_popup_shows_each_profile_quota_and_errors() {
    let config = subscriptions_config(&["personal", "work", "unavailable"]);
    let mut state = KimiQuotaState::default();
    state.configure(&ProviderSettingsModel::seed_from_config(&config), None);
    assert_eq!(state.state.subscriptions.len(), 3);
    state
        .state
        .subscriptions
        .get_mut("personal")
        .unwrap()
        .accept(Ok(snapshot()));
    let mut work = snapshot();
    work.windows[0].remaining_percent = 20.0;
    work.windows[0].used_percent = 80.0;
    let work_state = state.state.subscriptions.get_mut("work").unwrap();
    work_state.accept(Ok(work));
    work_state.accept(Err(QuotaError::Timeout));
    state
        .state
        .subscriptions
        .get_mut("unavailable")
        .unwrap()
        .accept(Err(QuotaError::Credentials));

    let mut harness = subscriptions_harness(state);
    harness.run();
    assert!(harness.query_by_label("personal · Kimi").is_none());
    harness.get_by_label("Kimi · 3 subscriptions").click();
    harness.run();
    for profile in ["personal", "work", "unavailable"] {
        harness.get_by_label(&format!("{profile} · Kimi"));
    }
    harness.get_by_label("wk: 75.0% remaining · 25.0% used · resets 2026-10-01 12:00 UTC");
    harness.get_by_label("wk: 20.0% remaining · 80.0% used · resets 2026-10-01 12:00 UTC");
    harness.get_by_label("Stale · quota refresh failed");
    harness.get_by_label("Quota error: quota request timed out");
    harness.get_by_label("Kimi · unavailable");
    harness.get_by_label(&format!("Quota error: {}", QuotaError::Credentials));
    harness.get_by_label("Kimi · 3 subscriptions").click();
    harness.run();
    assert!(harness.query_by_label("personal · Kimi").is_none());
}

#[test]
fn one_configured_kimi_subscription_keeps_the_compact_footer() {
    let mut state = KimiQuotaState::default();
    state.configure(
        &ProviderSettingsModel::seed_from_config(&subscriptions_config(&["personal"])),
        None,
    );
    state
        .state
        .subscriptions
        .get_mut("personal")
        .unwrap()
        .accept(Ok(snapshot()));
    let mut harness = subscriptions_harness(state);
    harness.run();
    harness.get_by_label("Kimi");
    assert_eq!(
        harness
            .get_by_role(egui::accesskit::Role::ProgressIndicator)
            .accesskit_node()
            .numeric_value(),
        Some(75.0)
    );
    assert!(
        harness
            .query_by_role(egui::accesskit::Role::Button)
            .is_none()
    );
    assert!(harness.query_by_label("personal · Kimi").is_none());
}

#[test]
fn reconfiguring_one_kimi_subscription_preserves_the_other_cached_quota() {
    let mut config = subscriptions_config(&["personal", "work"]);
    let mut state = KimiQuotaState::default();
    state.configure(&ProviderSettingsModel::seed_from_config(&config), None);
    for subscription in state.state.subscriptions.values_mut() {
        subscription.accept(Ok(snapshot()));
    }
    config.providers.get_mut("personal").unwrap().base_url = "http://localhost:2/coding/v1".into();
    state.configure(&ProviderSettingsModel::seed_from_config(&config), None);
    assert!(state.state.subscriptions["personal"].snapshot.is_none());
    assert!(state.state.subscriptions["work"].snapshot.is_some());
    config.providers.remove("personal");
    state.configure(&ProviderSettingsModel::seed_from_config(&config), None);
    assert_eq!(state.state.subscriptions.len(), 1);
    assert!(state.state.subscriptions["work"].snapshot.is_some());
}

#[test]
fn shared_worker_polls_all_registered_kimi_subscriptions() {
    let mut state = KimiQuotaState::default();
    state.configure(
        &ProviderSettingsModel::seed_from_config(&subscriptions_config(&["personal", "work"])),
        None,
    );
    for subscription in state.state.subscriptions.values_mut() {
        *subscription = QuotaState::with_backend(Box::new(FakeKimi));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while state
        .state
        .subscriptions
        .values()
        .any(|subscription| subscription.snapshot.is_none())
    {
        assert!(Instant::now() < deadline);
        state.state.poll(Instant::now());
        std::thread::yield_now();
    }
    assert_eq!(state.state.subscriptions.len(), 2);
}
