use egui::Ui;
use workspace_ui::ThreadRunPhase;

use crate::theme::tokens::{
    FONT_BADGE, FONT_SMALL, R_SM, SP_1, STATUS_STROKE, palette, phase_color, phase_hint,
    phase_label,
};
use crate::theme::widgets::badge;

pub fn phase_circle(ui: &mut Ui, phase: Option<ThreadRunPhase>) {
    let response = match phase {
        Some(ThreadRunPhase::Running) => {
            crate::theme::widgets::spinner_with(ui, FONT_SMALL, Some(palette().RUNNING))
        }
        Some(phase) => crate::theme::widgets::status_dot(ui, phase_color(phase)),
        None => crate::theme::widgets::status_dot(ui, palette().TEXT_MUTED),
    };
    response.on_hover_text(phase.map_or("Idle", phase_hint));
}

pub fn phase_indicator_with_ack(ui: &mut Ui, phase: ThreadRunPhase, unread: bool) {
    let label = phase_label(phase);
    ui.horizontal(|ui| match phase {
        ThreadRunPhase::Running => phase_circle(ui, Some(phase)),
        ThreadRunPhase::Pending => {
            badge(ui, label, phase_color(phase), palette().SURFACE_RAISED);
        }
        ThreadRunPhase::Stopped
        | ThreadRunPhase::Waiting
        | ThreadRunPhase::Done
        | ThreadRunPhase::Error => {
            let accent = phase_color(phase);
            egui::Frame::new()
                .fill(if unread {
                    accent
                } else {
                    egui::Color32::TRANSPARENT
                })
                .stroke(egui::Stroke::new(STATUS_STROKE, accent))
                .corner_radius(egui::CornerRadius::same(R_SM))
                .inner_margin(egui::Margin::symmetric(SP_1 as i8, 0))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(label)
                            .size(FONT_BADGE)
                            .color(if unread { palette().CANVAS } else { accent }),
                    )
                    .on_hover_text(phase_hint(phase));
                });
        }
    });
}
