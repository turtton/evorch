use egui::RichText;

use super::tokens::{FONT_BADGE, FONT_H2, FONT_H3, FONT_H4, FONT_SMALL, TEXT, TEXT_MUTED};

pub fn h2(text: impl Into<String>) -> RichText {
    RichText::new(text).size(FONT_H2).color(TEXT)
}

pub fn h3(text: impl Into<String>) -> RichText {
    RichText::new(text).size(FONT_H3).color(TEXT)
}

pub fn h4(text: impl Into<String>) -> RichText {
    RichText::new(text).size(FONT_H4).color(TEXT).strong()
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).color(TEXT_MUTED).size(FONT_SMALL)
}

pub fn badge(text: impl Into<String>) -> RichText {
    RichText::new(text).size(FONT_BADGE).color(TEXT)
}
