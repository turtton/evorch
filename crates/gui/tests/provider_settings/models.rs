use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use gui::model::codex_auth::CodexAuthModel;
use gui::model::provider_settings::{
    ModelsFetchState, OpenAiEditorModel, ProfileEditor, ProviderSettingsModel,
};

fn harness(loaded: bool) -> Harness<'static, ProviderSettingsModel> {
    let mut editor = OpenAiEditorModel::default();
    editor.add_model("first").unwrap();
    editor.add_model("second").unwrap();
    if loaded {
        editor.available_models = Some(vec!["first".into(), "fetched".into()]);
        editor.models_fetch_state = ModelsFetchState::Loaded;
    }
    let mut settings = ProviderSettingsModel::default();
    settings.editor = Some(ProfileEditor::OpenAiCompatible(editor));
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
    harness.run();
    harness.run_steps(30);
    harness
}

fn enter(h: &mut Harness<'_, ProviderSettingsModel>, text: &str) {
    h.get_by_label("Add model").focus();
    h.get_by_label("Add model").type_text(text);
    h.run();
    h.key_press(egui::Key::Enter);
    h.run();
}

#[test]
fn type_into_add_model_and_press_enter_adds_row_with_enable_checked() {
    // Given
    let mut h = harness(false);
    // When
    enter(&mut h, "new-model");
    // Then
    assert_eq!(
        h.get_by_label("Enable new-model")
            .accesskit_node()
            .toggled(),
        Some(egui::accesskit::Toggled::True)
    );
}

#[test]
fn add_duplicate_shows_already_added_inline() {
    // Given
    let mut h = harness(false);
    // When
    enter(&mut h, "first");
    // Then
    assert!(h.query_by_label("Already added").is_some());
    assert_eq!(h.state().openai().unwrap().models.len(), 2);
}

#[test]
fn remove_row_removes_model_and_updates_default_combo() {
    // Given
    let mut h = harness(false);
    // When
    h.get_by_label("Remove first").click();
    h.run();
    // Then
    assert!(h.query_by_label("Enable first").is_none());
    assert_eq!(h.state().openai().unwrap().default_model, "second");
    h.get_by_label("Default model").click();
    h.run();
    assert!(h.query_by_label("first").is_none());
}

#[test]
fn uncheck_enable_default_moves_default_to_next_enabled() {
    // Given
    let mut h = harness(false);
    // When
    h.get_by_label("Enable first").click();
    h.run();
    // Then
    assert_eq!(h.state().openai().unwrap().default_model, "second");
    assert_eq!(h.state().openai().unwrap().candidate_models(), ["second"]);
}

#[test]
fn fetch_apply_adds_selected_to_configured_and_clears_selection() {
    // Given
    let mut h = harness(true);
    h.get_by_label("fetched").click();
    h.run();
    // When
    h.get_by_label("Apply selected (1)").click();
    h.run();
    // Then
    assert!(h.query_by_label("Enable fetched").is_some());
    assert!(h.state().openai().unwrap().fetch_selected.is_empty());
    assert_eq!(
        h.state().openai().unwrap().models,
        ["first", "second", "fetched"]
    );
}

#[test]
fn fetch_panel_shows_already_added_for_configured_models() {
    // Given / When
    let h = harness(true);
    // Then
    assert!(h.query_by_label("first · Already added").is_some());
}

#[test]
fn apply_button_disabled_when_no_selection() {
    // Given / When
    let h = harness(true);
    // Then
    assert!(
        h.get_by_label("Apply selected (0)")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn enter_in_add_model_adds_row() {
    // Given
    let mut h = harness(false);
    // When
    enter(&mut h, "日本語モデル");
    // Then
    assert!(h.query_by_label("Edit 日本語モデル").is_some());
    assert!(h.query_by_label("Remove 日本語モデル").is_some());
}

#[test]
fn add_button_adds_model() {
    // Given
    let mut h = harness(false);
    h.get_by_label("Add model").focus();
    h.get_by_label("Add model").type_text("third");
    h.run();
    // When
    h.get_by_label("Add").click();
    h.run();
    // Then
    assert!(h.query_by_label("Enable third").is_some());
}

#[test]
fn rename_default_updates_row_and_default() {
    // Given
    let mut h = harness(false);
    h.get_by_label("Edit first").click();
    h.run();
    h.get_by_label("Model ID first").focus();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.run();
    h.get_by_label("Model ID first").type_text("renamed");
    h.run();
    // When
    h.get_by_label("Done first").click();
    h.run();
    // Then
    assert!(h.query_by_label("Enable renamed").is_some());
    assert_eq!(h.state().openai().unwrap().default_model, "renamed");
}

#[test]
#[ignore = "writes offscreen model management PNG evidence"]
fn capture_model_management() {
    // Given
    let mut h = harness(true);
    // When / Then
    h.render()
        .unwrap()
        .save("/tmp/opencode/models-fetched.png")
        .unwrap();
    h.get_by_label("fetched").click();
    h.run();
    h.render()
        .unwrap()
        .save("/tmp/opencode/models-selected.png")
        .unwrap();
    h.get_by_label("Apply selected (1)").click();
    h.run();
    assert!(h.query_by_label("Enable fetched").is_some());
    h.get_by_label("Enable first").click();
    h.run();
    h.get_by_label("Default model").click();
    h.run();
    assert_eq!(h.query_all_by_label("first").count(), 1);
    h.render()
        .unwrap()
        .save("/tmp/opencode/models-applied-default.png")
        .unwrap();
}

#[test]
fn configured_models_use_natural_height_when_content_exceeds_half_viewport() {
    // Given: enough rows to exceed the former half-viewport inner scroll limit.
    let mut editor = OpenAiEditorModel::default();
    for index in 0..12 {
        editor.add_model(&format!("model-{index}")).unwrap();
    }
    let presets = Default::default();
    let mut h = Harness::builder()
        .with_size(egui::vec2(1200.0, 1000.0))
        .build_ui(move |ui| {
            gui::theme::install(ui.ctx());
            gui::panes::provider_models::provider_models(
                ui,
                &mut editor,
                &gui::model::model_metadata::MetadataSources {
                    presets: &presets,
                    catalog: None,
                },
            );
        });
    // When: the model form lays out all rows.
    h.run();
    // Then: adding a model follows the complete list, not a nested scroll viewport.
    let last = h.get_by_label("Enable model-11").rect();
    let add = h.get_by_label("Add model").rect();
    assert!(add.top() > last.bottom());
}
