use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use gui::model::{
    subscription_quota::{SubscriptionQuotaSnapshot, SubscriptionQuotaWindow},
    telemetry::quota::QuotaState,
};
use providers::provider::codex::quota::QuotaError;

fn snapshot(remaining: f64) -> SubscriptionQuotaSnapshot {
    SubscriptionQuotaSnapshot {
        windows: vec![
            SubscriptionQuotaWindow {
                label: "5h".into(),
                remaining_percent: Some(remaining),
                used_percent: Some(100.0 - remaining),
                resets_at: "2026-10-09 12:00 UTC".into(),
                usage: None,
            },
            SubscriptionQuotaWindow {
                label: "Extra usage".into(),
                remaining_percent: None,
                used_percent: None,
                resets_at: "unknown".into(),
                usage: Some("$5.00 used; spending cap unavailable".into()),
            },
        ],
        stale: false,
        last_error: None,
    }
}
fn leaf(remaining: f64) -> QuotaState<SubscriptionQuotaSnapshot> {
    let mut state = QuotaState::default();
    state.accept(Ok(snapshot(remaining)));
    state
}

#[test]
fn native_quota_displays_known_bars_unknown_spending_and_stale_errors() {
    let mut state = leaf(75.0);
    state.accept(Err(QuotaError::Timeout));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(900.0, 600.0))
        .build_ui(move |ui| {
            ui.ctx().global_style_mut(|style| {
                style.interaction.tooltip_delay = 0.0;
                style.interaction.show_tooltips_only_when_still = false;
            });
            ui.horizontal(|ui| {
                gui::panes::quota_footer::subscription_quota_footer(ui, "Claude", &state)
            });
        });
    harness.run();
    harness.get_by_label("Claude · 75% 5h · stale");
    let bar = harness.get_by_role(egui::accesskit::Role::ProgressIndicator);
    assert_eq!(bar.accesskit_node().numeric_value(), Some(75.0));
    bar.hover();
    harness.run_steps(3);
    harness.get_by_label("Extra usage: quota percentage unavailable · resets unknown");
    harness.get_by_label("$5.00 used; spending cap unavailable");
    harness.get_by_label("Stale · quota refresh failed");
    harness.get_by_label("Quota error: quota request timed out");
}

#[test]
fn native_quota_profile_picker_switches_remaining_values() {
    let mut state = QuotaState::default();
    state.subscriptions.insert("personal".into(), leaf(75.0));
    state.subscriptions.insert("work".into(), leaf(20.0));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(900.0, 600.0))
        .build_ui(move |ui| {
            ui.horizontal(|ui| {
                gui::panes::quota_footer::subscription_quota_footer(ui, "Cursor", &state)
            });
        });
    harness.run();
    harness
        .get_by_label("Cursor · personal · 75% 5h · Extra usage unknown")
        .click();
    harness.run();
    harness.get_by_label("work · Cursor").click();
    harness.run();
    harness.get_by_label("Cursor · work · 20% 5h · Extra usage unknown");
    assert_eq!(
        harness
            .get_by_role(egui::accesskit::Role::ProgressIndicator)
            .accesskit_node()
            .numeric_value(),
        Some(20.0)
    );
}
