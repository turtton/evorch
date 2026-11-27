//! Model Settings: the screen-level modal of the core client that hosts the Providers, Routing
//! and Agent Roles sections (the legacy window keeps its own, smaller one in `settings`).
//!
//! The frame owns what every section shares: the size (as large as the window allows, never
//! larger than it), the left navigation, a header with the section's title and a close icon,
//! a scrolling body and a footer. The sections only draw their own content into the parts the
//! frame hands them, so a section added later changes nothing here. Closing (the × icon, Esc
//! or a click on the backdrop) is reported, not performed: whether unsaved edits are asked
//! about is the caller's decision.

use super::settings::SettingsTab;
use crate::theme::{
    icons,
    text::{h3, muted},
    tokens::*,
    widgets::{accessible, icon_button, readable_selection, surface_frame},
};
use egui::{Align, Layout, Rect, UiBuilder, vec2};

/// The sections of Model Settings are the tabs of the settings modal; the frame names and
/// describes them as this screen does.
pub fn section_label(tab: SettingsTab) -> &'static str {
    match tab {
        SettingsTab::Providers => "Providers",
        SettingsTab::Routing => "Routing",
        SettingsTab::AgentRoles => "Agent Roles",
    }
}

/// One line under the heading saying what the section is for.
pub fn section_subtitle(tab: SettingsTab) -> &'static str {
    match tab {
        SettingsTab::Providers => "Where models come from, with their sign-in and model list",
        SettingsTab::Routing => {
            "Logical model name to real provider and model. Candidates are tried from the top"
        }
        SettingsTab::AgentRoles => "Which route each agent role uses, and how it is tuned",
    }
}

pub fn section_icon(tab: SettingsTab) -> &'static str {
    match tab {
        SettingsTab::Providers => icons::HARD_DRIVES,
        SettingsTab::Routing => icons::PATH,
        SettingsTab::AgentRoles => icons::USERS,
    }
}

/// What the modal frame itself asked for; sections report their own actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsModalAction {
    SelectTab(SettingsTab),
    /// The × icon, Esc or the backdrop. Nothing is closed until the caller does it.
    Close,
}

/// The part of the frame the caller is asked to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ModalPart {
    /// The bottom of the navigation: the applied state of the saved settings.
    NavFooter,
    /// The right end of the header, left of the close icon (the profile control).
    HeaderRight,
    /// The scrolling body of the active section. `height` is what is visible of it at once.
    Body { height: f32 },
    /// The footer: the status line and, when needed, the confirmation to close.
    Footer,
}

/// Widest the modal grows; a window wider than this keeps margins around it.
pub const MODAL_MAX_WIDTH: f32 = 1080.0;
pub const MODAL_MAX_HEIGHT: f32 = 780.0;
/// The free space kept around the modal.
pub const MODAL_MARGIN: f32 = SP_4 * 1.5;
pub const NAV_WIDTH: f32 = 188.0;
pub const HEADER_HEIGHT: f32 = TOPBAR;
pub const FOOTER_HEIGHT: f32 = TOPBAR - SP_2;
const NAV_ROW: f32 = ROW_DENSE + SP_2;

/// The modal's size in a window of `window`: all of it minus a margin, up to a maximum. The
/// parts are laid out inside this size, so the header and the footer cannot leave the window.
pub fn modal_size(window: egui::Vec2) -> egui::Vec2 {
    vec2(
        (window.x - MODAL_MARGIN * 2.0).clamp(0.0, MODAL_MAX_WIDTH),
        (window.y - MODAL_MARGIN * 2.0).clamp(0.0, MODAL_MAX_HEIGHT),
    )
}

/// Draws the frame: the navigation, the header with its close icon, `part` for the content of
/// each region and the footer. While `busy` (a save is running) the navigation and the close
/// icon are disabled, and Esc does not close.
pub fn settings_modal(
    ctx: &egui::Context,
    active: SettingsTab,
    busy: bool,
    mut part: impl FnMut(&mut egui::Ui, ModalPart),
) -> Option<SettingsModalAction> {
    let mut action = None;
    let size = modal_size(ctx.viewport_rect().size());
    let modal = egui::Modal::new(egui::Id::new("model-settings"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED).inner_margin(egui::Margin::ZERO))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing = vec2(SP_2, SP_2);
            let (frame, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            let nav = Rect::from_min_size(frame.min, vec2(NAV_WIDTH.min(size.x), size.y));
            let main = Rect::from_min_max(egui::pos2(nav.max.x, frame.min.y), frame.max);
            let header = Rect::from_min_size(main.min, vec2(main.width(), HEADER_HEIGHT));
            let footer = Rect::from_min_max(
                egui::pos2(main.min.x, (main.max.y - FOOTER_HEIGHT).max(header.max.y)),
                main.max,
            );
            let body = Rect::from_min_max(
                egui::pos2(main.min.x, header.max.y),
                egui::pos2(main.max.x, footer.min.y),
            );
            let painter = ui.painter();
            painter.rect_filled(nav, 0.0, palette().SURFACE);
            let line = egui::Stroke::new(1.0, palette().BORDER);
            painter.vline(nav.max.x, nav.y_range(), line);
            painter.hline(main.x_range(), header.max.y, line);
            painter.hline(main.x_range(), footer.min.y, line);

            navigation(ui, nav, active, busy, &mut action, &mut part);
            self::header(ui, header, active, busy, &mut action, &mut part);
            self::body(ui, body, &mut part);
            self::footer(ui, footer, &mut part);
        });
    if !busy && modal.should_close() {
        action = Some(SettingsModalAction::Close);
    }
    action
}

fn region(ui: &mut egui::Ui, rect: Rect, layout: Layout) -> egui::Ui {
    let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(layout));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child.spacing_mut().item_spacing = vec2(SP_2, SP_2);
    child
}

fn navigation(
    ui: &mut egui::Ui,
    rect: Rect,
    active: SettingsTab,
    busy: bool,
    action: &mut Option<SettingsModalAction>,
    part: &mut impl FnMut(&mut egui::Ui, ModalPart),
) {
    let inner = rect.shrink2(vec2(SP_3, SP_3));
    let mut nav = region(ui, inner, Layout::top_down(Align::Min));
    nav.horizontal(|ui| {
        ui.label(egui::RichText::new(icons::STACK_SIMPLE).size(FONT_ICON));
        ui.label(h3("Model Settings"));
    });
    nav.add_space(SP_2);
    nav.add_enabled_ui(!busy, |ui| {
        readable_selection(ui);
        for tab in SettingsTab::ALL {
            let selected = tab == active;
            let response = ui.add_sized(
                vec2(ui.available_width(), NAV_ROW),
                egui::Button::selectable(
                    selected,
                    egui::RichText::new(icons::with_icon(section_icon(tab), section_label(tab))),
                )
                .frame_when_inactive(selected),
            );
            accessible(&response, section_label(tab));
            if response.clicked() && !selected {
                *action = Some(SettingsModalAction::SelectTab(tab));
            }
        }
    });
    // The applied state sits at the bottom, whatever the navigation's height.
    let used = nav.min_rect().max.y;
    let remaining = (inner.max.y - used).max(0.0);
    let mut bottom = region(
        ui,
        Rect::from_min_max(egui::pos2(inner.min.x, inner.max.y - remaining), inner.max),
        Layout::bottom_up(Align::Min),
    );
    part(&mut bottom, ModalPart::NavFooter);
}

fn header(
    ui: &mut egui::Ui,
    rect: Rect,
    active: SettingsTab,
    busy: bool,
    action: &mut Option<SettingsModalAction>,
    part: &mut impl FnMut(&mut egui::Ui, ModalPart),
) {
    let mut header = region(
        ui,
        rect.shrink2(vec2(SP_4, 0.0)),
        Layout::right_to_left(Align::Center),
    );
    header.add_enabled_ui(!busy, |ui| {
        if icon_button(ui, icons::X, "Close").clicked() {
            *action = Some(SettingsModalAction::Close);
        }
    });
    part(&mut header, ModalPart::HeaderRight);
    // What is left of the header, from its left edge, is the section's name.
    let left = Rect::from_min_max(
        rect.min + vec2(SP_4, 0.0),
        egui::pos2(header.min_rect().min.x - SP_2, rect.max.y),
    );
    if left.width() > 0.0 {
        let mut title = region(ui, left, Layout::left_to_right(Align::Center));
        title.label(h3(section_label(active)));
        title.add(egui::Label::new(muted(section_subtitle(active))).truncate());
    }
}

fn body(ui: &mut egui::Ui, rect: Rect, part: &mut impl FnMut(&mut egui::Ui, ModalPart)) {
    let inner = rect.shrink2(vec2(SP_4, SP_3));
    let mut body = region(ui, inner, Layout::top_down(Align::Min));
    let height = inner.height();
    egui::ScrollArea::vertical()
        .id_salt("model-settings-body")
        .auto_shrink([false, false])
        .max_height(height)
        .show(&mut body, |ui| {
            ui.set_width(ui.available_width());
            part(ui, ModalPart::Body { height });
        });
}

fn footer(ui: &mut egui::Ui, rect: Rect, part: &mut impl FnMut(&mut egui::Ui, ModalPart)) {
    let mut footer = region(
        ui,
        rect.shrink2(vec2(SP_4, 0.0)),
        Layout::left_to_right(Align::Center),
    );
    part(&mut footer, ModalPart::Footer);
}

#[cfg(test)]
mod tests {
    use super::{
        FOOTER_HEIGHT, HEADER_HEIGHT, ModalPart, SettingsModalAction, modal_size, settings_modal,
    };
    use crate::panes::settings::SettingsTab;
    use crate::theme::style::{ThemePreset, install_preset};
    use crate::theme::tokens::palette;
    use egui_kittest::kittest::Queryable;

    // Given: the modal in Graphite, whose selection stroke is its fill
    // When: the Routing section is active
    // Then: its navigation label is drawn in the accent's foreground, so it reads on the accent
    #[test]
    fn the_active_section_label_reads_on_the_accent_in_graphite() {
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            install_preset(ui.ctx(), ThemePreset::Graphite);
            settings_modal(ui.ctx(), SettingsTab::Routing, false, |_, _| {});
        });
        harness.run_steps(2);
        let label = harness
            .output()
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text().ends_with("Routing") => {
                    Some(text.override_text_color.unwrap_or(text.fallback_color))
                }
                _ => None,
            });
        assert_eq!(label, Some(palette().ACCENT_FG));
    }

    // Given: windows from the smallest the app supports up to a very large one
    // When: the modal is sized
    // Then: it fits inside the window, leaves room for the header and the footer, and stops
    //       growing at its maximum
    #[test]
    fn the_modal_fits_the_window_and_stops_growing() {
        for window in [
            egui::vec2(960.0, 600.0),
            egui::vec2(1280.0, 800.0),
            egui::vec2(3840.0, 2160.0),
        ] {
            let size = modal_size(window);
            assert!(size.x <= window.x && size.y <= window.y, "{window:?}");
            assert!(size.y > HEADER_HEIGHT + FOOTER_HEIGHT + 200.0, "{window:?}");
        }
        let largest = modal_size(egui::vec2(3840.0, 2160.0));
        assert_eq!(
            largest,
            egui::vec2(super::MODAL_MAX_WIDTH, super::MODAL_MAX_HEIGHT)
        );
    }

    // Given: the modal while a save is running
    // When: the × icon is pressed
    // Then: nothing is asked of the caller; once the save has ended the same press asks to close
    #[test]
    fn the_close_icon_is_disabled_while_busy_and_asks_to_close_otherwise() {
        let mut asked = Vec::new();
        let mut busy = true;
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut (bool, Vec<SettingsModalAction>)| {
                let action =
                    settings_modal(ui.ctx(), SettingsTab::Providers, state.0, |ui, part| {
                        if let ModalPart::Body { .. } = part {
                            ui.label("body");
                        }
                    });
                state.1.extend(action);
            },
            (busy, Vec::new()),
        );
        harness.run_steps(2);
        harness.get_by_label("Close").click();
        harness.run_steps(2);
        assert!(harness.state().1.is_empty());
        busy = false;
        harness.state_mut().0 = busy;
        harness.run_steps(2);
        harness.get_by_label("Close").click();
        harness.run_steps(2);
        asked.extend(harness.state().1.iter().copied());
        assert_eq!(asked, [SettingsModalAction::Close]);
    }
}
