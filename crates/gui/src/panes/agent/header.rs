use super::{AgentIdentity, AgentPaneAction, ConversationContext};
use crate::panes::agents::AgentsAction;
use crate::theme::{
    icons,
    text::{h3, muted},
    tokens::*,
    widgets::{fill_label, ghost, icon_text, metric},
};

pub(super) fn header_strip(
    ui: &mut egui::Ui,
    identity: &Option<AgentIdentity<'_>>,
    ctx: &ConversationContext<'_>,
    action: &mut Option<AgentPaneAction>,
) {
    if identity.is_none() && ctx.active_thread_title.is_none() {
        return;
    }
    // The header sits on the canvas; a hairline below separates it from the
    // transcript instead of a bordered box.
    let header = egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(SP_1 as i8, SP_1 as i8))
        .show(ui, |ui| {
            if let Some(identity) = identity {
                ui.horizontal_wrapped(|ui| {
                    ui.set_min_height(ROW_COMPACT - 2.0 * SP_2);
                    let back = ui.add(ghost(icons::with_icon(icons::ARROW_LEFT, "Thread")));
                    back.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "← Thread")
                    });
                    if back.clicked() {
                        *action = Some(AgentPaneAction::Agents(AgentsAction::ReturnToThread));
                    }
                    let label = match (identity.name, identity.role) {
                        (Some(name), Some(role)) => {
                            format!("{} / {name} / {role}", identity.run_id)
                        }
                        (Some(name), None) => format!("{} / {name}", identity.run_id),
                        (None, Some(role)) => format!("{} / {role}", identity.run_id),
                        (None, None) => identity.run_id.to_owned(),
                    };
                    ui.label(h3(label));
                });
            } else if let Some(title) = ctx.active_thread_title {
                ui.horizontal(|ui| {
                    ui.set_min_height(ROW_COMPACT - 2.0 * SP_2);
                    let info = ui.add(
                        ghost(icon_text(icons::INFO).color(palette().TEXT_MUTED))
                            .min_size(egui::vec2(ROW_DENSE - SP_1, ROW_DENSE - SP_1)),
                    );
                    info.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "ℹ")
                    });
                    if info.on_hover_text("実行の診断").clicked() {
                        *action = Some(AgentPaneAction::OpenDiagnostics);
                    }
                    let response = fill_label(ui, h3(title), ROW_DENSE, egui::Sense::hover());
                    let label = format!("Thread: {title}");
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label)
                    });
                });
                if let Some(cost) = ctx.thread_metrics.and_then(|metrics| metrics.cost) {
                    ui.label(muted(format!("Total cost ${cost:.3}")))
                        .on_hover_text("Main agent and subagents");
                }
            }
            if identity.is_none() && (ctx.parent_thread.is_some() || !ctx.child_threads.is_empty())
            {
                ui.horizontal_wrapped(|ui| {
                    if let Some(parent) = ctx.parent_thread
                        && ui
                            .small_button(format!("← 親 thread: {}", parent.title))
                            .clicked()
                    {
                        *action = Some(AgentPaneAction::Sidebar(
                            crate::panes::sidebar::SidebarAction::SwitchThread(parent.id.clone()),
                        ));
                    }
                    for child in &ctx.child_threads {
                        if ui
                            .small_button(format!("↳ 子 thread: {}", child.title))
                            .clicked()
                        {
                            *action = Some(AgentPaneAction::Sidebar(
                                crate::panes::sidebar::SidebarAction::SwitchThread(
                                    child.id.clone(),
                                ),
                            ));
                        }
                    }
                });
            }
        });
    let rect = header.response.rect;
    ui.painter().hline(
        rect.x_range(),
        rect.bottom(),
        egui::Stroke::new(1.0, palette().BORDER),
    );
    ui.add_space(SP_1);
}

pub(super) fn status_strip(ui: &mut egui::Ui, ctx: &ConversationContext<'_>) {
    let metrics = ctx.thread_metrics.unwrap_or_default();
    let seconds = metrics.wall_time.as_secs();
    let wall = if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{}m", seconds / 3600, seconds % 3600 / 60)
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = SP_3;
        crate::panes::phase_indicator::phase_circle(ui, ctx.phase);
        for segment in [
            metrics
                .conversation_cost
                .map_or_else(|| "$—".into(), |cost| format!("${cost:.3}")),
            metrics.cache_hit_rate_label(),
            metrics.ttft_label(),
            metrics.tok_s_label(),
            metrics.context_label(),
            format!("wall {wall}"),
        ] {
            metric(ui, &segment);
        }
    });
}
