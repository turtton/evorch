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

/// A failure reason in `color`: runtime reasons can be Japanese, so the label
/// shows the English form and the original text is kept in the hover.
pub fn reason_label(ui: &mut Ui, color: Color32, raw: impl AsRef<str>) -> Response {
    reason_label_with(ui, color, raw, str::to_owned)
}

/// [`reason_label`] with the English reason embedded in a longer `text`.
pub fn reason_label_with(
    ui: &mut Ui,
    color: Color32,
    raw: impl AsRef<str>,
    text: impl FnOnce(&str) -> String,
) -> Response {
    let raw = raw.as_ref();
    let shown = crate::model::failure_text::english(raw);
    let response = ui.colored_label(color, text(&shown));
    if shown == raw {
        response
    } else {
        response.on_hover_text(raw)
    }
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

/// Copies `value` on click; the tooltip shows the full ID and the accessible
/// name stays `label`.
pub fn copy_id_button(ui: &mut Ui, label: &str, value: &str) -> Response {
    let side = ROW_DENSE - SP_1;
    let response = ui.add(ghost(icon_text(icons::COPY)).min_size(egui::vec2(side, side)));
    accessible(&response, label);
    let response = response.on_hover_text(format!("{label}\n{value}"));
    if response.clicked() {
        ui.ctx().copy_text(value.to_owned());
    }
    response
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
/// label (`cache 99%`, `TTFT 240ms`, ...) as its accessible name.
pub fn metric(ui: &mut Ui, label: &str) -> Response {
    let (_, _, hint) = metric_parts(label);
    metric_detailed(ui, label, &format!("{hint}: {label}"), false)
}

/// [`metric`] with its own hover text, in the warning color when `warning`.
pub fn metric_detailed(ui: &mut Ui, label: &str, tooltip: &str, warning: bool) -> Response {
    let (icon, value, _) = metric_parts(label);
    let mut text = muted(icons::with_icon(icon, value));
    if warning {
        text = text.color(palette().WARNING_FG);
    }
    let response = ui.add(
        egui::Label::new(text)
            .sense(Sense::hover())
            .wrap_mode(egui::TextWrapMode::Extend),
    );
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, label));
    response.on_hover_text(tooltip)
}

fn metric_parts(label: &str) -> (&'static str, &str, &'static str) {
    let (average, rest) = match label.strip_prefix("avg ") {
        Some(rest) => (true, rest),
        None => (false, label),
    };
    let (icon, value, hint) = if let Some(value) = rest.strip_prefix('$') {
        (icons::CURRENCY_DOLLAR, value, "Cost")
    } else if let Some(value) = rest.strip_prefix("cache ") {
        (icons::DATABASE, value, "Prompt cache retention")
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
        (true, "Prompt cache retention") => "Average prompt cache retention",
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

/// How often a visible spinner moves. egui's `Spinner` asks for a repaint every
/// frame, which rebuilds the whole workbench at the display rate while anything
/// is running; this keeps the animation at a fraction of that cost.
pub const SPINNER_FRAME: std::time::Duration = std::time::Duration::from_millis(100);

/// A loading spinner the size of the style's interactive height.
#[track_caller]
pub fn spinner(ui: &mut Ui) -> Response {
    let size = ui.style().spacing.interact_size.y;
    spinner_with(ui, size, None)
}

/// A spinner of `size` points, in `color` or the strong text color.
#[track_caller]
pub fn spinner_with(ui: &mut Ui, size: f32, color: Option<Color32>) -> Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    response.widget_info(|| WidgetInfo::new(WidgetType::ProgressIndicator));
    paint_spinner(ui, rect, color);
    response
}

/// Paints the spinner arc in `rect` and schedules its next step when visible.
/// The repaint request carries the caller's location for frame diagnostics.
#[track_caller]
pub fn paint_spinner(ui: &Ui, rect: egui::Rect, color: Option<Color32>) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.ctx().request_repaint_after(SPINNER_FRAME);
    let color = color.unwrap_or_else(|| ui.visuals().strong_text_color());
    let radius = rect.height().min(rect.width()) / 2.0 - 2.0;
    let points = (radius.round() as u32).clamp(8, 128);
    let time = ui.input(|input| input.time);
    let start = time * std::f64::consts::TAU;
    let end = start + 240f64.to_radians() * time.sin();
    let arc = (0..points)
        .map(|index| {
            let angle = egui::emath::lerp(start..=end, f64::from(index) / f64::from(points));
            let (sin, cos) = angle.sin_cos();
            rect.center() + radius * egui::vec2(cos as f32, sin as f32)
        })
        .collect();
    ui.painter()
        .add(egui::Shape::line(arc, Stroke::new(3.0, color)));
}

#[cfg(test)]
mod tests {
    use super::metric_parts;
    use crate::theme::icons;

    #[test]
    fn metric_parts_strip_textual_prefixes_into_icons() {
        assert_eq!(
            metric_parts("cache 99% (avg 97%)"),
            (icons::DATABASE, "99% (avg 97%)", "Prompt cache retention")
        );
        assert_eq!(
            metric_parts("avg cache 97.0%"),
            (icons::DATABASE, "97.0%", "Average prompt cache retention")
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

    #[test]
    fn copy_id_button_names_its_action_and_copies_the_hidden_value() {
        use egui_kittest::{Harness, kittest::Queryable};
        // Given: an ID rendered only as a copy button.
        let mut harness = Harness::new_ui(|ui| {
            super::copy_id_button(ui, "Copy run ID", "run-42");
        });
        harness.run();
        // Then: the action, not the raw value, is its accessible name.
        assert!(harness.query_by_label("run-42").is_none());
        // When: activated.
        harness.get_by_label("Copy run ID").click();
        harness.step();
        // Then: the full value reaches the clipboard.
        assert!(harness.output().platform_output.commands.iter().any(
            |command| matches!(command, egui::OutputCommand::CopyText(text) if text == "run-42")
        ));
    }

    fn repaint_delay(ctx: &egui::Context, add: impl FnMut(&mut egui::Ui)) -> std::time::Duration {
        let mut output = ctx.run_ui(egui::RawInput::default(), add);
        output.textures_delta.clear();
        output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
    }

    #[test]
    fn spinner_schedules_its_next_step_instead_of_repainting_every_frame() {
        // Given: a context that has settled after its start-up repaints.
        let ctx = egui::Context::default();
        assert!(
            (0..10).any(|_| repaint_delay(&ctx, |_| {}) == std::time::Duration::MAX),
            "an empty UI never settled"
        );
        // When: only a spinner is visible.
        let delay = repaint_delay(&ctx, |ui| {
            super::spinner(ui);
        });
        // Then: it waits for its next animation step; egui's own spinner would not.
        assert!(
            delay > std::time::Duration::ZERO && delay <= super::SPINNER_FRAME,
            "{delay:?}"
        );
        let egui_delay = repaint_delay(&ctx, |ui| {
            ui.add(egui::Spinner::new());
        });
        assert_eq!(egui_delay, std::time::Duration::ZERO);
    }

    #[test]
    fn spinner_paints_an_arc_and_reports_a_progress_indicator() {
        use egui_kittest::{Harness, kittest::Queryable};
        let mut harness = Harness::new_ui(|ui| {
            super::spinner_with(ui, 16.0, Some(egui::Color32::RED));
        });
        harness.run_steps(2);
        harness.get_by_role(egui::accesskit::Role::ProgressIndicator);
        assert!(harness.output().shapes.iter().any(|shape| matches!(
            &shape.shape,
            egui::Shape::Path(path)
                if !path.closed && path.stroke.color == egui::epaint::ColorMode::Solid(egui::Color32::RED)
        )));
    }
}
