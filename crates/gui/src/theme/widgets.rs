use egui::{
    Align, Button, Color32, CornerRadius, Frame, Layout, Margin, Response, RichText, Sense, Stroke,
    Ui, UiBuilder, WidgetInfo, WidgetType,
};

use super::text::{h3, muted};
use super::tokens::*;

pub fn pane_root<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let out = ui.scope_builder(
        UiBuilder::new()
            .id_salt(("pane-root", title))
            .sense(Sense::hover()),
        add,
    );
    out.response
        .widget_info(|| WidgetInfo::labeled(WidgetType::Panel, true, title));
    out.inner
}

pub fn surface_frame(fill: Color32) -> Frame {
    Frame::new()
        .fill(fill)
        .inner_margin(Margin::same(SP_3 as i8))
        .corner_radius(CornerRadius::same(R_MD))
        .stroke(Stroke::new(1.0, BORDER))
}

pub fn card(ui: &mut Ui, accent: Color32, add: impl FnOnce(&mut Ui)) {
    surface_frame(SURFACE).show(ui, |ui| {
        let left_top = ui.available_rect_before_wrap().left_top();
        let content = Frame::new()
            .inner_margin(Margin {
                left: SP_2 as i8,
                ..Margin::ZERO
            })
            .show(ui, add);
        let bar =
            egui::Rect::from_min_size(left_top, egui::vec2(2.0, content.response.rect.height()));
        ui.painter().rect_filled(bar, CornerRadius::ZERO, accent);
    });
}

pub fn status_dot(ui: &mut Ui, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(DOT_SIZE, DOT_SIZE), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), DOT_SIZE / 2.0, color);
    response
}

pub fn badge(ui: &mut Ui, text: impl Into<String>, fg: Color32, bg: Color32) -> Response {
    Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(R_PILL))
        .inner_margin(Margin::symmetric(SP_1 as i8, 0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(FONT_BADGE).color(fg))
        })
        .response
}

pub fn primary_button(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(
        Button::new(RichText::new(text).color(ACCENT_FG))
            .fill(ACCENT)
            .corner_radius(CornerRadius::same(R_SM)),
    )
}

pub fn empty_state(ui: &mut Ui, title: &str, hint: &str, cta: Option<&str>) -> bool {
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space(SP_4);
        ui.label(h3(title));
        ui.label(muted(hint));
        if let Some(text) = cta {
            ui.add_space(SP_2);
            if primary_button(ui, text).clicked() {
                clicked = true;
            }
        }
    });
    clicked
}

pub fn compact_row<R>(ui: &mut Ui, selected: bool, add: impl FnOnce(&mut Ui) -> R) -> Response {
    let fill = if selected {
        SELECTED_ROW
    } else {
        ui.visuals().faint_bg_color
    };
    let mut prepared = Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(R_SM))
        .inner_margin(Margin::symmetric(SP_2 as i8, 0))
        .begin(ui);
    let response = {
        let content = &mut prepared.content_ui;
        let width = content.available_width();
        content
            .allocate_ui_with_layout(
                egui::vec2(width, ROW_DENSE),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_min_size(egui::vec2(width, ROW_DENSE));
                    add(ui)
                },
            )
            .response
    };
    if response.hovered() && !selected {
        prepared.frame.fill = HOVER_ROW;
    }
    prepared.end(ui)
}
