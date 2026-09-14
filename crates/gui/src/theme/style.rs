use egui::style::ScrollStyle;
use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Theme,
    ThemePreference, Visuals,
};

use super::tokens::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePreset {
    Graphite,
    HighContrast,
    TokyoNight,
}

impl ThemePreset {
    pub const fn palette(self) -> Palette {
        match self {
            Self::Graphite | Self::HighContrast => Palette::graphite(),
            Self::TokyoNight => Palette::tokyo_night(),
        }
    }
}

pub fn visuals() -> Visuals {
    visuals_for(ThemePreset::Graphite)
}

pub fn visuals_for(preset: ThemePreset) -> Visuals {
    let p = preset.palette();
    let mut visuals = Visuals::dark();
    visuals.dark_mode = true;
    visuals.panel_fill = p.CANVAS;
    visuals.window_fill = p.OVERLAY;
    visuals.window_stroke = Stroke::new(1.0, p.BORDER);
    visuals.window_corner_radius = CornerRadius::same(R_LG);
    visuals.menu_corner_radius = CornerRadius::same(R_MD);
    visuals.extreme_bg_color = p.INPUT;
    visuals.faint_bg_color = p.SURFACE;
    visuals.code_bg_color = p.SURFACE_RAISED;
    visuals.text_edit_bg_color = Some(p.INPUT);
    visuals.selection.bg_fill = match preset {
        ThemePreset::Graphite => p.ACCENT,
        ThemePreset::HighContrast => Color32::from_rgb(255, 190, 0),
        ThemePreset::TokyoNight => p.SELECTED_ROW,
    };
    visuals.selection.stroke = Stroke::new(1.0, p.ACCENT);
    visuals.hyperlink_color = p.ACCENT;
    visuals.error_fg_color = p.ERROR_FG;
    visuals.warn_fg_color = p.WARNING_FG;
    visuals.weak_text_color = Some(p.TEXT_MUTED);

    visuals.widgets.noninteractive.bg_fill = p.SURFACE;
    visuals.widgets.noninteractive.weak_bg_fill = p.SURFACE;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.BORDER);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.TEXT_MUTED);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(R_SM);

    visuals.widgets.inactive.weak_bg_fill = p.SURFACE_RAISED;
    visuals.widgets.inactive.bg_fill = p.SURFACE_RAISED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, p.BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, p.TEXT);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(R_SM);

    // Pin legacy egui fills explicitly; Night must never inherit neutral defaults.
    visuals.widgets.hovered.bg_fill = match preset {
        ThemePreset::Graphite | ThemePreset::HighContrast => Color32::from_gray(70),
        ThemePreset::TokyoNight => p.HOVER_ROW,
    };
    visuals.widgets.hovered.weak_bg_fill = p.HOVER_ROW;
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.5, p.TEXT);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, p.TEXT_MUTED);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(R_SM);

    visuals.widgets.active.bg_fill = match preset {
        ThemePreset::Graphite | ThemePreset::HighContrast => Color32::from_gray(55),
        ThemePreset::TokyoNight => p.ACTIVE_ROW,
    };
    visuals.widgets.active.weak_bg_fill = p.ACTIVE_ROW;
    visuals.widgets.active.fg_stroke = Stroke::new(2.0, p.TEXT);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, p.ACCENT);
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
    style_for(ThemePreset::Graphite)
}

pub fn style_for(preset: ThemePreset) -> Style {
    let mut style = Style {
        visuals: visuals_for(preset),
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
    install_preset(ctx, ThemePreset::Graphite);
}

pub fn install_preset(ctx: &egui::Context, preset: ThemePreset) {
    install_palette(preset.palette());
    ctx.set_theme(ThemePreference::Dark);
    ctx.set_style_of(Theme::Dark, style_for(preset));
    // A stray light preference should still render the dark design.
    ctx.set_style_of(Theme::Light, style_for(preset));
    super::fonts::install(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visuals_for_tokyo_night_sets_all_roles() {
        // Given: a preset, without installing any process-global palette.
        let p = Palette::tokyo_night();
        let graphite = visuals_for(ThemePreset::Graphite);
        // When: its visuals are built directly.
        let v = visuals_for(ThemePreset::TokyoNight);
        // Then: all painted roles derive from that preset, not egui or global state.
        assert!(v.dark_mode);
        for (actual, expected, legacy) in [
            (v.panel_fill, p.CANVAS, graphite.panel_fill),
            (v.window_fill, p.OVERLAY, graphite.window_fill),
            (v.extreme_bg_color, p.INPUT, graphite.extreme_bg_color),
            (v.faint_bg_color, p.SURFACE, graphite.faint_bg_color),
            (v.code_bg_color, p.SURFACE_RAISED, graphite.code_bg_color),
            (
                v.selection.bg_fill,
                p.SELECTED_ROW,
                graphite.selection.bg_fill,
            ),
            (v.hyperlink_color, p.ACCENT, graphite.hyperlink_color),
            (v.error_fg_color, p.ERROR_FG, graphite.error_fg_color),
            (v.warn_fg_color, p.WARNING_FG, graphite.warn_fg_color),
        ] {
            assert_eq!(actual, expected);
            assert_ne!(actual, legacy);
        }
        assert_eq!(v.window_stroke, Stroke::new(1.0, p.BORDER));
        assert_eq!(v.selection.stroke, Stroke::new(1.0, p.ACCENT));
        assert_eq!(v.text_edit_bg_color, Some(p.INPUT));
        assert_eq!(v.weak_text_color, Some(p.TEXT_MUTED));
        for (state, legacy, fill, weak, border, text, width) in [
            (
                v.widgets.noninteractive,
                graphite.widgets.noninteractive,
                p.SURFACE,
                p.SURFACE,
                p.BORDER,
                p.TEXT_MUTED,
                1.0,
            ),
            (
                v.widgets.inactive,
                graphite.widgets.inactive,
                p.SURFACE_RAISED,
                p.SURFACE_RAISED,
                p.BORDER,
                p.TEXT,
                1.0,
            ),
            (
                v.widgets.hovered,
                graphite.widgets.hovered,
                p.HOVER_ROW,
                p.HOVER_ROW,
                p.TEXT_MUTED,
                p.TEXT,
                1.5,
            ),
            (
                v.widgets.active,
                graphite.widgets.active,
                p.ACTIVE_ROW,
                p.ACTIVE_ROW,
                p.ACCENT,
                p.TEXT,
                2.0,
            ),
            (
                v.widgets.open,
                graphite.widgets.open,
                p.HOVER_ROW,
                p.HOVER_ROW,
                p.TEXT_MUTED,
                p.TEXT,
                1.5,
            ),
        ] {
            assert_eq!(state.bg_fill, fill);
            assert_eq!(state.weak_bg_fill, weak);
            assert_eq!(state.bg_stroke, Stroke::new(1.0, border));
            assert_eq!(state.fg_stroke, Stroke::new(width, text));
            assert_ne!(state.bg_fill, legacy.bg_fill);
        }
        assert_eq!(style_for(ThemePreset::TokyoNight).visuals, v);
    }
}
