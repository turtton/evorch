//! Phosphor icon glyphs.
//!
//! Icons are font glyphs, so they mix into labels, buttons and dock tab
//! titles and follow the surrounding text color. Keep accessible labels on
//! icon-only controls through [`super::widgets::icon_button`].

use egui::{FontFamily, RichText};

pub use egui_phosphor::regular::*;

use super::fonts::ICON_FILL_FONT_NAME;

/// Renders `icon` from the filled Phosphor set (e.g. an active pin).
///
/// Falls back to the outline glyph until the theme fonts are installed
/// (fonts apply from the next frame, and bare test harnesses skip them).
pub fn filled(ui: &egui::Ui, icon: &str) -> RichText {
    let family = FontFamily::Name(ICON_FILL_FONT_NAME.into());
    let bound = ui
        .ctx()
        .fonts(|fonts| fonts.definitions().families.contains_key(&family));
    let text = RichText::new(icon);
    if bound { text.family(family) } else { text }
}

/// `icon` followed by `text`, separated by a regular space.
pub fn with_icon(icon: &str, text: impl AsRef<str>) -> String {
    format!("{icon} {}", text.as_ref())
}
