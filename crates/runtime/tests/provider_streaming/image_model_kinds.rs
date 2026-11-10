//! A conversation chain that a normal load lets through with an image model fails
//! before any request is sent (ADR 0038).

use std::sync::Arc;

use config::{Config, LoadOptions};
use mock_openai::StreamingMockOpenAi;
use routing::{ComposeDeps, MapEnv, factory::FactoryOptions};
use runtime::compose::compose_routed_model;
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use sandbox::credential::FileCredentialStore;

#[tokio::test]
async fn image_model_in_the_worker_chain_warns_on_load_and_fails_before_sending() {
    // Given: an OpenAI-style profile listing gpt-image-1, put in the worker's chain.
    let directory = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn(vec![]);
    std::fs::write(
        directory.path().join("config.toml"),
        format!(
            r#"version = 3
[providers.openai]
type = "openai-compatible"
base_url = "{}"
api_key_env = "IMAGE_KIND_KEY"
models = ["gpt-4o", "gpt-image-1"]
default_model = "gpt-4o"
[routing.routes]
art = [{{ profile = "openai", model = "gpt-image-1" }}]
chat = [{{ profile = "openai" }}]
[agents.worker]
models = [{{ route = "chat" }}, {{ route = "art" }}]
"#,
            mock.base_url()
        ),
    )
    .unwrap();
    let options = LoadOptions {
        user_config_dir: Some(directory.path().into()),
        read_env: false,
        ..Default::default()
    };
    // When: loading strictly, then normally.
    let strict = Config::load_strict(&options);
    let config = Config::load(&options).expect("a normal load only warns");
    // Then: strict loading names the chain, the route and the model.
    let Err(config::ConfigError::InvalidField { path, message }) = strict else {
        panic!("strict load must fail: {strict:?}");
    };
    assert_eq!(path, "agents.worker.models");
    assert!(
        message.contains("`art`") && message.contains("openai/gpt-image-1"),
        "{message}"
    );

    // When: the runtime composed from the normal load sends for the worker.
    let model = compose_routed_model(
        &config,
        ComposeDeps {
            credential_store: Arc::new(FileCredentialStore::open(directory.path()).unwrap()),
            event_bus: None,
            env: Arc::new(MapEnv::from_iter([("IMAGE_KIND_KEY", "test-secret")])),
            catalog: model::ModelCatalog::new(),
            factory: FactoryOptions::default(),
        },
        None,
    )
    .expect("composition succeeds");
    let error = model
        .complete(
            &AgentInvocationContext {
                attempts: None,
                run_id: "image-kind".into(),
                ..Default::default()
            },
            Role::Worker,
            &[],
            &[],
        )
        .await
        .expect_err("the chain holds an image model");
    // Then: it fails with the typed error and no HTTP request reached the provider.
    assert_eq!(
        error,
        RuntimeError::ModelKindMismatch {
            origin: "chain `worker` route `art`".into(),
            profile: "openai".into(),
            model: "gpt-image-1".into(),
            kind: config::ModelKind::ImageGeneration,
        }
    );
    assert!(mock.recorded_requests().is_empty());
    // The model picker offers only the chat model of the profile.
    assert_eq!(model.available_profiles()[0].models, ["gpt-4o"]);
}
