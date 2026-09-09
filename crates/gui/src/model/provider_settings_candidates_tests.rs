use super::*;
use config::types::provider::ModelEntryConfig;

fn editor() -> ProviderSettingsModel {
    ProviderSettingsModel {
        base_url: "https://example.com/v1".into(),
        models: vec![
            ModelEntryConfig::enabled("a"),
            ModelEntryConfig::enabled("b"),
        ],
        default_model: "a".into(),
        ..Default::default()
    }
}

#[test]
fn add_model_trims_and_rejects_duplicates_with_already_added() {
    let mut model = editor();
    model.add_model(" c ").unwrap();
    assert_eq!(model.add_model(" c "), Err("Already added".into()));
    assert_eq!(model.models, ["a", "b", "c"]);
    assert!(model.add_model(" \t ").is_err());
}

#[test]
fn remove_model_picks_first_enabled() {
    let mut model = editor();
    model.remove_model(0).unwrap();
    assert_eq!(model.default_model, "b");
    assert_eq!(model.models, ["b"]);
}

#[test]
fn disable_default_picks_first_enabled() {
    let mut model = editor();
    model.set_model_enabled(0, false).unwrap();
    assert_eq!(model.default_model, "b");
    assert!(!model.models[0].enabled);
}

#[test]
fn last_enabled_clears_default_and_fails_validation() {
    let mut model = editor();
    model.set_model_enabled(1, false).unwrap();
    model.set_model_enabled(0, false).unwrap();
    assert!(model.default_model.is_empty());
    assert_eq!(
        model.validation_error.as_deref(),
        Some("at least one enabled model required")
    );
    assert!(config::validate_openai_compatible_provider_input(&model.to_input()).is_err());
}

#[test]
fn enable_recovers_default_and_validation() {
    let mut model = editor();
    model.set_model_enabled(0, false).unwrap();
    model.set_model_enabled(1, false).unwrap();
    model.set_model_enabled(0, true).unwrap();
    assert_eq!(model.default_model, "a");
    assert!(model.validation_error.is_none());
}

#[test]
fn remove_last_enabled_clears_default() {
    let mut model = editor();
    model.set_model_enabled(1, false).unwrap();
    model.remove_model(0).unwrap();
    assert!(model.default_model.is_empty());
    assert!(model.validation_error.is_some());
}

#[test]
fn rename_model_updates_default_model_reference() {
    let mut model = editor();
    model.rename_model(0, " renamed ").unwrap();
    assert_eq!(model.default_model, "renamed");
    assert_eq!(model.models[0].id, "renamed");
    assert_eq!(model.rename_model(0, "b"), Err("Already added".into()));
}

#[test]
fn apply_fetched_selection_adds_only_selected_new_ids_and_clears_selection() {
    let mut model = editor();
    model.available_models = Some(vec!["a".into(), "c".into(), "d".into()]);
    model.selection_toggle("a");
    model.selection_toggle("c");
    model.selection_toggle("d");
    model.selection_toggle("d");
    model.apply_fetched_selection();
    assert_eq!(model.models, ["a", "b", "c"]);
    assert!(model.fetch_selected.is_empty());
}

#[test]
fn is_added_reflects_configured_ids_including_disabled() {
    let mut model = editor();
    model.set_model_enabled(0, false).unwrap();
    assert!(model.is_added("a"));
    assert!(!model.is_added("new"));
}

#[test]
fn candidate_models_are_only_enabled_configured_models() {
    let mut model = editor();
    model.available_models = Some(vec!["fetched".into()]);
    model.set_model_enabled(0, false).unwrap();
    assert_eq!(model.candidate_models(), ["b"]);
}

#[test]
fn candidates_preserve_first_id_order_with_enabled_duplicate_priority() {
    let mut model = editor();
    model.models[0].enabled = false;
    model.models.push(ModelEntryConfig::enabled("a"));
    let candidates = model.candidate_models();
    assert_eq!(candidates, ["a", "b"]);
}

#[test]
fn seed_from_config_preserves_enabled_flags() {
    let mut config = config::Config::default();
    let mut profile = config::ProviderProfileConfig {
        provider_type: config::ProviderTypeConfig::OpenAiCompatible,
        models: editor().models,
        default_model: "b".into(),
        ..Default::default()
    };
    profile.models[0].enabled = false;
    config.providers.insert("test".into(), profile.clone());
    let model = ProviderSettingsModel::seed_from_config(&config);
    assert_eq!(model.models, profile.models);
}

#[test]
fn to_input_emits_model_entries_with_flags() {
    let mut model = editor();
    model.set_model_enabled(0, false).unwrap();
    let input = model.to_input();
    assert_eq!(input.models, model.models);
    assert!(config::validate_openai_compatible_provider_input(&input).is_ok());
}
