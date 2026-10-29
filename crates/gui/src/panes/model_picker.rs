//! Composer model button and the modal it opens: a fuzzy search over provider
//! and model names, then the reasoning effort of the model picked.

use egui::{Key, Modifiers};
use runtime::compose::ProfileSummary;
use workspace_ui::ModelPreference;

use crate::model::model_picker::{
    ModelPickerState, PickerRow, PickerStage, effort_rows, picker_rows, preference_label,
    same_model,
};
use crate::theme::text::{h3, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::surface_frame;

#[derive(Clone, Copy)]
pub struct ModelPickerContext<'a> {
    pub profiles: &'a [ProfileSummary],
    pub preference: Option<&'a ModelPreference>,
    /// The role's routed model, shown while no explicit preference is set.
    pub default_model: Option<&'a str>,
    pub enabled: bool,
}

/// Search box of the model list.
pub const SEARCH_LABEL: &str = "Search providers and models";
/// Model list entry that clears the explicit selection.
pub const AUTOMATIC_LABEL: &str = "Automatic routing";
/// Effort entry that sends no explicit effort.
pub const DEFAULT_EFFORT_LABEL: &str = "Default effort";
/// Heading of the effort step.
pub const EFFORT_HEADING: &str = "Reasoning effort";

pub fn model_picker(
    ui: &mut egui::Ui,
    context: ModelPickerContext<'_>,
    state: &mut ModelPickerState,
) -> Option<Option<ModelPreference>> {
    let enabled = context.enabled && !context.profiles.is_empty();
    let label = context
        .preference
        .map(|preference| match &preference.reasoning_effort {
            Some(effort) => format!("{} · {effort}", preference_label(preference)),
            None => preference_label(preference),
        })
        .or_else(|| context.default_model.map(str::to_owned))
        .unwrap_or_else(|| "Select model".into());
    // Keys act on the modal only when it was already open; the Enter that pressed
    // this button must not also pick a row.
    let was_open = state.open;
    let button = ui
        .add_enabled(enabled, egui::Button::new(&label).truncate())
        .on_hover_text("Choose the model and reasoning effort");
    if button.clicked() {
        state.open(context.preference, context.profiles);
    }
    if !enabled {
        state.close();
    }
    if state.open {
        return picker_modal(ui.ctx(), context, state, was_open);
    }
    None
}

fn picker_modal(
    ctx: &egui::Context,
    context: ModelPickerContext<'_>,
    state: &mut ModelPickerState,
    accept_keys: bool,
) -> Option<Option<ModelPreference>> {
    let keys = accept_keys.then(|| read_keys(ctx));
    if keys.is_some_and(|keys| keys.escape) {
        match state.stage {
            PickerStage::Models => state.close(),
            PickerStage::Effort { .. } => {
                state.stage = PickerStage::Models;
                state.focus_query = true;
            }
        }
        if !state.open {
            return None;
        }
    }
    let mut chosen = None;
    let modal = egui::Modal::new(egui::Id::new("model-picker"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width((ctx.viewport_rect().width() * 0.5).min(PICKER_MAX_WIDTH) - SP_4 * 2.0);
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            chosen = match state.stage.clone() {
                PickerStage::Models => models_step(ui, context, state, keys),
                PickerStage::Effort { model, selected } => {
                    effort_step(ui, context, state, &model, selected, keys)
                }
            };
        });
    if chosen.is_none() && state.open && modal.should_close() {
        state.close();
    }
    chosen
}

#[derive(Clone, Copy, Default)]
struct Keys {
    up: bool,
    down: bool,
    enter: bool,
    /// Closes the model list; in the effort step it returns to the list.
    escape: bool,
}

/// Takes the picker's keys out of the frame so that nothing drawn after it, such
/// as the composer below, also reacts to them.
fn read_keys(ctx: &egui::Context) -> Keys {
    ctx.input_mut(|input| {
        let mut press = |key| input.consume_key(Modifiers::NONE, key);
        Keys {
            up: press(Key::ArrowUp),
            down: press(Key::ArrowDown),
            enter: press(Key::Enter),
            escape: press(Key::Escape),
        }
    })
}

/// Moves a highlight by the arrow keys, wrapping around `len` rows.
fn step(selected: usize, len: usize, keys: Keys) -> usize {
    if len == 0 {
        return 0;
    }
    let selected = selected.min(len - 1);
    if keys.down {
        (selected + 1) % len
    } else if keys.up {
        (selected + len - 1) % len
    } else {
        selected
    }
}

fn models_step(
    ui: &mut egui::Ui,
    context: ModelPickerContext<'_>,
    state: &mut ModelPickerState,
    keys: Option<Keys>,
) -> Option<Option<ModelPreference>> {
    ui.label(h3("Select model"));
    let search = ui.add(
        egui::TextEdit::singleline(&mut state.query)
            .id(egui::Id::new("model-picker-search"))
            .hint_text(SEARCH_LABEL)
            .desired_width(f32::INFINITY),
    );
    search
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, SEARCH_LABEL));
    if state.focus_query {
        search.request_focus();
        state.focus_query = false;
    }
    if search.changed() {
        state.selected = 0;
    }
    let rows = picker_rows(context.profiles, &state.query);
    let keys = keys.unwrap_or_default();
    let moved = keys.up || keys.down;
    state.selected = step(state.selected, rows.len(), keys);
    let mut chosen = None;
    if rows.is_empty() {
        ui.label(muted("No matching models"));
    }
    egui::ScrollArea::vertical()
        .max_height(PICKER_LIST_HEIGHT)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for (index, row) in rows.iter().enumerate() {
                let (text, detail, current) = row_texts(row, context.preference);
                let response = picker_row(ui, index == state.selected, &text, &detail, current);
                if response.hovered()
                    && response
                        .ctx
                        .input(|i| i.pointer.delta() != egui::Vec2::ZERO)
                {
                    state.selected = index;
                }
                if moved && index == state.selected {
                    response.scroll_to_me(None);
                }
                if response.clicked() {
                    chosen = Some(index);
                }
            }
        });
    if keys.enter && chosen.is_none() && !rows.is_empty() {
        chosen = Some(state.selected);
    }
    let row = rows.get(chosen?)?;
    state.choose_row(row, context.preference, context.profiles)
}

/// Text, provider type, and whether the row is the current selection.
fn row_texts(row: &PickerRow, current: Option<&ModelPreference>) -> (String, String, bool) {
    match row {
        PickerRow::Automatic => (AUTOMATIC_LABEL.into(), String::new(), current.is_none()),
        PickerRow::Model(option) => (
            preference_label(&option.preference),
            option.provider.into(),
            current.is_some_and(|current| same_model(current, &option.preference)),
        ),
    }
}

/// Detail text of a row: the provider type, then a marker on the current selection.
fn detail_text(detail: &str, current: bool) -> String {
    match (detail.is_empty(), current) {
        (true, true) => "current".to_owned(),
        (true, false) => String::new(),
        (false, true) => format!("{detail} · current"),
        (false, false) => detail.to_owned(),
    }
}

/// Where a row's two texts go. The detail keeps its own width at the right and the
/// name takes what is left, so a long name is elided instead of running into it.
struct RowLayout {
    name: std::sync::Arc<egui::Galley>,
    name_pos: egui::Pos2,
    detail: std::sync::Arc<egui::Galley>,
    detail_pos: egui::Pos2,
}

/// One line of text no wider than `width`, ending in an ellipsis when it was cut.
fn elided(
    ui: &egui::Ui,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), font, color, width);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.painter().layout_job(job)
}

fn layout_row(ui: &egui::Ui, rect: egui::Rect, text: &str, detail: &str) -> RowLayout {
    let inner = rect.shrink2(egui::vec2(SP_2, 0.0));
    let detail = elided(
        ui,
        detail,
        egui::FontId::proportional(FONT_SMALL),
        palette().TEXT_MUTED,
        inner.width() * ROW_DETAIL_MAX_SHARE,
    );
    let detail_width = detail.size().x;
    let gap = if detail_width > 0.0 { SP_3 } else { 0.0 };
    let name_width = (inner.width() - detail_width - gap).max(0.0);
    let name = elided(
        ui,
        text,
        egui::FontId::proportional(FONT_BODY),
        palette().TEXT,
        name_width,
    );
    let name_pos = egui::pos2(inner.left(), rect.center().y - name.size().y / 2.0);
    let detail_pos = egui::pos2(
        inner.right() - detail_width,
        rect.center().y - detail.size().y / 2.0,
    );
    RowLayout {
        name,
        name_pos,
        detail,
        detail_pos,
    }
}

/// A full-width selectable row: the name on the left, elided when long, and the
/// provider type on the right.
fn picker_row(
    ui: &mut egui::Ui,
    highlighted: bool,
    text: &str,
    detail: &str,
    current: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_DENSE),
        egui::Sense::click(),
    );
    let enabled = ui.is_enabled();
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            enabled,
            highlighted,
            text,
        )
    });
    if ui.is_rect_visible(rect) {
        let fill = if highlighted {
            palette().SELECTED_ROW
        } else if response.hovered() {
            palette().HOVER_ROW
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_SM), fill);
        let layout = layout_row(ui, rect, text, &detail_text(detail, current));
        ui.painter()
            .galley(layout.name_pos, layout.name, palette().TEXT);
        ui.painter()
            .galley(layout.detail_pos, layout.detail, palette().TEXT_MUTED);
    }
    response
}

fn effort_step(
    ui: &mut egui::Ui,
    context: ModelPickerContext<'_>,
    state: &mut ModelPickerState,
    model: &ModelPreference,
    selected: usize,
    keys: Option<Keys>,
) -> Option<Option<ModelPreference>> {
    let keys = keys.unwrap_or_default();
    ui.label(h3(EFFORT_HEADING));
    ui.horizontal(|ui| {
        ui.label(muted(preference_label(model)));
        if ui.small_button("Back").clicked() {
            state.stage = PickerStage::Models;
            state.focus_query = true;
        }
    });
    let efforts = effort_rows(model, context.profiles);
    let selected = step(selected, efforts.len(), keys);
    let mut chosen = None;
    for (index, effort) in efforts.iter().enumerate() {
        let text = effort.as_deref().unwrap_or(DEFAULT_EFFORT_LABEL);
        let response = picker_row(
            ui,
            index == selected,
            text,
            "",
            *effort == model.reasoning_effort,
        );
        if response.clicked() {
            chosen = Some(index);
        }
    }
    if let PickerStage::Effort { selected: kept, .. } = &mut state.stage {
        *kept = selected;
    }
    if keys.enter && chosen.is_none() {
        chosen = Some(selected);
    }
    state.choose_effort(context.profiles, chosen?)
}

/// Modal content width limit; the modal is narrower on small windows.
const PICKER_MAX_WIDTH: f32 = 560.0;
/// Visible height of the model list before it scrolls.
const PICKER_LIST_HEIGHT: f32 = 360.0;
/// Largest share of a row's width the detail may take before it is cut short.
const ROW_DETAIL_MAX_SHARE: f32 = 0.4;

#[cfg(test)]
mod tests {
    use super::*;

    /// Lays out one row of `width` and returns it with the row's rectangle.
    fn layout(width: f32, text: &str, detail: &str) -> (RowLayout, egui::Rect) {
        let mut laid_out = None;
        let mut harness = egui_kittest::Harness::builder().build_ui(|ui| {
            crate::theme::install(ui.ctx());
            let rect =
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(width, ROW_DENSE));
            laid_out = Some((layout_row(ui, rect, text, detail), rect));
        });
        harness.run();
        drop(harness);
        laid_out.expect("the UI ran once")
    }

    #[test]
    fn long_names_are_elided_and_never_overlap_the_detail() {
        // (width, name, detail, name is cut)
        let long = "profile / a-very-long-model-name-that-keeps-going-and-going-and-going";
        for (width, name, detail, cut) in [
            (
                400.0,
                "local / model-b",
                "openai-compatible · current",
                false,
            ),
            (400.0, long, "openai-compatible · current", true),
            (160.0, long, "anthropic-subscription · current", true),
            (400.0, long, "", true),
        ] {
            let (row, rect) = layout(width, name, detail);
            let name_rect = egui::Rect::from_min_size(row.name_pos, row.name.size());
            let detail_rect = egui::Rect::from_min_size(row.detail_pos, row.detail.size());
            assert_eq!(row.name.elided, cut, "{name:?} in {width}");
            assert!(rect.contains_rect(name_rect), "{name:?} in {width}");
            assert!(rect.contains_rect(detail_rect), "{detail:?} in {width}");
            if !detail.is_empty() {
                assert!(
                    name_rect.right() < detail_rect.left(),
                    "name {name_rect:?} overlaps detail {detail_rect:?} in {width}"
                );
            }
        }
    }
}
