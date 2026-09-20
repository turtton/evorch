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
        });
    });
}

pub(super) fn status_strip(ui: &mut egui::Ui, ctx: &ConversationContext<'_>) {
    use crate::theme::text::muted;
    let metrics = ctx.thread_metrics.unwrap_or_default();
    let seconds = metrics.wall_time.as_secs();
    let wall = if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{}m", seconds / 3600, seconds % 3600 / 60)
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        crate::panes::phase_indicator::phase_circle(ui, ctx.phase);
        for segment in [
            metrics
                .cost
                .map_or_else(|| "$—".into(), |cost| format!("${cost:.3}")),
            metrics
                .cache_hit_rate
                .map_or_else(|| "cache —".into(), |rate| format!("cache {rate:.0}%")),
            metrics.ttft.map_or_else(
                || "TTFT —".into(),
                |ttft| format!("TTFT {}ms", ttft.as_millis()),
            ),
            metrics
                .tok_s
                .map_or_else(|| "— tok/s".into(), |rate| format!("{rate:.1} tok/s")),
            metrics
                .context_pressure
                .map_or_else(|| "ctx —".into(), |pressure| format!("ctx {pressure}%")),
            format!("wall {wall}"),
        ] {
            ui.label(muted("·"));
            ui.add(egui::Label::new(muted(segment)).extend());
        }
    });
}
