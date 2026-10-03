use egui::{
    Align, Button, Color32, CornerRadius, Frame, Id, Layout, Margin, Response, RichText, Sense,
    Stroke, Ui, UiBuilder, WidgetInfo, WidgetType,
};

use super::icons;
use super::text::{h3, medium, muted};
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
        .stroke(Stroke::new(1.0, palette().BORDER))
}

/// Borderless rounded surface: elevation comes from fill alone.
pub fn soft_frame(fill: Color32) -> Frame {
    Frame::new()
        .fill(fill)
        .inner_margin(Margin::symmetric(SP_3 as i8, SP_2 as i8))
        .corner_radius(CornerRadius::same(R_LG))
}

/// Fill for an interactive row, fading the hover highlight in and out.
pub fn row_fill(ui: &Ui, id: Id, hovered: bool, selected: bool) -> Color32 {
    let t = ui
        .ctx()
        .animate_bool_with_time(id.with("row-hover"), hovered || selected, HOVER_FADE);
    let target = if selected {
        palette().ACTIVE_ROW
    } else {
        palette().HOVER_ROW
    };
    Color32::TRANSPARENT.lerp_to_gamma(target, t)
}

pub fn status_dot(ui: &mut Ui, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(DOT_SIZE, DOT_SIZE), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), DOT_SIZE / 2.0, color);
    response
}

/// Status dot with a soft halo while `active` (e.g. running). The halo is
/// static: a breathing animation would repaint continuously while any run is
/// live, and running rows already carry a spinner.
pub fn halo_dot(ui: &mut Ui, color: Color32, active: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(DOT_SIZE, DOT_SIZE), Sense::hover());
    let painter = ui.painter();
    if active {
        painter.circle_filled(
            rect.center(),
            DOT_SIZE / 2.0 + 3.0,
            color.gamma_multiply(0.2),
        );
    }
    painter.circle_filled(rect.center(), DOT_SIZE / 2.0, color);
    response
}

pub fn badge(ui: &mut Ui, text: impl Into<String>, fg: Color32, bg: Color32) -> Response {
    Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(R_PILL))
        .inner_margin(Margin::symmetric(SP_1 as i8 + 2, 0))
        .show(ui, |ui| ui.label(medium(text).size(FONT_BADGE).color(fg)))
        .response
}

/// Accent button painted as `icon text`, named `text` for accessibility.
pub fn primary_icon_button(ui: &mut Ui, icon: &str, text: &str) -> Response {
    let response = primary_button(ui, icons::with_icon(icon, text));
    accessible(&response, text);
    response
}

pub fn primary_button(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(
        Button::new(medium(text).color(palette().ACCENT_FG))
            .fill(palette().ACCENT)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(R_SM)),
    )
}

/// Button that only shows its frame while hovered or pressed.
pub fn ghost<'a>(text: impl Into<egui::WidgetText>) -> Button<'a> {
    Button::new(text.into())
        .frame_when_inactive(false)
        .corner_radius(CornerRadius::same(R_SM))
}

/// Icon glyph sized for toolbar and row actions.
pub fn icon_text(icon: &str) -> RichText {
    RichText::new(icon).size(FONT_ICON)
}

/// Overrides a button's accessible name, e.g. when it paints an icon.
pub fn accessible(response: &Response, label: &str) {
    let enabled = response.enabled();
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
}

/// Gives an icon-only control its accessible name and tooltip.
pub fn labeled(response: Response, label: &str) -> Response {
    accessible(&response, label);
    response.on_hover_text(label)
}

/// Frameless icon button whose accessible name is `label`.
pub fn icon_button(ui: &mut Ui, icon: &str, label: &str) -> Response {
    icon_button_rich(ui, icon_text(icon), label)
}

pub fn icon_button_rich(ui: &mut Ui, icon: RichText, label: &str) -> Response {
    let side = ROW_DENSE - SP_1;
    labeled(ui.add(ghost(icon).min_size(egui::vec2(side, side))), label)
}

/// Icon + text ghost button keeping `text` as its accessible name.
pub fn ghost_icon_button(ui: &mut Ui, icon: &str, text: &str) -> Response {
    let response = ui.add(ghost(RichText::new(icons::with_icon(icon, text))));
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, text));
    response
}

/// One telemetry value rendered as `icon value`, keeping the full textual
/// label (`cache 50%`, `TTFT 240ms`, ...) as its accessible name.
pub fn metric(ui: &mut Ui, label: &str) -> Response {
    let (icon, value, hint) = metric_parts(label);
    let response = ui.add(
        egui::Label::new(muted(icons::with_icon(icon, value)))
            .sense(Sense::hover())
            .wrap_mode(egui::TextWrapMode::Extend),
    );
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, label));
    response.on_hover_text(format!("{hint}: {label}"))
}

fn metric_parts(label: &str) -> (&'static str, &str, &'static str) {
    let (average, rest) = match label.strip_prefix("avg ") {
        Some(rest) => (true, rest),
        None => (false, label),
    };
    let (icon, value, hint) = if let Some(value) = rest.strip_prefix('$') {
        (icons::CURRENCY_DOLLAR, value, "Cost")
    } else if let Some(value) = rest.strip_prefix("cache ") {
        (icons::DATABASE, value, "Prompt cache hit rate")
    } else if let Some(value) = rest.strip_prefix("TTFT ") {
        (icons::TIMER, value, "Time to first token")
    } else if let Some(value) = rest.strip_prefix("ctx ") {
        (icons::STACK, value, "Context window usage")
    } else if let Some(value) = rest.strip_prefix("wall ") {
        (icons::CLOCK, value, "Wall time")
    } else if let Some(value) = rest.strip_prefix("Δ ") {
        (icons::HOURGLASS_MEDIUM, value, "Current request elapsed")
    } else if rest.contains("tok/s") {
        (icons::LIGHTNING, rest, "Output throughput")
    } else if rest.ends_with(" tok") {
        (icons::COINS, rest, "Tokens")
    } else {
        (icons::DOT_OUTLINE, rest, "Metric")
    };
    let hint = match (average, hint) {
        (true, "Prompt cache hit rate") => "Average prompt cache hit rate",
        (true, "Time to first token") => "Average time to first token",
        (true, "Output throughput") => "Average output throughput",
        (_, hint) => hint,
    };
    (icon, value, hint)
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

/// Dense list row: transparent at rest, with an animated hover highlight and
/// a persistent fill when `selected`.
pub fn compact_row<R>(ui: &mut Ui, selected: bool, add: impl FnOnce(&mut Ui) -> R) -> Response {
    let id = ui.next_auto_id();
    let mut prepared = Frame::new()
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
    let hovered = ui.rect_contains_pointer(response.rect.expand2(egui::vec2(SP_2, 0.0)));
    prepared.frame.fill = row_fill(ui, id, hovered, selected);
    prepared.end(ui)
}

/// Fills the rest of a row with a left-aligned, truncated, clickable label.
pub fn row_title(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> Response {
    fill_label(ui, text, ROW_DENSE, Sense::click())
}

/// Allocates the remaining width of the current row and paints `text`
/// left-aligned and truncated in it. `Label` centers itself in sized or
/// justified allocations, which reads as a button rather than a list item.
pub fn fill_label(
    ui: &mut Ui,
    text: impl Into<egui::WidgetText>,
    height: f32,
    sense: Sense,
) -> Response {
    let width = ui.available_width().max(0.0);
    let galley = text.into().into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        width,
        egui::TextStyle::Body,
    );
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), sense);
    let enabled = ui.is_enabled();
    let label = galley.job.text.clone();
    let elided = galley.elided;
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, enabled, &label));
    if ui.is_rect_visible(rect) {
        let pos = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
        let color = if sense.senses_click() {
            ui.style().interact(&response).text_color()
        } else {
            ui.visuals().strong_text_color()
        };
        ui.painter().galley(pos, galley, color);
    }
    if elided {
        return response.on_hover_text(label);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::metric_parts;
    use crate::theme::icons;

    #[test]
    fn metric_parts_strip_textual_prefixes_into_icons() {
        assert_eq!(
            metric_parts("cache 50% (Δ25%)"),
            (icons::DATABASE, "50% (Δ25%)", "Prompt cache hit rate")
        );
        assert_eq!(
            metric_parts("avg TTFT 300ms"),
            (icons::TIMER, "300ms", "Average time to first token")
        );
        assert_eq!(
            metric_parts("$0.414"),
            (icons::CURRENCY_DOLLAR, "0.414", "Cost")
        );
        assert_eq!(
            metric_parts("— tok/s"),
            (icons::LIGHTNING, "— tok/s", "Output throughput")
        );
        assert_eq!(metric_parts("wall 12s"), (icons::CLOCK, "12s", "Wall time"));
    }
}
