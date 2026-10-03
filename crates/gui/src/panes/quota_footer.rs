use crate::model::telemetry::quota::{QuotaData, QuotaState};
use crate::theme::text::muted;
use providers::provider::kimi_quota::KimiQuotaSnapshot;

/// Compact remaining quota, with window duration and reset details on hover.
pub fn quota_footer(ui: &mut egui::Ui, state: &QuotaState) {
    subscriptions_footer(ui, "Codex", state, render_codex, codex_summary);
}

pub fn kimi_quota_footer(ui: &mut egui::Ui, state: &QuotaState<KimiQuotaSnapshot>) {
    subscriptions_footer(ui, "Kimi", state, render_kimi, kimi_summary);
}

fn subscriptions_footer<T: QuotaData>(
    ui: &mut egui::Ui,
    service: &str,
    state: &QuotaState<T>,
    render_quota: fn(&mut egui::Ui, &QuotaState<T>, bool),
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
        render_quota(ui, subscription, false);
        return;
    }
    let selected = ui
        .data(|data| data.get_temp::<String>(selection_id))
        .filter(|name| state.subscriptions.contains_key(name))
        .unwrap_or_else(|| state.subscriptions.keys().next().unwrap().clone());
    ui.data_mut(|data| data.insert_temp(selection_id, selected.clone()));
    let selected_state = &state.subscriptions[&selected];
    ui.menu_button(
        muted(format!(
            "{service} · {selected} · {}",
            summary(selected_state)
        )),
        |ui| {
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    for (index, (profile, state)) in state.subscriptions.iter().enumerate() {
                        if index > 0 {
                            ui.separator();
                        }
                        if ui
                            .selectable_label(
                                selected == *profile,
                                format!("{profile} · {service}"),
                            )
                            .clicked()
                        {
                            ui.data_mut(|data| data.insert_temp(selection_id, profile.clone()));
                            ui.close();
                        }
                        ui.push_id(profile, |ui| render_quota(ui, state, true));
                    }
                });
        },
    );
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

fn render_codex(ui: &mut egui::Ui, state: &QuotaState, expanded: bool) {
    let Some(snapshot) = &state.snapshot else {
        unavailable(ui, "Codex", state, expanded);
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
    let use_bars = !windows
        .iter()
        .any(|window| window.window_duration.as_secs() == 5 * 3600);
    let segments: Vec<_> = windows
        .iter()
        .map(|window| {
            let duration = if window.window_duration.as_secs() == 7 * 24 * 60 * 60 {
                "wk".to_owned()
            } else {
                window.duration_label()
            };
            details.push(format!(
                "{duration}: {:.1}% remaining · {:.1}% used · resets {}",
                window.remaining_percent,
                window.used_percent,
                window.resets_at.format("%Y-%m-%d %H:%M UTC")
            ));
            (duration, window.remaining_percent)
        })
        .collect();
    if expanded {
        if let Some(window) = &snapshot.quota.code_review {
            details.push(format!(
                "Code review: {:.1}% remaining · {:.1}% used · resets {}",
                window.remaining_percent,
                window.used_percent,
                window.resets_at.format("%Y-%m-%d %H:%M UTC")
            ));
        }
        show_expanded(ui, details, snapshot.stale, &state.error);
        return;
    }
    render(
        ui,
        "Codex",
        &segments,
        use_bars,
        snapshot.stale,
        &state.error,
        details,
    );
}

fn render_kimi(ui: &mut egui::Ui, state: &QuotaState<KimiQuotaSnapshot>, expanded: bool) {
    let Some(snapshot) = &state.snapshot else {
        unavailable(ui, "Kimi", state, expanded);
        return;
    };
    let mut details = vec!["Kimi · remaining quota".into()];
    let segments: Vec<_> = snapshot
        .windows
        .iter()
        .map(|window| {
            let reset = window.resets_at.map_or_else(
                || "unknown".into(),
                |at| at.format("%Y-%m-%d %H:%M UTC").to_string(),
            );
            details.push(format!(
                "{}: {:.1}% remaining · {:.1}% used · resets {reset}",
                window.label, window.remaining_percent, window.used_percent
            ));
            (window.label.clone(), window.remaining_percent)
        })
        .collect();
    if expanded {
        show_expanded(ui, details, snapshot.stale, &state.error);
        return;
    }
    render(
        ui,
        "Kimi",
        &segments,
        true,
        snapshot.stale,
        &state.error,
        details,
    );
}

fn unavailable<T: QuotaData>(ui: &mut egui::Ui, name: &str, state: &QuotaState<T>, expanded: bool) {
    let response = ui.label(muted(format!("{name} · {}", status(state))));
    if let Some(error) = &state.error {
        if expanded {
            ui.label(muted(format!("Quota error: {error}")));
        } else {
            response.on_hover_text(error.to_string());
        }
    }
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
    if let Some(error) = error {
        details.push(format!("Quota error: {error}"));
    }
    for detail in details {
        ui.label(muted(detail));
    }
}

fn render(
    ui: &mut egui::Ui,
    name: &str,
    segments: &[(String, f64)],
    use_bars: bool,
    stale: bool,
    error: &Option<providers::provider::codex::quota::QuotaError>,
    mut details: Vec<String>,
) {
    if stale {
        details.push("Stale · quota refresh failed".into());
    }
    if let Some(error) = error {
        details.push(format!("Quota error: {error}"));
    }
    let suffix = if stale { " · stale" } else { "" };
    if use_bars && !segments.is_empty() {
        ui.label(muted(format!("{name}{suffix}")))
            .on_hover_ui(|ui| show_details(ui, &details));
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
                    .desired_width(54.0)
                    .desired_height(7.0),
            )
            .on_hover_ui(|ui| show_details(ui, &details));
        }
    } else {
        let parts: Vec<_> = segments
            .iter()
            .map(|(label, remaining)| format!("{:.0}% {label}", remaining.clamp(0.0, 100.0)))
            .collect();
        let joined = if parts.is_empty() {
            "unavailable".into()
        } else {
            parts.join(" · ")
        };
        ui.label(muted(format!("{name} · {joined}{suffix}")))
            .on_hover_ui(|ui| show_details(ui, &details));
    }
}

fn show_details(ui: &mut egui::Ui, details: &[String]) {
    for detail in details {
        ui.label(detail);
    }
}
