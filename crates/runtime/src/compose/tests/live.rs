use super::*;

#[tokio::test]
async fn switchable_model_delegates_to_replaced_model() {
    // Given
    let (first, first_requests) = routed_model(Ok(response()), "first", None);
    let (second, second_requests) = routed_model(Ok(response()), "second", None);
    let model = SwitchableModel::new(Arc::new(first));
    // When
    model.replace(Arc::new(second));
    let result = model
        .complete(
            &AgentInvocationContext {
                run_id: "switch".into(),
            },
            Role::Worker,
            &[],
            &[],
        )
        .await;
    // Then
    assert_eq!(result, Ok(response()));
    assert!(first_requests.lock().unwrap().is_empty());
    assert_eq!(second_requests.lock().unwrap()[0].model, "second");
    assert_eq!(model.selected_model(Role::Worker), "local/second");
}

#[tokio::test]
async fn unconfigured_model_reports_settings_guidance() {
    // Given
    let model = UnconfiguredModel;
    // When
    let result = model
        .complete(
            &AgentInvocationContext {
                run_id: "empty".into(),
            },
            Role::Worker,
            &[],
            &[],
        )
        .await;
    // Then
    assert_eq!(
        result,
        Err(RuntimeError::Model {
            reason: "no provider configured — open Settings".into()
        })
    );
    assert_eq!(model.selected_model(Role::Worker), "unresolved:worker");
}

#[test]
fn compose_routed_model_matches_compose_runtime_output() {
    // Given
    let config = config::Config {
        providers: BTreeMap::from([(
            "local".into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAiCompatible,
                api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
                base_url: "https://example.test/v1".into(),
                credential: config::CredentialRefConfig::Env {
                    var: "TEST_KEY".into(),
                },
                models: vec!["live".into()],
                excluded_models: vec![],
                default_model: "live".into(),
            },
        )]),
        ..config::Config::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn CredentialStore> =
        Arc::new(sandbox::credential::FileCredentialStore::open(dir.path()).unwrap());
    let bus = Arc::new(EventBus::new(32));
    let env: Arc<dyn routing::EnvLookup> =
        Arc::new(routing::MapEnv::from_iter([("TEST_KEY", "test-secret")]));
    let deps = ComposeDeps {
        credential_store: store.clone(),
        event_bus: Some(bus.clone()),
        env: env.clone(),
        catalog: ModelCatalog::builtin(),
        factory: FactoryOptions::default(),
    };
    // When
    let model = compose_routed_model(&config, deps).unwrap();
    let runtime = compose_runtime(RuntimeComposition {
        config: &config,
        executor: Arc::new(ToolExecutor::new(bus.clone())),
        bus,
        credential_store: store,
        env,
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .unwrap();
    // Then: the same role identities used by runtime composition are preserved.
    assert_eq!(model.selected_model(Role::Worker), "local/live");
    assert_eq!(model.selected_model(Role::Orchestrator), "local/live");
    assert_eq!(
        model.providers.keys().cloned().collect::<Vec<_>>(),
        ["local"]
    );
    assert_eq!(
        runtime.model_identity,
        ModelIdentity::Routed {
            profiles: vec!["local".into()],
            selected: routed_roles()
                .into_iter()
                .map(|role| (role_key(role).into(), model.selected_model(role)))
                .collect(),
        }
    );
}
