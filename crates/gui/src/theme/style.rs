use egui::style::ScrollStyle;
use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Theme,
    ThemePreference, Visuals,
};

use super::tokens::*;

pub fn visuals() -> Visuals {
    let mut visuals = Visuals::dark();
    visuals.dark_mode = true;
    visuals.panel_fill = CANVAS;
    visuals.window_fill = OVERLAY;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.window_corner_radius = CornerRadius::same(R_LG);
    visuals.menu_corner_radius = CornerRadius::same(R_MD);
    visuals.extreme_bg_color = INPUT;
    visuals.faint_bg_color = SURFACE;
    visuals.code_bg_color = SURFACE_RAISED;
    visuals.text_edit_bg_color = Some(INPUT);
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    visuals.hyperlink_color = ACCENT;
    visuals.error_fg_color = ERROR_FG;
    visuals.warn_fg_color = WARNING_FG;
    visuals.weak_text_color = Some(TEXT_MUTED);

    visuals.widgets.noninteractive.bg_fill = SURFACE;
    visuals.widgets.noninteractive.weak_bg_fill = SURFACE;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.noninteractive.fg_stroke.color = TEXT_MUTED;
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(R_SM);

    visuals.widgets.inactive.weak_bg_fill = SURFACE_RAISED;
    visuals.widgets.inactive.bg_fill = SURFACE_RAISED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.fg_stroke.color = TEXT;
    visuals.widgets.inactive.corner_radius = CornerRadius::same(R_SM);

    visuals.widgets.hovered.weak_bg_fill = HOVER_ROW;
    visuals.widgets.hovered.fg_stroke.color = TEXT;
    visuals.widgets.hovered.bg_stroke.color = TEXT_MUTED;
    visuals.widgets.hovered.corner_radius = CornerRadius::same(R_SM);

    visuals.widgets.active.weak_bg_fill = ACTIVE_ROW;
    visuals.widgets.active.fg_stroke.color = TEXT;
    visuals.widgets.active.bg_stroke.color = ACCENT;
    visuals.widgets.active.corner_radius = CornerRadius::same(R_SM);

    visuals.widgets.open = visuals.widgets.hovered;
    visuals.button_frame = true;
    visuals.striped = false;
    visuals.window_shadow = egui::Shadow {
        offset: [0, 2],
        blur: 8,
        spread: 0,
        color: Color32::from_black_alpha(160),
    };
    visuals.popup_shadow = visuals.window_shadow;
    visuals
}

pub fn style() -> Style {
    let mut style = Style {
        visuals: visuals(),
        ..Default::default()
    };
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(FONT_SMALL, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Body,
        FontId::new(FONT_BODY, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(FONT_BODY, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(FONT_H1, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(FONT_MONO, FontFamily::Monospace),
    );
    style.text_styles.insert(
        text_style_h2(),
        FontId::new(FONT_H2, FontFamily::Proportional),
    );
    style.text_styles.insert(
        text_style_h3(),
        FontId::new(FONT_H3, FontFamily::Proportional),
    );
    style.text_styles.insert(
        text_style_h4(),
        FontId::new(FONT_H4, FontFamily::Proportional),
    );
    style.text_styles.insert(
        text_style_badge(),
        FontId::new(FONT_BADGE, FontFamily::Proportional),
    );

    style.spacing.item_spacing = egui::vec2(SP_2, SP_1);
    style.spacing.button_padding = egui::vec2(SP_2, SP_1);
    style.spacing.menu_margin = Margin::same(SP_2 as i8);
    style.spacing.window_margin = Margin::same(SP_2 as i8);
    style.spacing.indent = SP_4;
    style.spacing.interact_size = egui::vec2(40.0, TAB_HEIGHT);
    style.spacing.text_edit_width = 280.0;
    style.spacing.scroll = ScrollStyle::thin();
    style
}

pub fn install(ctx: &egui::Context) {
    ctx.set_theme(ThemePreference::Dark);
    ctx.set_style_of(Theme::Dark, style());
    // A stray light preference should still render the dark design.
    ctx.set_style_of(Theme::Light, style());
}
