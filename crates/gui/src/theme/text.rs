use egui::RichText;

use super::tokens::{FONT_BADGE, FONT_H2, FONT_H3, FONT_H4, FONT_SMALL, palette};

/// Inter `wght` axis values. Body text keeps the font default (400).
pub const WEIGHT_MEDIUM: f32 = 500.0;
pub const WEIGHT_SEMIBOLD: f32 = 600.0;

pub fn medium(text: impl Into<String>) -> RichText {
    RichText::new(text).variation("wght", WEIGHT_MEDIUM)
}

pub fn semibold(text: impl Into<String>) -> RichText {
    RichText::new(text).variation("wght", WEIGHT_SEMIBOLD)
}

pub fn h2(text: impl Into<String>) -> RichText {
    semibold(text).size(FONT_H2).color(palette().TEXT)
}

pub fn h3(text: impl Into<String>) -> RichText {
    semibold(text).size(FONT_H3).color(palette().TEXT)
}

pub fn h4(text: impl Into<String>) -> RichText {
    semibold(text).size(FONT_H4).color(palette().TEXT).strong()
}

/// Muted label for sidebar section headings.
pub fn section(text: impl Into<String>) -> RichText {
    medium(text).size(FONT_SMALL).color(palette().TEXT_MUTED)
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .color(palette().TEXT_MUTED)
        .size(FONT_SMALL)
}

pub fn badge(text: impl Into<String>) -> RichText {
    medium(text).size(FONT_BADGE).color(palette().TEXT)
}
