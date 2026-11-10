//! Settings failures show runtime reasons in English with the original on hover.

use egui_kittest::{Harness, kittest::Queryable};
use gui::model::codex_auth::CodexAuthModel;
use gui::model::provider_settings::ProviderSettingsModel;

fn modal(error: &str) -> Harness<'static, ProviderSettingsModel> {
    let mut settings = ProviderSettingsModel::default();
    settings.error = Some(error.to_owned());
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1200.0, 1000.0))
        .build_ui_state(
            |ui, settings| {
                gui::theme::install(ui.ctx());
                gui::panes::provider_settings::provider_settings_modal(
                    ui.ctx(),
                    settings,
                    &CodexAuthModel::default(),
                );
            },
            settings,
        );
    harness.run_steps(30);
    harness
}

// Given: the error a provider settings save reports when the rebuilt model is rejected
// (the RoutingError Display, Japanese inside an English wrapper)
// When: the settings modal shows it
// Then: the label is English and hovering it reveals the original text
#[test]
fn provider_settings_error_is_english_with_the_original_in_the_hover() {
    let raw = "invalid provider profile: provider type `openai` は api protocol `openai-responses` のみをサポートします (actual: openai-completions)。api_protocol を `openai-responses` に変更してください";
    let english = "invalid provider profile: provider type `openai` supports only api protocol `openai-responses` (actual: openai-completions); set api_protocol to `openai-responses`";
    let mut harness = modal(raw);
    assert!(harness.query_by_label(raw).is_none());
    harness.get_by_label(english).hover();
    harness.run_steps(5);
    harness.get_by_label(raw);
}

// Given: a reason in a language no template knows / When: shown
// Then: a generic English label, and the original is still reachable
#[test]
fn unknown_japanese_settings_error_stays_reachable_through_the_hover() {
    let raw = "未知の保存エラー";
    let mut harness = modal(raw);
    harness
        .get_by_label(gui::model::failure_text::UNTRANSLATED)
        .hover();
    harness.run_steps(5);
    harness.get_by_label(raw);
}
