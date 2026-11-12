use super::*;

fn context(directory: &std::path::Path) -> ProductionModel {
    let user_dir = directory.join("user-config");
    std::fs::create_dir_all(&user_dir).unwrap();
    std::fs::write(
        user_dir.join("config.toml"),
        r#"
[providers.claude]
provider_type = "anthropic-subscription"
api_protocol = "anthropic-messages"
base_url = "https://example.test/v1"
default_model = "claude-test"
models = ["claude-test"]

[providers.claude.credential]
type = "keyring"
service = "evorch"
account = "claude"
"#,
    )
    .unwrap();
    ProductionModel {
        model_catalog_dir: Some(directory.join("catalog")),
        load_options: config::LoadOptions {
            user_config_dir: Some(user_dir),
            read_env: false,
            ..Default::default()
        },
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(directory.join("credentials")).unwrap(),
        ),
        bus: Arc::new(event_bus::EventBus::new(16)),
        env: Arc::new(routing::MapEnv::default()),
    }
}

async fn published_catalog(directory: &std::path::Path) -> Arc<catalog::ModelCatalog> {
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let cache = directory.join("published");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join("models-dev.json"),
        serde_json::json!({"fetched_at": fetched_at, "api": {"anthropic": {"models": {
            "claude-test": {"id": "claude-test", "limit": {"output": 64000}}
        }}}})
        .to_string(),
    )
    .unwrap();
    Arc::new(
        catalog::ModelCatalog::load_or_refresh(&cache)
            .await
            .unwrap(),
    )
}

// Given: a startup model and one project model exist
// When: a refreshed catalog is published, then another project's model is created
// Then: all three serve the published catalog to runs started afterwards, without recomposing
// the models that already existed.
#[tokio::test]
async fn published_catalog_reaches_startup_existing_and_later_project_models() {
    let directory = tempfile::tempdir().unwrap();
    let base = context(directory.path());
    let startup = Arc::new(runtime::compose::SwitchableModel::new(
        compose_production_model(&config::Config::load(&base.load_options).unwrap(), &base)
            .unwrap(),
    ));
    let main = (base.clone(), startup.clone());
    let projects = ProjectModels::new(base);
    let (first_root, later_root) = (
        directory.path().join("first"),
        directory.path().join("later"),
    );
    let first = projects.model_for(&first_root).unwrap();
    let first_state = first.output_limit_catalogs().unwrap();

    let catalog = published_catalog(directory.path()).await;
    publish_model_catalog(Some(&main), Some(&projects), &catalog);
    let later = projects.model_for(&later_root).unwrap();

    for (name, model) in [("startup", &startup), ("first", &first), ("later", &later)] {
        let current = model.output_limit_catalogs().unwrap().current_catalog();
        assert!(
            current.is_some_and(|current| Arc::ptr_eq(&current, &catalog)),
            "{name}"
        );
    }
    // The existing project model kept its shared state instead of being recomposed.
    assert!(Arc::ptr_eq(
        &first_state.current_catalog().unwrap(),
        &catalog
    ));
    assert!(Arc::ptr_eq(
        &first,
        &projects.model_for(&first_root).unwrap()
    ));
}
