//! The fixed strip above the project list: add a project, sort, fold everything.

use egui::{Align, Button, Layout, Sense, Stroke, Ui, UiBuilder, WidgetInfo, WidgetType};
use workspace_ui::{ProjectSort, SidebarState, SidebarView};

use crate::theme::icons;
use crate::theme::tokens::{ROW_DENSE, SP_1, SP_2, palette};
use crate::theme::widgets::{accessible, ghost, icon_button, icon_text};

use super::SidebarAction;

/// Height of the strip, separator included: one dense row.
pub const TOOLBAR_HEIGHT: f32 = ROW_DENSE;
/// Side of the icon-only buttons.
const ICON_BUTTON: f32 = ROW_DENSE - SP_1;

const ADD_LABEL: &str = "Add project";
const SORT_LABEL: &str = "Sort projects";

const SORT_CHOICES: [(ProjectSort, &str); 3] = [
    (ProjectSort::Registration, "Registration order"),
    (ProjectSort::Name, "Name (A–Z)"),
    (ProjectSort::RecentActivity, "Recent activity"),
];

/// Draws the strip across the full width and records the operator's choice in `action`.
pub fn render(
    ui: &mut Ui,
    sidebar: &SidebarState,
    view: &SidebarView,
    action: &mut Option<SidebarAction>,
) {
    let (band, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), TOOLBAR_HEIGHT),
        Sense::hover(),
    );
    ui.painter().hline(
        band.x_range(),
        band.bottom() - 0.5,
        Stroke::new(1.0, palette().BORDER),
    );
    let inner = band
        .shrink2(egui::vec2(SP_2, 0.0))
        .with_max_y(band.bottom() - 1.0);
    let mut strip = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Center)),
    );
    strip.spacing_mut().item_spacing.x = SP_1;

    let add_text = icons::with_icon(icons::PLUS, ADD_LABEL);
    let text_width = ui
        .painter()
        .layout_no_wrap(
            add_text.clone(),
            egui::TextStyle::Button.resolve(ui.style()),
            palette().TEXT,
        )
        .size()
        .x;
    let full_width = text_width + 2.0 * ui.spacing().button_padding.x;
    // The text label only stays while it fits beside both icon buttons.
    let wide = inner.width() >= full_width + 2.0 * ICON_BUTTON + 3.0 * SP_1;

    if wide {
        let response = strip.add(ghost(add_text).min_size(egui::vec2(0.0, ICON_BUTTON)));
        accessible(&response, ADD_LABEL);
        if response.clicked() {
            *action = Some(SidebarAction::OpenAddProject);
        }
    } else if icon_button(&mut strip, icons::PLUS, ADD_LABEL).clicked() {
        *action = Some(SidebarAction::OpenAddProject);
    }

    strip.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        fold_button(ui, sidebar, view, action);
        sort_button(ui, view, action);
    });
}

/// Folds every project when any is open, otherwise unfolds them all.
fn fold_button(
    ui: &mut Ui,
    sidebar: &SidebarState,
    view: &SidebarView,
    action: &mut Option<SidebarAction>,
) {
    let fold = view.any_open(sidebar);
    let (icon, label) = if fold {
        (icons::ARROWS_IN_LINE_VERTICAL, "Collapse all projects")
    } else {
        (icons::ARROWS_OUT_LINE_VERTICAL, "Expand all projects")
    };
    let clicked = ui
        .add_enabled_ui(!sidebar.projects.is_empty(), |ui| {
            icon_button(ui, icon, label).clicked()
        })
        .inner;
    if clicked {
        *action = Some(SidebarAction::SetAllProjectsCollapsed(fold));
    }
}

/// A menu of the three orders, the current one checked.
fn sort_button(ui: &mut Ui, view: &SidebarView, action: &mut Option<SidebarAction>) {
    let button =
        ghost(icon_text(icons::ARROWS_DOWN_UP)).min_size(egui::vec2(ICON_BUTTON, ICON_BUTTON));
    let (response, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
        for (sort, name) in SORT_CHOICES {
            let current = view.project_sort == sort;
            let mut item = Button::selectable(current, name);
            if current {
                item = item.right_text(icons::CHECK);
            }
            let response = ui.add(item);
            // `right_text` would otherwise become part of the accessible name.
            response.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, current, name));
            if response.clicked() {
                *action = Some(SidebarAction::SetProjectSort(sort));
            }
        }
    });
    accessible(&response, SORT_LABEL);
    response.on_hover_text(SORT_LABEL);
}
