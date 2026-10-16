use egui::{Label, RichText, Ui};

use crate::model::tasks::TaskRow;
use crate::model::transcript::MessageDirection;
use crate::theme::icons;
use crate::theme::tokens::{FONT_SMALL, SP_1, SP_2, palette};
use crate::theme::widgets::copy_id_button;

pub(super) fn label(
    direction: MessageDirection,
    peer_run_id: &str,
    content: &str,
    agents: &[TaskRow],
) -> String {
    let arrow = match direction {
        MessageDirection::Incoming => "←",
        MessageDirection::Outgoing => "→",
    };
    let peer = match agents
        .iter()
        .find(|row| row.run_id.to_string() == peer_run_id)
    {
        Some(row) => format!("{} ({})", row.name, row.role),
        None => crate::format::short_id(peer_run_id),
    };
    format!("{arrow} {peer}: {content}")
}

pub(super) fn show(
    ui: &mut Ui,
    direction: MessageDirection,
    peer_run_id: &str,
    content: &str,
    agents: &[TaskRow],
) {
    let (icon, color) = match direction {
        MessageDirection::Incoming => (icons::ARROW_BEND_DOWN_RIGHT, palette().SUCCESS),
        MessageDirection::Outgoing => (icons::PAPER_PLANE_RIGHT, palette().WARNING_FG),
    };
    let text = label(direction, peer_run_id, content, agents);
    ui.horizontal_wrapped(|ui| {
        ui.add_space(SP_1);
        ui.spacing_mut().item_spacing.x = SP_2;
        ui.label(RichText::new(icon).size(FONT_SMALL + 1.0).color(color));
        let response = ui.add(
            Label::new(
                RichText::new(&text)
                    .size(FONT_SMALL)
                    .color(palette().TEXT_MUTED),
            )
            .wrap(),
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &text));
        response.on_hover_text(peer_run_id);
        copy_id_button(ui, "Copy run ID", peer_run_id);
    });
}
