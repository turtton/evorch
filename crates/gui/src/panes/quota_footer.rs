use crate::format;
use crate::model::telemetry::quota::{QuotaData, QuotaState};
use crate::theme::{text::muted, tokens::palette};
use chrono::{DateTime, Utc};
use providers::provider::kimi_quota::KimiQuotaSnapshot;

const HEALTHY_PERCENT: f64 = 50.0;
const LOW_PERCENT: f64 = 20.0;

/// The percentage stays in the label, tooltip and accessible value too.
pub fn remaining_fill(remaining_percent: f64) -> egui::Color32 {
    if remaining_percent > HEALTHY_PERCENT {
        palette().SUCCESS
    } else if remaining_percent >= LOW_PERCENT {
        palette().WARNING_FG
    } else {
        palette().ERROR_FG
    }
}

fn reset_text(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    resets_at.map_or_else(
        || "resets unknown".into(),
        |at| format!("resets {}", format::time_until(at, now)),
    )
}

/// How much of one subscription's quota a call draws.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    /// Service name followed by one bar per window.
    Footer,
    /// Bars only, after a profile picker that already names the service.
    Bars,
    /// Every detail line, inside the profile picker.
    Details,
}

/// Compact remaining quota, with window duration and reset details on hover.
/// `now` is the frame's wall-clock time used for relative reset times.
pub fn quota_footer(ui: &mut egui::Ui, state: &QuotaState, now: DateTime<Utc>) {
    subscriptions_footer(
        ui,
        "Codex",
        state,
        |ui, state, view| render_codex(ui, state, view, now),
        codex_summary,
    );
}

pub fn kimi_quota_footer(
    ui: &mut egui::Ui,
    state: &QuotaState<KimiQuotaSnapshot>,
    now: DateTime<Utc>,
) {
    subscriptions_footer(
        ui,
        "Kimi",
        state,
        |ui, state, view| render_kimi(ui, state, view, now),
        kimi_summary,
    );
}

fn subscriptions_footer<T: QuotaData>(
    ui: &mut egui::Ui,
    service: &str,
    state: &QuotaState<T>,
    render_quota: impl Fn(&mut egui::Ui, &QuotaState<T>, View),
    summary: fn(&QuotaState<T>) -> String,
) {
    let selection_id = ui.id().with(("selected_quota_profile", service));
    if state.subscriptions.len() <= 1 {
        let (profile, subscription) = state
            .subscriptions
            .iter()
            .next()
            .map(|(name, subscription)| (name.as_str(), subscription))
            .unwrap_or(("", state));
        ui.data_mut(|data| data.insert_temp(selection_id, profile.to_owned()));
        render_quota(ui, subscription, View::Footer);
        return;
    }
    let selected = ui
        .data(|data| data.get_temp::<String>(selection_id))
        .filter(|name| state.subscriptions.contains_key(name))
        .unwrap_or_else(|| state.subscriptions.keys().next().unwrap().clone());
    ui.data_mut(|data| data.insert_temp(selection_id, selected.clone()));
    let selected_state = &state.subscriptions[&selected];
    let label = format!("{service} · {selected} · {}", summary(selected_state));
    let picker = ui.menu_button(muted(format!("{service} · {selected}")), |ui| {
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .show(ui, |ui| {
                for (index, (profile, state)) in state.subscriptions.iter().enumerate() {
                    if index > 0 {
                        ui.separator();
                    }
                    if ui
                        .selectable_label(selected == *profile, format!("{profile} · {service}"))
                        .clicked()
                    {
                        ui.data_mut(|data| data.insert_temp(selection_id, profile.clone()));
                        ui.close();
                    }
                    ui.push_id(profile, |ui| render_quota(ui, state, View::Details));
                }
            });
    });
    picker
        .response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    render_quota(ui, selected_state, View::Bars);
}

fn codex_summary(state: &QuotaState) -> String {
    let Some(snapshot) = &state.snapshot else {
        return status(state);
    };
    let parts: Vec<_> = [&snapshot.quota.primary, &snapshot.quota.secondary]
        .into_iter()
        .flatten()
        .map(|window| {
            let duration = if window.window_duration.as_secs() == 7 * 24 * 60 * 60 {
                "wk".to_owned()
            } else {
                window.duration_label()
            };
            format!(
                "{:.0}% {duration}",
                window.remaining_percent.clamp(0.0, 100.0)
            )
        })
        .collect();
    summary_with_stale(parts.join(" · "), snapshot.stale)
}

fn kimi_summary(state: &QuotaState<KimiQuotaSnapshot>) -> String {
    let Some(snapshot) = &state.snapshot else {
        return status(state);
    };
    let parts: Vec<_> = snapshot
        .windows
        .iter()
        .map(|window| {
            format!(
                "{:.0}% {}",
                window.remaining_percent.clamp(0.0, 100.0),
                window.label
            )
        })
        .collect();
    summary_with_stale(parts.join(" · "), snapshot.stale)
}

fn summary_with_stale(usage: String, stale: bool) -> String {
    let usage = if usage.is_empty() {
        "unavailable".to_owned()
    } else {
        usage
    };
    if stale {
        format!("{usage} · stale")
    } else {
        usage
    }
}

fn status<T: QuotaData>(state: &QuotaState<T>) -> String {
    if state.in_flight() {
        "loading…".to_owned()
    } else {
        "unavailable".to_owned()
    }
}

fn render_codex(ui: &mut egui::Ui, state: &QuotaState, view: View, now: DateTime<Utc>) {
    let Some(snapshot) = &state.snapshot else {
        unavailable(ui, "Codex", state, view);
        return;
    };
    let mut details = vec![format!(
        "Codex · {} · remaining quota",
        snapshot.quota.plan.as_deref().unwrap_or("unknown plan")
    )];
    let windows: Vec<_> = [&snapshot.quota.primary, &snapshot.quota.secondary]
        .into_iter()
        .flatten()
        .collect();
    let segments: Vec<_> = windows
        .iter()
        .map(|window| {
            let duration = if window.window_duration.as_secs() == 7 * 24 * 60 * 60 {
                "wk".to_owned()
            } else {
                window.duration_label()
            };
            details.push(format!(
                "{duration}: {:.1}% remaining · {}",
                window.remaining_percent,
                reset_text(Some(window.resets_at), now)
            ));
            (duration, window.remaining_percent)
        })
        .collect();
    if view == View::Details {
        if let Some(window) = &snapshot.quota.code_review {
            details.push(format!(
                "Code review: {:.1}% remaining · {}",
                window.remaining_percent,
                reset_text(Some(window.resets_at), now)
            ));
        }
        show_expanded(ui, details, snapshot.stale, &state.error);
        return;
    }
    render(
        ui,
        (view == View::Footer).then_some("Codex"),
        &segments,
        snapshot.stale,
        &state.error,
        details,
    );
}

fn render_kimi(
    ui: &mut egui::Ui,
    state: &QuotaState<KimiQuotaSnapshot>,
    view: View,
    now: DateTime<Utc>,
) {
    let Some(snapshot) = &state.snapshot else {
        unavailable(ui, "Kimi", state, view);
        return;
    };
    let mut details = vec!["Kimi · remaining quota".into()];
    let segments: Vec<_> = snapshot
        .windows
        .iter()
        .map(|window| {
            details.push(format!(
                "{}: {:.1}% remaining · {}",
                window.label,
                window.remaining_percent,
                reset_text(window.resets_at, now)
            ));
            (window.label.clone(), window.remaining_percent)
        })
        .collect();
    if view == View::Details {
        show_expanded(ui, details, snapshot.stale, &state.error);
        return;
    }
    render(
        ui,
        (view == View::Footer).then_some("Kimi"),
        &segments,
        snapshot.stale,
        &state.error,
        details,
    );
}

/// A quota error line in English; when the wording had to be translated the
/// original follows on a second line (hover popups cannot nest another hover).
fn quota_error_line(raw: &str) -> String {
    let shown = crate::model::failure_text::english(raw);
    if shown == raw {
        format!("Quota error: {shown}")
    } else {
        format!("Quota error: {shown}\n{raw}")
    }
}

fn unavailable<T: QuotaData>(ui: &mut egui::Ui, name: &str, state: &QuotaState<T>, view: View) {
    if view == View::Bars {
        // The profile picker already reads "<service> · <profile> · unavailable".
        return;
    }
    if view == View::Details {
        ui.label(muted(format!("{name} · {}", status(state))));
        if let Some(error) = &state.error {
            quota_error_label(ui, &error.to_string());
        }
        return;
    }
    let mut details = vec![format!("{name} quota {}", status(state))];
    details.extend(state.error.iter().map(|error| {
        let raw = error.to_string();
        let shown = crate::model::failure_text::english(&raw);
        if shown == raw {
            shown
        } else {
            format!("{shown}\n{raw}")
        }
    }));
    minimized(ui, name, &format!("{name} · {}", status(state)), &details);
}

/// Only the service name stays visible; `summary` remains its accessible name.
fn minimized(ui: &mut egui::Ui, name: &str, summary: &str, details: &[String]) {
    let response = ui.label(muted(name));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, summary));
    response.on_hover_ui(|ui| show_details(ui, details));
}

fn show_expanded(
    ui: &mut egui::Ui,
    mut details: Vec<String>,
    stale: bool,
    error: &Option<providers::provider::codex::quota::QuotaError>,
) {
    if stale {
        details.push("Stale · quota refresh failed".into());
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs(1));
    for detail in details {
        ui.label(muted(detail));
    }
    if let Some(error) = error {
        quota_error_label(ui, &error.to_string());
    }
}

/// The error as an English label with the original in its hover.
fn quota_error_label(ui: &mut egui::Ui, raw: &str) {
    let line = quota_error_line(raw);
    let response = ui.label(muted(line.lines().next().unwrap_or_default()));
    if crate::model::failure_text::english(raw) != raw {
        response.on_hover_text(raw);
    }
}

/// One bar per window, after the service `name` when it is not already shown.
fn render(
    ui: &mut egui::Ui,
    name: Option<&str>,
    segments: &[(String, f64)],
    stale: bool,
    error: &Option<providers::provider::codex::quota::QuotaError>,
    mut details: Vec<String>,
) {
    if stale {
        details.push("Stale · quota refresh failed".into());
    }
    if let Some(error) = error {
        details.push(quota_error_line(&error.to_string()));
    }
    let suffix = if stale { " · stale" } else { "" };
    if let Some(name) = name {
        // Bars carry no text, so the title keeps the textual summary as its
        // accessible name; the visible footer stays compact.
        let parts: Vec<_> = segments
            .iter()
            .map(|(label, remaining)| format!("{:.0}% {label}", remaining.clamp(0.0, 100.0)))
            .collect();
        let summary = if parts.is_empty() {
            format!("{name} · unavailable{suffix}")
        } else {
            format!("{name} · {}{suffix}", parts.join(" · "))
        };
        let response = ui.label(muted(format!("{name}{suffix}")));
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &summary));
        response.on_hover_ui(|ui| show_details(ui, &details));
    } else if stale {
        ui.label(muted("stale"))
            .on_hover_ui(|ui| show_details(ui, &details));
    }
    for (label, remaining) in segments {
        // Keep the percentage in the tooltip/accessibility value; the footer stays compact.
        ui.label(muted(label))
            .on_hover_ui(|ui| show_details(ui, &details));
        let remaining = remaining.clamp(0.0, 100.0);
        // The clamped percentage is always finite and representable as f32.
        #[allow(clippy::cast_possible_truncation)]
        let ratio = (remaining / 100.0) as f32;
        ui.add(
            egui::ProgressBar::new(ratio)
                .fill(remaining_fill(remaining))
                .desired_width(54.0)
                .desired_height(7.0),
        )
        .on_hover_ui(|ui| show_details(ui, &details));
    }
}

fn show_details(ui: &mut egui::Ui, details: &[String]) {
    // Only a visible tooltip keeps its relative reset times ticking.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs(1));
    for detail in details {
        ui.label(detail);
    }
}

pub fn subscription_quota_footer(
    ui: &mut egui::Ui,
    service: &str,
    state: &QuotaState<crate::model::subscription_quota::SubscriptionQuotaSnapshot>,
    now: DateTime<Utc>,
) {
    // Each service has an independent profile picker and worker tree.
    subscriptions_footer(
        ui,
        service,
        state,
        |ui, state, view| render_subscription(ui, service, state, view, now),
        subscription_summary,
    );
}
fn subscription_summary(
    state: &QuotaState<crate::model::subscription_quota::SubscriptionQuotaSnapshot>,
) -> String {
    let Some(snapshot) = &state.snapshot else {
        return status(state);
    };
    summary_with_stale(
        snapshot
            .windows
            .iter()
            .map(|window| {
                window.remaining_percent.map_or_else(
                    || format!("{} unknown", window.label),
                    |p| format!("{p:.0}% {}", window.label),
                )
            })
            .collect::<Vec<_>>()
            .join(" · "),
        snapshot.stale,
    )
}
fn render_subscription(
    ui: &mut egui::Ui,
    service: &str,
    state: &QuotaState<crate::model::subscription_quota::SubscriptionQuotaSnapshot>,
    view: View,
    now: DateTime<Utc>,
) {
    let Some(snapshot) = &state.snapshot else {
        unavailable(ui, service, state, view);
        return;
    };
    let mut details = vec![format!("{service} · remaining quota")];
    let mut segments = Vec::new();
    for window in &snapshot.windows {
        let reset = reset_text(window.resets_at, now);
        match window.remaining_percent {
            Some(remaining) => {
                details.push(format!(
                    "{}: {remaining:.1}% remaining · {reset}",
                    window.label
                ));
                segments.push((window.label.clone(), remaining));
            }
            _ => details.push(format!(
                "{}: quota percentage unavailable · {reset}",
                window.label
            )),
        }
        if let Some(usage) = &window.usage {
            details.push(usage.clone());
        }
    }
    if view == View::Details {
        show_expanded(ui, details, snapshot.stale, &state.error);
        return;
    }
    if segments.is_empty() && view == View::Footer {
        minimized(
            ui,
            service,
            &format!("{service} · percentage unavailable"),
            &details,
        );
    }
    render(
        ui,
        (view == View::Footer && !segments.is_empty()).then_some(service),
        &segments,
        snapshot.stale,
        &state.error,
        details,
    );
}

#[cfg(test)]
mod tests {
    // Given: a quota error whose wording is Japanese, and one in English
    // When: formatted for the footer / Then: English first, the original kept only when translated
    #[test]
    fn quota_error_lines_translate_and_keep_the_original() {
        assert_eq!(
            super::quota_error_line("HTTP error"),
            "Quota error: HTTP error"
        );
        let raw = "モデル呼び出しに失敗しました: HTTP 500";
        assert_eq!(
            super::quota_error_line(raw),
            format!("Quota error: Model call failed: HTTP 500\n{raw}")
        );
    }

    use super::*;

    #[test]
    fn bar_fill_tracks_remaining_quota_thresholds() {
        let p = palette();
        for (remaining, expected) in [
            (100.0, p.SUCCESS),
            (50.1, p.SUCCESS),
            (50.0, p.WARNING_FG),
            (20.0, p.WARNING_FG),
            (19.9, p.ERROR_FG),
            (0.0, p.ERROR_FG),
        ] {
            assert_eq!(remaining_fill(remaining), expected, "{remaining}%");
        }
    }

    #[test]
    fn reset_time_reads_as_remaining_time() {
        let now: DateTime<Utc> = "2026-09-12T06:30:00Z".parse().unwrap();
        let reset = "2026-09-13T12:00:00Z".parse().ok();
        assert_eq!(reset_text(reset, now), "resets in 1d 5h 30m");
        assert_eq!(reset_text(Some(now), now), "resets now");
        assert_eq!(reset_text(None, now), "resets unknown");
    }
}
