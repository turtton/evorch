//! Non-startup projects select their own role profile, but share credentials,
//! the bus and the user configuration. Runtime caches each project's context.
use super::BootstrapOptions;
use std::sync::Arc;

pub(super) fn resolver(
    options: &BootstrapOptions,
    credentials: Arc<dyn sandbox::CredentialStore>,
    bus: Arc<event_bus::EventBus>,
) -> runtime::ProjectModelResolver {
    let options = options.clone();
    Arc::new(move |root| {
        if root == options.project_root {
            return None;
        }
        let compose = || {
            let config = config::Config::load_strict(&config::LoadOptions {
                project_dir: Some(root.to_path_buf()),
                user_config_dir: Some(options.config_dir.clone()),
                read_env: false,
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;
            runtime::compose::compose_routed_model(
                &config,
                routing::ComposeDeps {
                    credential_store: credentials.clone(),
                    event_bus: Some(bus.clone()),
                    env: Arc::new(routing::ProcessEnv),
                    catalog: model::ModelCatalog::new(),
                    factory: routing::factory::FactoryOptions {
                        claude_installation_id_path: Some(
                            options
                                .config_dir
                                .join(providers::provider::claude::CLAUDE_INSTALLATION_ID_FILE),
                        ),
                        ..Default::default()
                    },
                },
                None,
            )
            .map(|m| m as Arc<dyn runtime::AgentModel>)
            .map_err(|e| e.to_string())
        };
        Some(compose().unwrap_or_else(|error| {
            tracing::warn!(%error, "project model unavailable");
            Arc::new(runtime::compose::UnconfiguredModel)
        }))
    })
}
