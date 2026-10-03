//! Font stack: bundled Inter for UI text, Phosphor for icons, and a system
//! CJK font as fallback.
//!
//! Inter ships as a variable font, so weights are selected per text through
//! the `wght` axis (see [`super::text`]) instead of separate font files.
//! egui's default font set (Hack/Ubuntu) has no Japanese glyphs, so CJK
//! strings would render as tofu. A system CJK sans font is resolved via
//! fontconfig and inserted right after the primary text font of each family,
//! so Latin glyphs keep Inter/Hack metrics while Japanese thread names,
//! project names, goal text, and error messages still render natively.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

pub const FONT_QUERY: &str = "sans:lang=ja";
pub const CJK_FONT_NAME: &str = "system-cjk-sans";
pub const INTER_FONT_NAME: &str = "inter";
pub const ICON_FONT_NAME: &str = "phosphor";
/// Filled Phosphor glyphs (same codepoints as the outline set), selected by
/// family through [`super::icons::filled`].
pub const ICON_FILL_FONT_NAME: &str = "phosphor-fill";

const INTER: &[u8] = include_bytes!("../../assets/fonts/InterVariable.ttf");

pub fn install(ctx: &egui::Context) {
    let cjk = resolve_cjk_font();
    if cjk.is_some() {
        tracing::info!("installed system CJK font (fontconfig query: {FONT_QUERY})");
    } else {
        tracing::warn!(
            "no system font matched fontconfig query {FONT_QUERY:?}; CJK text may show as tofu. Install Noto Sans CJK or Takao."
        );
    }
    ctx.set_fonts(definitions(cjk));
}

fn definitions(cjk: Option<Vec<u8>>) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        INTER_FONT_NAME.to_owned(),
        Arc::new(FontData::from_static(INTER)),
    );
    fonts.font_data.insert(
        ICON_FONT_NAME.to_owned(),
        Arc::new(egui_phosphor::Variant::Regular.font_data()),
    );
    fonts.font_data.insert(
        ICON_FILL_FONT_NAME.to_owned(),
        Arc::new(egui_phosphor::Variant::Fill.font_data()),
    );
    if let Some(bytes) = cjk {
        fonts.font_data.insert(
            CJK_FONT_NAME.to_owned(),
            Arc::new(FontData::from_owned(bytes)),
        );
    }
    let has_cjk = fonts.font_data.contains_key(CJK_FONT_NAME);

    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(0, INTER_FONT_NAME.to_owned());
    proportional.insert(1, ICON_FONT_NAME.to_owned());
    if has_cjk {
        proportional.insert(2, CJK_FONT_NAME.to_owned());
    }

    // Monospace keeps egui's Hack first so code columns stay aligned.
    let monospace = fonts.families.entry(FontFamily::Monospace).or_default();
    let after_primary = monospace.len().min(1);
    monospace.insert(after_primary, ICON_FONT_NAME.to_owned());
    if has_cjk {
        monospace.insert(after_primary + 1, CJK_FONT_NAME.to_owned());
    }

    // The filled set resolves icons first and falls back to the text stack.
    let mut filled = vec![ICON_FILL_FONT_NAME.to_owned()];
    filled.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
    fonts
        .families
        .insert(FontFamily::Name(ICON_FILL_FONT_NAME.into()), filled);
    fonts
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
    fn text_fonts_lead_and_cjk_falls_back() {
        // Given: a CJK font is available.
        let fonts = definitions(Some(INTER.to_vec()));
        // Then: Inter/Hack keep Latin metrics, icons and CJK follow as fallbacks.
        let proportional = &fonts.families[&FontFamily::Proportional];
        assert_eq!(
            &proportional[..3],
            [INTER_FONT_NAME, ICON_FONT_NAME, CJK_FONT_NAME]
        );
        let monospace = &fonts.families[&FontFamily::Monospace];
        assert_eq!(monospace[0], "Hack");
        assert_eq!(&monospace[1..3], [ICON_FONT_NAME, CJK_FONT_NAME]);
        let filled = &fonts.families[&FontFamily::Name(ICON_FILL_FONT_NAME.into())];
        assert_eq!(filled[0], ICON_FILL_FONT_NAME);
    }

    #[test]
    fn missing_cjk_font_still_installs_text_and_icon_fonts() {
        let fonts = definitions(None);
        let proportional = &fonts.families[&FontFamily::Proportional];
        assert_eq!(&proportional[..2], [INTER_FONT_NAME, ICON_FONT_NAME]);
        assert!(!proportional.iter().any(|name| name == CJK_FONT_NAME));
    }
}
