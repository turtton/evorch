//! Row and chip widgets the Routing and Agent Roles sections share.

use crate::theme::{tokens::*, widgets::badge};
use egui::{Align, CornerRadius, Layout, Margin, Rect, Sense, Stroke, UiBuilder, vec2};

/// A row of `height` that is one click target, with `left` drawn from its left edge and
/// `right` from its right edge. The target is allocated first, so the controls `right` and
/// `left` add are on top of it and keep their own clicks; what `left` draws that does not fit
/// beside `right` is clipped.
pub(super) fn click_row(
    ui: &mut egui::Ui,
    height: f32,
    selected: bool,
    left: impl FnOnce(&mut egui::Ui),
    right: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    if selected || response.hovered() {
        let fill = if selected {
            palette().ACTIVE_ROW
        } else {
            palette().HOVER_ROW
        };
        ui.painter()
            .rect_filled(rect, CornerRadius::same(R_SM), fill);
    }
    let clip = rect.intersect(ui.clip_rect());
    let inner = rect.shrink2(vec2(SP_2, 0.0));
    let mut right_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::right_to_left(Align::Center)),
    );
    right_ui.set_clip_rect(clip);
    right_ui.spacing_mut().item_spacing = vec2(SP_1, 0.0);
    right(&mut right_ui);
    let used = right_ui.min_rect();
    let edge = if used.width() > 0.0 {
        used.min.x - SP_2
    } else {
        inner.max.x
    };
    let left_rect = Rect::from_min_max(inner.min, egui::pos2(edge.max(inner.min.x), inner.max.y));
    let mut left_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(left_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    left_ui.set_clip_rect(left_rect.intersect(clip));
    left_ui.spacing_mut().item_spacing = vec2(SP_2, 0.0);
    left(&mut left_ui);
    response
}

/// A pill with a short fact in it.
pub(super) fn chip(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    fg: egui::Color32,
    bg: egui::Color32,
) -> egui::Response {
    badge(ui, text, fg, bg)
}

pub(super) fn muted_chip(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    chip(ui, text, palette().TEXT_MUTED, palette().SURFACE)
}

pub(super) fn warning_chip(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    chip(ui, text, palette().WARNING_FG, palette().WARNING_SURFACE)
}

pub(super) fn error_chip(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    chip(ui, text, palette().ERROR_FG, palette().ERROR_SURFACE)
}

/// A route name or a `profile · model` target, in a small box.
pub(super) fn route_chip(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    egui::Frame::new()
        .fill(palette().SURFACE)
        .stroke(Stroke::new(1.0, palette().BORDER))
        .corner_radius(CornerRadius::same(R_SM))
        .inner_margin(Margin::symmetric(6, 0))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text)
                        .monospace()
                        .size(FONT_BADGE)
                        .color(palette().TEXT),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            )
        })
        .response
}

/// A small heading over a block of a section.
pub(super) fn block_title(ui: &mut egui::Ui, text: &str) {
    ui.label(crate::theme::text::section(text.to_uppercase()));
}

/// Gives a combo box the accessible name `label`.
pub(super) fn named_combo(response: &egui::Response, label: &str) {
    let enabled = response.enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, enabled, label));
}

/// Gives a button-like response the accessible name `label`.
pub(super) fn named_row(response: &egui::Response, label: &str) {
    crate::theme::widgets::accessible(response, label);
}
