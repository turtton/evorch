use super::*;

#[test]
fn candidate_models_excludes_fetched_ids_when_filter_changes() {
    // Given
    let mut model = ProviderSettingsModel {
        available_models: Some(vec!["a".into(), "b".into()]),
        models_text: "manual".into(),
        excluded_models_text: "a".into(),
        ..ProviderSettingsModel::default()
    };
    // When
    let first = model.candidate_models();
    model.excluded_models_text = "b".into();
    let second = model.candidate_models();
    // Then
    assert_eq!(first, ["b"]);
    assert_eq!(second, ["a"]);
}

#[test]
fn candidate_models_excludes_manual_ids_when_fetch_is_unavailable() {
    // Given
    let model = ProviderSettingsModel {
        models_text: "a,b,a,c,b".into(),
        excluded_models_text: " b,b ".into(),
        ..ProviderSettingsModel::default()
    };
    // When
    let candidates = model.candidate_models();
    // Then
    assert_eq!(candidates, ["a", "c"]);
}

#[test]
fn candidate_models_appends_default_once_when_excluded() {
    // Given
    let model = ProviderSettingsModel {
        available_models: Some(vec!["a".into(), "b".into(), "a".into()]),
        default_model: "a".into(),
        excluded_models_text: "a".into(),
        ..ProviderSettingsModel::default()
    };
    // When
    let candidates = model.candidate_models();
    // Then
    assert_eq!(candidates, ["b", "a"]);
}

#[test]
fn candidate_models_keeps_source_order_without_duplicating_default() {
    // Given
    let model = ProviderSettingsModel {
        models_text: "b,a,b".into(),
        default_model: "b".into(),
        ..ProviderSettingsModel::default()
    };
    // When
    let candidates = model.candidate_models();
    // Then
    assert_eq!(candidates, ["b", "a"]);
}

#[test]
fn to_input_appends_trimmed_default_when_missing_from_manual_models() {
    // Given
    let model = ProviderSettingsModel {
        models_text: "manual".into(),
        default_model: " fetched ".into(),
        ..ProviderSettingsModel::default()
    };
    // When
    let input = model.to_input();
    // Then
    assert_eq!(input.models, ["manual", "fetched"]);
    assert_eq!(input.default_model, " fetched ");
}

#[test]
fn to_input_omits_blank_default_when_models_are_empty() {
    // Given
    let model = ProviderSettingsModel {
        default_model: " \t ".into(),
        ..ProviderSettingsModel::default()
    };
    // When
    let input = model.to_input();
    // Then
    assert!(input.models.is_empty());
    assert_eq!(input.default_model, " \t ");
}
