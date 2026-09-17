use super::{AgentIdentity, AgentPaneAction, ConversationContext};
use crate::panes::agents::AgentsAction;
use crate::theme::{text::h3, tokens::*, widgets::surface_frame};

pub(super) fn header_strip(
    ui: &mut egui::Ui,
    identity: &Option<AgentIdentity<'_>>,
    ctx: &ConversationContext<'_>,
    action: &mut Option<AgentPaneAction>,
) {
    if identity.is_none() && ctx.active_thread_title.is_none() {
        return;
    }
    surface_frame(palette().SURFACE).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.set_min_height(ROW_COMPACT - 2.0 * SP_2);
            if let Some(identity) = identity {
                let label = match (identity.name, identity.role) {
                    (Some(name), Some(role)) => format!("{} / {name} / {role}", identity.run_id),
                    (Some(name), None) => format!("{} / {name}", identity.run_id),
                    (None, Some(role)) => format!("{} / {role}", identity.run_id),
                    (None, None) => identity.run_id.to_owned(),
                };
                ui.label(h3(label));
                if ui.button("← Thread").clicked() {
                    *action = Some(AgentPaneAction::Agents(AgentsAction::ReturnToThread));
                }
            } else if let Some(title) = ctx.active_thread_title {
                ui.label(h3(format!("Thread: {title}")));
            }
            if let Some(metrics) = &ctx.thread_metrics {
                let mut segments = Vec::new();
                if let Some(cost) = metrics.cost {
                    segments.push(format!("${cost:.3}"));
                }
                if let Some(rate) = metrics.cache_hit_rate {
                    segments.push(format!("cache {rate:.0}%"));
                }
                if let Some(pressure) = metrics.context_pressure {
                    segments.push(format!("ctx {pressure}%"));
                }
                let seconds = metrics.wall_time.as_secs();
                if seconds > 0 {
                    segments.push(if seconds < 60 {
                        format!("{seconds}s")
                    } else if seconds < 3600 {
                        format!("{}m", seconds / 60)
                    } else {
                        format!("{}h{}m", seconds / 3600, seconds % 3600 / 60)
                    });
                }
                if !segments.is_empty() {
                    ui.label(crate::theme::text::muted(segments.join(" · ")));
                }
            }
            if let Some(phase) = ctx.phase {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    crate::panes::phase_indicator::phase_indicator_with_ack(
                        ui,
                        phase,
                        ctx.phase_unread,
                    );
                });
            }
        });
    });
}
