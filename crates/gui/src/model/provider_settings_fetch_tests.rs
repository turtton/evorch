use super::*;

#[test]
fn poll_discards_success_when_base_url_changes() {
    // Given
    let mut model = ProviderSettingsModel {
        base_url: "https://server-a.invalid/v1".into(),
        ..ProviderSettingsModel::default()
    };
    model.start_models_fetch_with_key(None);
    assert_eq!(
        model.models_fetch_base_url.as_deref(),
        Some("https://server-a.invalid/v1")
    );
    let (tx, rx) = channel();
    model.models_rx = Some(rx);
    model.base_url = "https://server-b.invalid/v1".into();
    tx.send(Ok(vec!["server-a-model".into()])).unwrap();
    // When
    assert!(model.poll_models());
    // Then
    assert_eq!(
        model.models_fetch_state,
        ModelsFetchState::Failed("Base URL changed during fetch; result discarded".into())
    );
    assert_eq!(model.available_models, None);
    assert_eq!(model.models_fetch_base_url, None);
}

#[test]
fn poll_discards_error_when_base_url_changes() {
    // Given
    let mut model = ProviderSettingsModel {
        base_url: "https://server-a.invalid/v1".into(),
        ..ProviderSettingsModel::default()
    };
    model.start_models_fetch_with_key(None);
    let (tx, rx) = channel();
    model.models_rx = Some(rx);
    model.base_url = "https://server-b.invalid/v1".into();
    tx.send(Err("server-a error".into())).unwrap();
    // When
    assert!(model.poll_models());
    // Then
    assert_eq!(
        model.models_fetch_state,
        ModelsFetchState::Failed("Base URL changed during fetch; result discarded".into())
    );
    assert_eq!(model.available_models, None);
    assert_eq!(model.models_fetch_base_url, None);
}

#[test]
fn poll_loads_models_when_base_url_is_unchanged() {
    // Given
    let mut model = ProviderSettingsModel {
        base_url: "https://server-a.invalid/v1".into(),
        ..ProviderSettingsModel::default()
    };
    model.start_models_fetch_with_key(None);
    let (tx, rx) = channel();
    model.models_rx = Some(rx);
    tx.send(Ok(vec!["server-a-model".into()])).unwrap();
    // When
    assert!(model.poll_models());
    // Then
    assert_eq!(model.models_fetch_state, ModelsFetchState::Loaded);
    assert_eq!(model.available_models, Some(vec!["server-a-model".into()]));
    assert_eq!(model.models_fetch_base_url, None);
}
