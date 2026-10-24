use egui_kittest::{
    Harness,
    kittest::{By, Queryable},
};
use gui::model::codex_auth::CodexAuthModel;
use gui::model::provider_settings::{ProviderKind, ProviderSettingsModel};

#[test]
#[ignore = "writes Codex model editor PNG evidence using an offscreen adapter"]
fn capture_codex_model_editor() {
    // Given
    for (width, height) in [(1200_u16, 900_u16), (800, 600)] {
        for state_name in ["idle", "loaded", "failed"] {
            let mut settings = ProviderSettingsModel::default();
            settings.open = true;
            settings.add(ProviderKind::CodexSubscription);
            let editor = settings.codex_mut().unwrap();
            match state_name {
                "loaded" => {
                    editor.fetch.models_fetch_state =
                        gui::model::provider_settings::ModelsFetchState::Loaded;
                    editor.fetch.available_models =
                        Some(vec!["gpt-new".into(), "gpt-6-astra".into()]);
                    editor.fetch.fetch_selected.insert("gpt-new".into());
                }
                "failed" => {
                    editor.fetch.models_fetch_state =
                        gui::model::provider_settings::ModelsFetchState::Failed(
                            "Codex access token expired; Sign in again".into(),
                        )
                }
                _ => {}
            }
            let state = gui::app::WorkbenchState::new(
                gui::fixture::DemoSource(Vec::new()),
                &workspace_ui::UiSettings::default(),
            )
            .unwrap()
            .with_provider_settings(settings);
            let mut harness =
                gui::headless::HeadlessWorkbench::new(state, [f32::from(width), f32::from(height)]);
            // When
            harness.run();
            // Then
            assert!(harness.has_label("Configured models"));
            let frame = harness.capture().unwrap();
            frame
                .save_png(std::path::Path::new(&format!(
                    "/tmp/opencode/codex-models-{state_name}-{width}x{height}.png",
                )))
                .unwrap();
        }
    }
}

fn editor() -> Harness<'static, ProviderSettingsModel> {
    let mut model = ProviderSettingsModel::default();
    model.add(ProviderKind::CodexSubscription);
    Harness::builder()
        .with_size(egui::vec2(1200.0, 900.0))
        .build_ui_state(
            |ui, model| {
                let ctx = ui.ctx();
                gui::theme::install(ctx);
                gui::panes::provider_settings::provider_settings_modal(
                    ctx,
                    model,
                    &CodexAuthModel::default(),
                );
            },
            model,
        )
}

#[test]
fn adds_trimmed_model_when_add_is_clicked() {
    // Given
    let mut harness = editor();
    harness.run_steps(16);
    harness.get_by_label("Add model").click();
    harness.run_steps(16);
    harness
        .get_by_label("Add model")
        .type_text("  custom-gpt  ");
    harness.run_steps(16);
    // When
    harness.get_by_label("Add").click();
    harness.run_steps(16);
    // Then
    assert_eq!(
        harness
            .state_mut()
            .codex_mut()
            .unwrap()
            .models
            .last()
            .unwrap(),
        "custom-gpt"
    );
}

#[test]
fn rejects_duplicate_when_add_is_clicked() {
    // Given
    let mut harness = editor();
    harness.run_steps(16);
    harness.get_by_label("Add model").click();
    harness.run_steps(16);
    harness.get_by_label("Add model").type_text(" gpt-6-astra ");
    harness.run_steps(16);
    // When
    harness.get_by_label("Add").click();
    harness.run_steps(16);
    // Then
    assert_eq!(harness.state_mut().codex_mut().unwrap().models.len(), 5);
    assert!(harness.query_by_label("Model already added").is_some());
}

#[test]
fn ignores_whitespace_when_add_is_clicked() {
    // Given
    let mut harness = editor();
    harness.run_steps(16);
    harness.get_by_label("Add model").click();
    harness.run_steps(16);
    harness.get_by_label("Add model").type_text("   ");
    harness.run_steps(16);
    // When
    harness.get_by_label("Add").click();
    harness.run_steps(16);
    // Then
    assert_eq!(harness.state_mut().codex_mut().unwrap().models.len(), 5);
}

#[test]
fn selects_remaining_default_when_current_default_is_removed() {
    // Given
    let mut harness = editor();
    harness.run_steps(16);
    // When
    harness.get_by_label("Remove gpt-6-astra").click();
    harness.run_steps(16);
    // Then
    let editor = harness.state_mut().codex_mut().unwrap();
    assert!(!editor.models.iter().any(|id| id == "gpt-6-astra"));
    assert_eq!(editor.default_model, "gpt-5.6-sol");
}

#[test]
fn selects_configured_model_when_default_choice_is_clicked() {
    // Given
    let mut harness = editor();
    harness.run_steps(16);
    harness.get_by_label("Default model").click();
    harness.run_steps(16);
    // When
    harness
        .query(
            By::new()
                .role(egui::accesskit::Role::Button)
                .label("gpt-5.5"),
        )
        .unwrap()
        .click();
    harness.run_steps(16);
    // Then
    assert_eq!(
        harness.state_mut().codex_mut().unwrap().default_model,
        "gpt-5.5"
    );
}

#[test]
fn refresh_reports_failure_when_store_is_unavailable() {
    // Given
    let mut settings = ProviderSettingsModel::default();
    settings.add(ProviderKind::CodexSubscription);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1200.0, 900.0))
        .build_ui_state(
            |ui, settings| {
                gui::theme::install(ui.ctx());
                if gui::panes::provider_settings::provider_settings_modal(
                    ui.ctx(),
                    settings,
                    &CodexAuthModel::default(),
                ) == Some(gui::panes::provider_settings::ProviderSettingsAction::RefreshModels)
                {
                    settings.start_models_fetch_with_store(None);
                }
            },
            settings,
        );
    harness.run_steps(16);
    // When
    harness.get_by_label("Refresh models").click();
    harness.run_steps(16);
    // Then
    use gui::model::provider_settings::ModelsFetchState;
    let settings = harness.state_mut();
    let editor = settings.codex_mut().unwrap();
    assert_eq!(editor.fetch.models_fetch_state, ModelsFetchState::Loading);
    let result = editor
        .fetch
        .models_rx
        .take()
        .unwrap()
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(result).unwrap();
    editor.fetch.models_rx = Some(rx);
    settings.poll_models();
    harness.run_steps(16);
    assert!(matches!(
        harness
            .state_mut()
            .codex_mut()
            .unwrap()
            .fetch
            .models_fetch_state,
        ModelsFetchState::Failed(_)
    ));
    assert!(harness.query_by_label("Add model").is_some());
}

#[test]
fn applies_fetched_checkbox_selection_when_apply_is_clicked() {
    // Given
    let mut harness = editor();
    let editor = harness.state_mut().codex_mut().unwrap();
    editor.fetch.models_fetch_state = gui::model::provider_settings::ModelsFetchState::Loaded;
    editor.fetch.available_models = Some(vec!["gpt-new".into(), "gpt-6-astra".into()]);
    harness.run_steps(16);
    harness.get_by_label("gpt-new").click();
    harness.run_steps(16);
    // When
    harness.get_by_label("Apply selected (1)").click();
    harness.run_steps(16);
    // Then
    assert_eq!(
        harness
            .state_mut()
            .codex_mut()
            .unwrap()
            .models
            .last()
            .unwrap(),
        "gpt-new"
    );
    assert!(harness.query_by_label("gpt-new · Already added").is_some());
}

#[test]
fn shows_fetched_context_window_and_override_control() {
    let mut harness = editor();
    harness
        .state_mut()
        .codex_mut()
        .unwrap()
        .fetch
        .context_windows
        .insert("gpt-6-astra".into(), 272_000);
    harness.run_steps(16);
    assert!(harness.query_by_label("Provider: 272000").is_some());
    assert!(
        harness
            .query_all(By::new().label("Override"))
            .next()
            .is_some()
    );
}

#[test]
fn offers_the_fixed_image_model_and_keeps_it_out_of_the_default() {
    // Given: a new Codex profile; its catalog never lists image models.
    let mut harness = editor();
    harness.run_steps(16);
    // When: the fixed image model is added.
    harness.get_by_label("Add gpt-image-2").click();
    harness.run_steps(16);
    // Then: it is configured as an image model, and the button is gone.
    let editor = harness.state_mut().codex_mut().unwrap();
    assert_eq!(editor.models.last().unwrap(), "gpt-image-2");
    assert_eq!(
        editor.model_entries["gpt-image-2"].kind,
        Some(config::ModelKind::ImageGeneration)
    );
    assert_eq!(editor.default_model, "gpt-6-astra");
    assert!(harness.query_by_label("Add gpt-image-2").is_none());
    assert_eq!(
        harness.get_by_label("Kind gpt-image-2").value().as_deref(),
        Some("Image generation")
    );
    // When: the default model picker opens.
    harness.get_by_label("Default model").click();
    harness.run_steps(16);
    // Then: the image model is not a default choice.
    assert!(
        harness
            .query(
                By::new()
                    .role(egui::accesskit::Role::Button)
                    .label("gpt-image-2"),
            )
            .is_none()
    );
    assert!(
        harness
            .query(
                By::new()
                    .role(egui::accesskit::Role::Button)
                    .label("gpt-5.5"),
            )
            .is_some()
    );
}

#[test]
fn typed_models_record_their_inferred_kind_and_the_kind_can_be_changed() {
    // Given
    let mut harness = editor();
    harness.run_steps(16);
    harness.get_by_label("Add model").click();
    harness.run_steps(16);
    harness.get_by_label("Add model").type_text("gpt-image-1");
    harness.run_steps(16);
    // When
    harness.get_by_label("Add").click();
    harness.run_steps(16);
    harness.get_by_label("Kind gpt-5.5").click();
    harness.run_steps(16);
    harness.get_by_label("Image generation").click();
    harness.run_steps(16);
    // Then
    let editor = harness.state_mut().codex_mut().unwrap();
    assert_eq!(
        editor.model_entries["gpt-image-1"].kind,
        Some(config::ModelKind::ImageGeneration)
    );
    assert_eq!(
        editor.model_entries["gpt-5.5"].kind,
        Some(config::ModelKind::ImageGeneration)
    );
    assert!(
        !editor
            .default_model_choices()
            .contains(&"gpt-5.5".to_owned())
    );
    // Models the user did not touch keep no explicit kind.
    assert!(
        editor
            .model_entries
            .get("gpt-6-astra")
            .is_none_or(|entry| entry.kind.is_none())
    );
}
