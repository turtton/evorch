use super::*;
use gui::model::production::{ProductionModel, compose_production_model};
use runtime::compose::{SwitchableModel, UnconfiguredModel};
use sandbox::{CredentialStore, Secret};

fn options(root: &std::path::Path) -> config::LoadOptions {
    config::LoadOptions {
        project_dir: Some(root.to_owned()),
        user_config_dir: Some(root.join("user")),
        read_env: false,
        ..Default::default()
    }
}

fn production(root: &std::path::Path, store: Arc<dyn CredentialStore>) -> ProductionModel {
    ProductionModel {
        load_options: options(root),
        credential_store: store,
        bus: Arc::new(event_bus::EventBus::new(32)),
        env: Arc::new(routing::MapEnv::default()),
    }
}

#[tokio::test]
async fn production_non_demo_composition_uses_loaded_config_and_file_store() {
    // Given
    let server =
        mock_openai::StreamingMockOpenAi::spawn(vec![mock_openai::ScriptedResponse::text_stream(
            "response",
            "live-model",
            ["live answer"],
        )]);
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        sandbox::credential::FileCredentialStore::open(temp.path().join("credentials")).unwrap(),
    );
    store
        .set("live", &Secret::from("test-secret".to_owned()))
        .unwrap();
    std::fs::write(
        temp.path().join("evorch.toml"),
        format!(
            r#"
[providers.live]
type = "openai-compatible"
base_url = "{}"
models = ["live-model"]
default_model = "live-model"
[providers.live.credential]
kind = "keyring"
service = "evorch"
account = "live"
"#,
            server.base_url()
        ),
    )
    .unwrap();
    let context = production(temp.path(), store);
    let config = config::Config::load(&context.load_options).unwrap();
    // When
    let model = compose_production_model(&config, &context).unwrap();
    let response = model
        .complete(
            &AgentInvocationContext {
                run_id: "gui-chat".into(),
            },
            Role::Worker,
            &[Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            }],
            &[],
        )
        .await
        .unwrap();
    // Then
    assert_eq!(
        response.message.content,
        vec![ContentBlock::Text {
            text: "live answer".into()
        }]
    );
    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer test-secret")
    );
    assert_eq!(requests[0].body["model"], "live-model");
}

#[test]
fn provider_save_recomposes_live_model() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        sandbox::credential::FileCredentialStore::open(temp.path().join("credentials")).unwrap(),
    );
    let context = production(temp.path(), store.clone());
    let model = Arc::new(SwitchableModel::new(Arc::new(UnconfiguredModel)));
    let runtime = AgentRuntime::new(
        context.bus.clone(),
        Arc::new(ToolExecutor::new(context.bus.clone())),
        model.clone(),
    );
    let mut state = WorkbenchState::new(runtime, &UiSettings::default())
        .unwrap()
        .with_provider_settings_path(temp.path().join("evorch.toml"))
        .with_credential_store(store)
        .with_production_model(context, model.clone());
    let settings = state.provider_settings_mut();
    settings.open = true;
    settings.name = "live".into();
    settings.base_url = "https://example.test/v1".into();
    settings.credential_mode = gui::model::provider_settings::CredentialMode::Keyring;
    settings.api_key_input = "new-secret".into();
    settings.models_text = "new-model".into();
    settings.default_model = "new-model".into();
    assert_eq!(model.selected_model(Role::Worker), "unresolved:worker");
    // When
    state.submit_provider_settings();
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.provider_settings().open && state.provider_settings().error.is_none() {
        assert!(Instant::now() < deadline, "save worker timed out");
        state.poll_provider_save();
        std::thread::yield_now();
    }
    // Then
    assert_eq!(state.provider_settings().error, None);
    assert_eq!(model.selected_model(Role::Worker), "live/new-model");
    assert_eq!(
        state.provider_status(),
        &gui::model::composer::ProviderStatus::Configured
    );
}
