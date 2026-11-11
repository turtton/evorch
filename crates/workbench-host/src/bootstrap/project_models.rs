//! Non-startup projects select their own role profile, but share credentials,
//! the bus and the user configuration. Runtime caches each project's context.
use super::BootstrapOptions;
use std::sync::Arc;
type LiveProjects = Arc<
    std::sync::Mutex<
        std::collections::BTreeMap<std::path::PathBuf, Arc<runtime::compose::SwitchableModel>>,
    >,
>;

pub(super) fn resolver(
    options: &BootstrapOptions,
    credentials: Arc<dyn sandbox::CredentialStore>,
    bus: Arc<event_bus::EventBus>,
) -> (runtime::ProjectModelResolver, super::evaluation::Reload) {
    let options = options.clone();
    let models: LiveProjects = Arc::default();
    let reload_models = models.clone();
    let reload_options = options.clone();
    let reload_credentials = credentials.clone();
    let reload_bus = bus.clone();
    let resolver: runtime::ProjectModelResolver = Arc::new(move |root| {
        if root == options.project_root {
            return None;
        }
        let mut models = models.lock().unwrap();
        if let Some(model) = models.get(root) {
            return Some(model.clone());
        }
        let compose = || {
            let config = config::Config::load_strict(&config::LoadOptions {
                project_dir: Some(root.to_path_buf()),
                user_config_dir: Some(options.config_dir.clone()),
                read_env: false,
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;
            super::models::compose_production_model(
                &config,
                &super::models::ProductionModel {
                    credential_store: credentials.clone(),
                    bus: bus.clone(),
                    env: Arc::new(routing::ProcessEnv),
                    model_catalog_dir: Some(options.config_dir.join("catalog")),
                    load_options: config::LoadOptions {
                        project_dir: Some(root.to_path_buf()),
                        user_config_dir: Some(options.config_dir.clone()),
                        read_env: false,
                        ..Default::default()
                    },
                },
            )
            .map(|m| m as Arc<dyn runtime::AgentModel>)
            .map_err(|e| e.to_string())
        };
        let model = Arc::new(runtime::compose::SwitchableModel::new(
            compose().unwrap_or_else(|error| {
                tracing::warn!(%error, "project model unavailable");
                Arc::new(runtime::compose::UnconfiguredModel)
            }),
        ));
        models.insert(root.to_path_buf(), model.clone());
        Some(model)
    });
    let reload: super::evaluation::Reload = Box::new(move || {
        let models = reload_models.lock().unwrap().clone();
        for (root, model) in models {
            let load_options = config::LoadOptions {
                project_dir: Some(root),
                user_config_dir: Some(reload_options.config_dir.clone()),
                read_env: false,
                ..Default::default()
            };
            let config = config::Config::load_strict(&load_options).map_err(|e| e.to_string())?;
            let context = super::models::ProductionModel {
                load_options,
                credential_store: reload_credentials.clone(),
                bus: reload_bus.clone(),
                env: Arc::new(routing::ProcessEnv),
                model_catalog_dir: None,
            };
            model.replace(
                super::models::compose_production_model(&config, &context)
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(())
    });
    (resolver, reload)
}
