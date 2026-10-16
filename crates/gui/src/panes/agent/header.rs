use super::{AgentIdentity, AgentPaneAction, ConversationContext};
use crate::format::{duration, usd};
use crate::panes::agents::AgentsAction;
use crate::theme::{
    icons,
    text::{h3, muted},
    tokens::*,
    widgets::{copy_id_button, fill_label, ghost, icon_text, metric, metric_detailed},
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
                        (Some(name), Some(role)) => format!("{name} / {role}"),
                        (Some(label), None) | (None, Some(label)) => label.to_owned(),
                        (None, None) => identity.run_id.to_owned(),
                    };
                    ui.label(h3(label));
                    copy_id_button(ui, "Copy run ID", identity.run_id);
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
                    if info.on_hover_text("Run diagnostics").clicked() {
                        *action = Some(AgentPaneAction::OpenDiagnostics);
                    }
                    let context = ui.add(
                        ghost(icon_text(icons::STACK).color(palette().TEXT_MUTED))
                            .min_size(egui::vec2(ROW_DENSE - SP_1, ROW_DENSE - SP_1)),
                    );
                    context.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Agent context")
                    });
                    if context
                        .on_hover_text("Context this thread's run received")
                        .clicked()
                    {
                        *action = Some(AgentPaneAction::OpenContext);
                    }
                    let response = fill_label(ui, h3(title), ROW_DENSE, egui::Sense::hover());
                    let label = format!("Thread: {title}");
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label)
                    });
                });
                if let Some(cost) = ctx.thread_metrics.and_then(|metrics| metrics.cost) {
                    ui.label(muted(format!("Total cost (incl. subagents) {}", usd(cost))))
                        .on_hover_text("Main agent and all of its subagents in this thread");
                }
            }
            if identity.is_none() && (ctx.parent_thread.is_some() || !ctx.child_threads.is_empty())
            {
                ui.horizontal_wrapped(|ui| {
                    if let Some(parent) = ctx.parent_thread
                        && ui
                            .small_button(format!("← Parent thread: {}", parent.title))
                            .clicked()
                    {
                        *action = Some(AgentPaneAction::Sidebar(
                            crate::panes::sidebar::SidebarAction::SwitchThread(parent.id.clone()),
                        ));
                    }
                    for child in &ctx.child_threads {
                        if ui
                            .small_button(format!("↳ Child thread: {}", child.title))
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

/// `agent_focused` marks a focused subagent, whose cost includes its own
/// subagents; the thread view shows the main agent's runs only.
pub(super) fn status_strip(ui: &mut egui::Ui, ctx: &ConversationContext<'_>, agent_focused: bool) {
    let metrics = ctx.thread_metrics.unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = SP_3;
        crate::panes::phase_indicator::phase_circle(ui, ctx.phase);
        let cost = metrics.conversation_cost.map_or_else(|| "$—".into(), usd);
        let scope = if agent_focused {
            "This agent's cost (incl. its subagents)"
        } else {
            "Main agent cost (excludes subagents)"
        };
        metric_detailed(ui, &cost, &format!("{scope}: {cost}"), false);
        metric_detailed(
            ui,
            &metrics.cache_label(),
            &metrics.cache_tooltip(),
            metrics.cache_reuse.latest_is_low(),
        );
        for segment in [
            metrics.ttft_label(),
            metrics.tok_s_label(),
            metrics.context_label(),
            format!("wall {}", duration(metrics.wall_time)),
        ] {
            metric(ui, &segment);
        }
    });
}
