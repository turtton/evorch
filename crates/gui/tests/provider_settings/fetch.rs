use super::workbench_with_seeded_settings;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::provider_settings::{ModelsFetchState, ProviderSettingsModel};
use mock_openai::{StreamingMockOpenAi, WriteMode};

fn run_until_fetch_finishes(harness: &mut HeadlessWorkbench<DemoSource>) {
    // run() は Loading 中の repaint 要求を消化し続け max_steps を超過し得るため、確定的に 1 フレームずつ進める。
    for _ in 0..200 {
        harness.step();
        match &harness.state().provider_settings().models_fetch_state {
            ModelsFetchState::Loaded | ModelsFetchState::Failed(_) => return,
            ModelsFetchState::Idle | ModelsFetchState::Loading => std::thread::yield_now(),
        }
    }
    panic!(
        "model fetch did not finish within 200 frame runs: {:?}",
        harness.state().provider_settings().models_fetch_state
    );
}

#[test]
fn poll_reports_failure_when_injected_key_is_missing() {
    // Given: no injected credential and a model with a previous fetched list.
    let mut model = ProviderSettingsModel {
        available_models: Some(vec!["stale-model".into()]),
        ..ProviderSettingsModel::default()
    };
    // When: the credential-free fetch finishes and its result is polled.
    model.start_models_fetch_with_key(None);
    assert_eq!(model.models_fetch_state, ModelsFetchState::Loading);
    let rx = model.models_rx.take().expect("fetch receiver");
    let result = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("fetch result");
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(result).expect("result forwarded");
    model.models_rx = Some(rx);
    assert!(model.poll_models());
    // Then: failure clears stale candidates without requiring an environment mutation.
    assert_eq!(
        model.models_fetch_state,
        ModelsFetchState::Failed("API key env var is not set".into())
    );
    assert_eq!(model.available_models, None);
    assert!(model.models_rx.is_none());
}

#[test]
fn fetched_models_populate_modal_when_request_succeeds() {
    // Given: a local models endpoint and a manually seeded provider.
    let server = StreamingMockOpenAi::spawn_with_models(
        vec![],
        WriteMode::default(),
        vec!["mock-model-b".into(), "mock-model-a".into()],
    );
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_seeded_settings(
        temp.path(),
        ProviderSettingsModel {
            name: "local".into(),
            base_url: server.base_url(),
            api_key_env: "TEST_KEY".into(),
            models_text: "manual-model".into(),
            default_model: "manual-model".into(),
            ..ProviderSettingsModel::default()
        },
        [1200.0, 900.0],
    );
    harness.click_label("Open Settings");
    harness.run();
    // When: the modal fetches using an injected key without mutating the environment.
    harness
        .state_mut()
        .provider_settings_mut()
        .start_models_fetch_with_key(Some("sk-test".into()));
    run_until_fetch_finishes(&mut harness);
    // Then: the ordered models and loaded status reach the rendered modal.
    let model = harness.state().provider_settings();
    assert_eq!(model.models_fetch_state, ModelsFetchState::Loaded);
    assert_eq!(
        model.available_models,
        Some(vec!["mock-model-b".into(), "mock-model-a".into()])
    );
    assert!(harness.has_label("Loaded 2 models from /v1/models"));
    assert!(server.recorded_requests().iter().any(|request| {
        request.method == "GET"
            && request.path == "/v1/models"
            && request.authorization.as_deref() == Some("Bearer sk-test")
    }));
    harness.click_label("Default model");
    harness.run();
    assert!(harness.has_label("mock-model-b"));
    assert!(harness.has_label("mock-model-a"));
}

#[test]
fn manual_models_remain_available_when_request_fails() {
    // Given: an unreachable endpoint and a manually configured model.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_seeded_settings(
        temp.path(),
        ProviderSettingsModel {
            base_url: "http://127.0.0.1:9".into(),
            api_key_env: "TEST_KEY".into(),
            models_text: "manual-model".into(),
            default_model: "manual-model".into(),
            ..ProviderSettingsModel::default()
        },
        [1200.0, 900.0],
    );
    harness.click_label("Open Settings");
    harness.run();
    // When: an authenticated request fails and the UI polls the result.
    harness
        .state_mut()
        .provider_settings_mut()
        .start_models_fetch_with_key(Some("k".into()));
    run_until_fetch_finishes(&mut harness);
    // Then: the failure leaves manual entry and the selected model intact.
    let model = harness.state().provider_settings();
    let ModelsFetchState::Failed(error) = &model.models_fetch_state else {
        panic!("expected failed fetch, got {:?}", model.models_fetch_state);
    };
    assert_eq!(model.available_models, None);
    assert_eq!(model.parsed_models(), ["manual-model"]);
    assert!(harness.has_label(&format!("Auto-fetch failed ({error}); manual entry below")));
    harness.click_label("Default model");
    harness.run();
    assert!(harness.has_label("manual-model"));
}

#[test]
fn save_persists_fetched_selection_when_manual_models_differ() {
    // Given: a real models endpoint and a provider with only a manual model.
    let server = StreamingMockOpenAi::spawn_with_models(
        vec![],
        WriteMode::default(),
        vec!["mock-model-a".into()],
    );
    let temp = tempfile::tempdir().expect("temp dir");
    let mut harness = workbench_with_seeded_settings(
        temp.path(),
        ProviderSettingsModel {
            name: "local".into(),
            base_url: server.base_url(),
            api_key_env: "TEST_KEY".into(),
            models_text: "manual-model".into(),
            default_model: "manual-model".into(),
            ..ProviderSettingsModel::default()
        },
        [1200.0, 900.0],
    );
    harness.click_label("Open Settings");
    harness.run();
    harness
        .state_mut()
        .provider_settings_mut()
        .start_models_fetch_with_key(Some("sk-test".into()));
    run_until_fetch_finishes(&mut harness);
    assert_eq!(
        harness.state().provider_settings().models_fetch_state,
        ModelsFetchState::Loaded
    );
    harness.state_mut().provider_settings_mut().default_model = "mock-model-a".into();
    harness.run();
    // When: the fetched selection is saved through the modal.
    harness.click_label("Save");
    harness.run();
    // Then: the modal closes and the selected model is persisted as a member.
    assert!(!harness.state().provider_settings().open);
    assert!(!harness.has_label("Save"));
    let raw = std::fs::read_to_string(temp.path().join("evorch.toml")).expect("saved config");
    assert!(raw.contains("default_model = \"mock-model-a\""));
    let saved = super::load_config(temp.path());
    let provider = saved.providers.get("local").expect("saved provider");
    assert_eq!(provider.default_model, "mock-model-a");
    assert_eq!(provider.models, ["manual-model", "mock-model-a"]);
}
