use crate::model::telemetry::quota::QuotaState;
use crate::theme::text::muted;

/// Compact remaining quota, with window duration and reset details on hover.
pub fn quota_footer(ui: &mut egui::Ui, state: &QuotaState) {
    let Some(snapshot) = &state.snapshot else {
        let label = if state.in_flight() {
            "Codex · loading…"
        } else {
            "Codex · unavailable"
        };
        let response = ui.label(muted(label));
        if let Some(error) = &state.error {
            response.on_hover_text(error.to_string());
        }
        return;
    };
    let mut segments = Vec::new();
    let mut details = vec![format!(
        "Codex · {} · remaining quota",
        snapshot.quota.plan.as_deref().unwrap_or("unknown plan")
    )];
    for window in [&snapshot.quota.primary, &snapshot.quota.secondary]
        .into_iter()
        .flatten()
    {
        let duration = if window.window_duration.as_secs() == 7 * 24 * 60 * 60 {
            "wk".to_owned()
        } else {
            window.duration_label()
        };
        segments.push(format!(
            "{:.0}% {duration}",
            window.remaining_percent.clamp(0.0, 100.0)
        ));
        details.push(format!(
            "{duration}: {:.1}% remaining · {:.1}% used · resets {}",
            window.remaining_percent,
            window.used_percent,
            window.resets_at.format("%Y-%m-%d %H:%M UTC")
        ));
    }
    if segments.is_empty() {
        segments.push("unavailable".into());
    }
    if snapshot.stale {
        details.push("Stale · quota refresh failed".into());
    }
    if let Some(error) = &state.error {
        details.push(format!("Quota error: {error}"));
    }
    let suffix = if snapshot.stale { " · stale" } else { "" };
    ui.label(muted(format!("Codex · {}{suffix}", segments.join(" · "))))
        .on_hover_ui(|ui| {
            for detail in details {
                ui.label(detail);
            }
        });
}
