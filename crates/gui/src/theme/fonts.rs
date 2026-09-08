//! Runtime font discovery for CJK-capable GUI rendering.
//!
//! egui's default font set (Hack/Ubuntu) has no Japanese glyphs, so CJK
//! strings render as tofu. This module resolves a system CJK sans font via
//! fontconfig and prepends it to both the proportional and monospace
//! families so Japanese thread names, project names, goal text, and error
//! messages render natively.

use std::sync::Arc;

pub const FONT_QUERY: &str = "sans:lang=ja";
pub const CJK_FONT_NAME: &str = "system-cjk-sans";

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    match resolve_cjk_font() {
        Some(bytes) => {
            fonts.font_data.insert(
                CJK_FONT_NAME.to_owned(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                let candidates = fonts.families.entry(family).or_insert_with(Vec::new);
                candidates.insert(0, CJK_FONT_NAME.to_owned());
            }
            ctx.set_fonts(fonts);
            tracing::info!("installed system CJK font (fontconfig query: {FONT_QUERY})");
        }
        None => {
            tracing::warn!(
                "no system font matched fontconfig query {FONT_QUERY:?}; CJK text may show as tofu. Install Noto Sans CJK or Takao."
            );
        }
    }
}

fn resolve_cjk_font() -> Option<Vec<u8>> {
    let output = std::process::Command::new("fc-match")
        .args(["--format=%{file}", FONT_QUERY])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if path.is_empty() || !output.status.success() {
        return None;
    }
    std::fs::read(&path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_query_targets_japanese_sans() {
        assert!(FONT_QUERY.contains("ja"));
        assert!(FONT_QUERY.starts_with("sans:"));
    }

    #[test]
    fn prepends_cjk_font_to_both_families() {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            CJK_FONT_NAME.to_owned(),
            Arc::new(egui::FontData::from_static(&[0])),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_insert_with(Vec::new)
                .insert(0, CJK_FONT_NAME.to_owned());
        }
        for family in [
            &egui::FontFamily::Proportional,
            &egui::FontFamily::Monospace,
        ] {
            let first = fonts
                .families
                .get(family)
                .and_then(|v| v.first())
                .map(String::as_str);
            assert_eq!(first, Some(CJK_FONT_NAME));
        }
        assert!(fonts.font_data.contains_key(CJK_FONT_NAME));
    }
}
