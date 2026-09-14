use egui::{Color32, CornerRadius, Margin, Stroke};
use egui_dock::{Style, TabBodyStyle, TabInteractionStyle, TabStyle};

use super::tokens::*;

pub fn dock_style(style: &egui::Style) -> Style {
    let mut dock = Style::from_egui(style);
    dock.dock_area_padding = None;
    dock.main_surface_border_stroke = Stroke::NONE;
    dock.separator.width = 1.0;
    dock.separator.extra_interact_width = 6.0;
    dock.separator.color_idle = palette().BORDER;
    dock.separator.color_hovered = palette().TEXT_MUTED;
    dock.separator.color_dragged = palette().ACCENT;

    dock.tab_bar.bg_fill = palette().SURFACE_RAISED;
    dock.tab_bar.height = TAB_HEIGHT + 2.0 * TAB_GAP;
    dock.tab_bar.inner_margin = Margin::symmetric(SP_1 as i8, TAB_GAP as i8);
    dock.tab_bar.corner_radius = CornerRadius::ZERO;
    dock.tab_bar.hline_color = palette().BORDER;
    dock.tab_bar.fill_tab_bar = false;
    dock.tab_bar.show_scroll_bar_on_overflow = true;

    dock.tab.spacing = TAB_GAP;
    dock.tab.minimum_width = None;
    dock.tab.hline_below_active_tab_name = true;

    let top_radius = CornerRadius {
        nw: R_SM,
        ne: R_SM,
        sw: 0,
        se: 0,
    };
    dock.tab.active = TabInteractionStyle {
        bg_fill: palette().ACTIVE_ROW,
        text_color: palette().TEXT,
        outline_color: palette().ACCENT,
        corner_radius: top_radius,
    };
    dock.tab.focused = TabInteractionStyle {
        bg_fill: palette().ACTIVE_ROW,
        text_color: palette().TEXT,
        outline_color: palette().ACCENT,
        corner_radius: top_radius,
    };
    dock.tab.inactive = TabInteractionStyle {
        bg_fill: palette().SURFACE_RAISED,
        text_color: palette().TEXT_MUTED,
        outline_color: Color32::TRANSPARENT,
        corner_radius: top_radius,
    };
    dock.tab.hovered = TabInteractionStyle {
        bg_fill: palette().HOVER_ROW,
        text_color: palette().TEXT,
        outline_color: palette().BORDER,
        corner_radius: top_radius,
    };
    dock.tab.inactive_with_kb_focus = dock.tab.inactive.clone();
    dock.tab.inactive_with_kb_focus.outline_color = palette().ACCENT;
    dock.tab.active_with_kb_focus = dock.tab.active.clone();
    dock.tab.focused_with_kb_focus = dock.tab.focused.clone();

    dock.tab.tab_body = TabBodyStyle {
        inner_margin: Margin::same(SP_2 as i8),
        stroke: Stroke::NONE,
        corner_radius: CornerRadius::ZERO,
        bg_fill: palette().CANVAS,
        hidden_tab_bar_drag_height: None,
    };

    dock.buttons.close_tab_color = palette().TEXT_MUTED;
    dock.buttons.close_tab_active_color = palette().TEXT;
    dock.buttons.close_tab_bg_fill = palette().HOVER_ROW;
    dock.buttons.add_tab_color = palette().TEXT_MUTED;
    dock.buttons.add_tab_active_color = palette().TEXT;
    dock.buttons.add_tab_bg_fill = palette().HOVER_ROW;
    dock.buttons.add_tab_border_color = palette().BORDER;
    dock.buttons.close_all_tabs_color = palette().TEXT_MUTED;
    dock.buttons.close_all_tabs_active_color = palette().TEXT;
    dock.buttons.close_all_tabs_bg_fill = palette().HOVER_ROW;
    dock.buttons.close_all_tabs_disabled_color = palette().TEXT_MUTED;
    dock.buttons.collapse_tabs_color = palette().TEXT_MUTED;
    dock.buttons.collapse_tabs_active_color = palette().TEXT;
    dock.buttons.collapse_tabs_bg_fill = palette().HOVER_ROW;
    dock.buttons.collapse_tabs_border_color = palette().BORDER;
    dock.buttons.minimize_window_color = palette().TEXT_MUTED;
    dock.buttons.minimize_window_active_color = palette().TEXT;
    dock.buttons.minimize_window_bg_fill = palette().HOVER_ROW;
    dock.buttons.minimize_window_border_color = palette().BORDER;
    dock.buttons.show_tab_bar_color = palette().TEXT_MUTED;
    dock.buttons.show_tab_bar_active_color = palette().TEXT;

    dock.overlay.selection_color = palette().ACCENT.gamma_multiply(0.35);
    dock.overlay.hovered_leaf_highlight.color = palette().ACCENT.gamma_multiply(0.15);
    dock
}

pub fn attention_tab_style(base: &TabStyle, color: Color32) -> TabStyle {
    let mut style = base.clone();
    style.inactive.text_color = color;
    style.inactive.outline_color = color.gamma_multiply(0.6);
    style.active.outline_color = color;
    style.hovered.text_color = color;
    style
}
