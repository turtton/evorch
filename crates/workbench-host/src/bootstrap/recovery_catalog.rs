//! Synchronous publication on canonical project model pins, with owned refresh.
use super::super::project_models::ModelCatalogResolver;
use super::super::{BootstrapError, BootstrapOptions, services::BackgroundServices};
use runtime::{AgentModel, AgentRuntime, CurrentRecoveryAuthority, RecoveryModels};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Published {
    resolver: Option<ModelCatalogResolver>,
    catalogs: BTreeMap<PathBuf, Arc<catalog::ModelCatalog>>,
    models: Vec<(PathBuf, Arc<dyn AgentModel>)>,
}
/// Process-local current catalogs, never serialized or restored from a recipe.
#[derive(Clone, Default)]
pub struct RecoveryCatalog(Arc<Mutex<Published>>);
impl RecoveryCatalog {
    /// Preload the startup project's catalog. Legacy embedded bootstrap does not
    /// call this entry point. Hosts with registered projects use preload_for_projects.
    pub async fn preload(
        options: &BootstrapOptions,
        config: &config::Config,
        services: &mut BackgroundServices,
    ) -> Result<Self, BootstrapError> {
        Self::preload_roots(options, config, [options.project_root.clone()], services).await
    }
    /// Await bounded loading of each current project's effective catalog BEFORE
    /// graph scan. A missing catalog blocks only projects using that directory.
    pub fn preload_for_projects<'a>(
        options: &'a BootstrapOptions,
        config: &'a config::Config,
        projects: &dyn crate::projects::ProjectDirectory,
        services: &'a mut BackgroundServices,
    ) -> impl std::future::Future<Output = Result<Self, BootstrapError>> + Send + 'a {
        // ProjectDirectory is Send, not Sync: snapshot the current roots before
        // returning a host-owned Send future, never borrow the directory across I/O.
        let roots = projects
            .entries()
            .into_iter()
            .map(|(_, root)| root)
            .chain(std::iter::once(options.project_root.clone()))
            .collect::<Vec<_>>();
        Self::preload_roots(options, config, roots, services)
    }
    async fn preload_roots(
        options: &BootstrapOptions,
        config: &config::Config,
        roots: impl IntoIterator<Item = PathBuf>,
        services: &mut BackgroundServices,
    ) -> Result<Self, BootstrapError> {
        let resolver = ModelCatalogResolver::new(options);
        let directories = roots
            .into_iter()
            .map(|root| resolver.for_project(Some(&root)))
            .collect::<BTreeSet<_>>();
        let current = Self(Arc::new(Mutex::new(Published {
            resolver: Some(resolver),
            ..Default::default()
        })));
        for directory in directories {
            if config.providers.is_empty() && !directory.join("models-dev.json").exists() {
                continue; // Explicit unconfigured health mode; no discovery/network.
            }
            let mut catalog = match catalog::ModelCatalog::load_or_refresh(&directory).await {
                Ok(catalog) => catalog,
                Err(error) => {
                    tracing::warn!(%error, directory = %directory.display(), "recovery catalog unavailable; corresponding graph components held");
                    continue;
                }
            };
            let refresh = catalog.take_refresh();
            current.publish(&directory, catalog);
            if let Some(refresh) = refresh {
                let target = current.clone();
                services.start("recovery-catalog", |stop| async move {
                    // Shutdown joins blocking cache I/O; never detach/abort it.
                    if let Ok(Ok(catalog)) = refresh.await {
                        target.publish(&directory, catalog);
                    }
                    let _ = stop.await;
                    Ok(())
                });
            }
        }
        Ok(current)
    }
    /// Publish only the matching directory on each canonical current project.
    pub fn publish_projects(
        &self,
        runtime: &AgentRuntime,
        projects: &dyn crate::projects::ProjectDirectory,
    ) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, root) in projects.entries() {
            let model = runtime.recovery_model_for_catalog(&runtime::RunConfig {
                project_root: Some(root.clone()),
                ..Default::default()
            });
            state.register(Some(&root), model);
        }
    }
    pub fn is_available(&self) -> bool {
        !self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalogs
            .is_empty()
    }
    pub fn is_available_for_project(&self, root: Option<&Path>) -> bool {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .resolver
            .as_ref()
            .is_some_and(|resolver| state.catalogs.contains_key(&resolver.for_project(root)))
    }
    fn publish(&self, directory: &Path, catalog: catalog::ModelCatalog) {
        let catalog = Arc::new(catalog);
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (known, model) in &state.models {
            if known == directory {
                model.publish_model_catalog(catalog.clone());
            }
        }
        state.catalogs.insert(directory.to_path_buf(), catalog);
    }
    /// Retain these exact current pins through adoption and activation. No model
    /// admission/complete is called; a reload requires fresh canonical pins.
    pub fn pin(
        &self,
        runtime: &AgentRuntime,
        authority: &CurrentRecoveryAuthority,
    ) -> RecoveryModels {
        let models = runtime.pin_recovery_models(authority);
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (run, config) in &authority.members {
            if let Some(model) = models.model(*run) {
                state.register(config.project_root.as_deref(), model);
            }
        }
        models
    }
}
impl Published {
    fn register(&mut self, root: Option<&Path>, model: Arc<dyn AgentModel>) {
        let Some(resolver) = &self.resolver else {
            return;
        };
        let directory = resolver.for_project(root);
        if self
            .models
            .iter()
            .any(|(known, pin)| known == &directory && Arc::ptr_eq(pin, &model))
        {
            return;
        }
        if let Some(catalog) = self.catalogs.get(&directory) {
            model.publish_model_catalog(catalog.clone());
        }
        self.models.push((directory, model));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{RunConfig, RunId};
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Model(Mutex<Vec<Arc<catalog::ModelCatalog>>>);
    #[async_trait::async_trait]
    impl AgentModel for Model {
        fn narrows_edit_tools(&self) -> bool {
            // This tool-free fixture already represents a concrete execution
            // context; no additional edit-tool wrapper is required.
            true
        }
        fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
            "test/model".into()
        }
        async fn complete(
            &self,
            _: &runtime::AgentInvocationContext,
            _: runtime::Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
            panic!("catalog publication must not execute a model")
        }
        fn publish_model_catalog(&self, catalog: Arc<catalog::ModelCatalog>) {
            self.0.lock().unwrap().push(catalog);
        }
    }
    #[tokio::test]
    async fn nonstartup_project_keeps_its_config_catalog_and_canonical_pins_without_admission() {
        let dir = test_socket_dir::SplitDir::new();
        let directory = dir.path().join("catalog");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("models-dev.json"), serde_json::json!({"fetched_at":0,
            "api":{"test":{"models":{"model":{"id":"model","limit":{"context":8192,"output":1024}}}}}}
        ).to_string()).unwrap();
        // Holding the real refresh lock serves cached data regardless of cache
        // age, without wall-clock assertions or any HTTP/background refresh.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join("models-dev.lock"))
            .unwrap();
        lock.lock().unwrap();
        let options = BootstrapOptions {
            config_dir: dir.path().to_path_buf(),
            ownership_root: dir.sockets().to_path_buf(),
            project_root: dir.path().to_path_buf(),
        };
        let mut services = BackgroundServices::default();
        let other = dir.path().join("other");
        let resolver = ModelCatalogResolver::new(&options);
        assert_eq!(resolver.for_project(Some(&other)), directory);
        assert_eq!(
            resolver.for_project(Some(dir.path())),
            catalog::default_cache_dir()
        );
        let catalog = RecoveryCatalog::preload_roots(
            &options,
            &Default::default(),
            [other.clone()],
            &mut services,
        )
        .await
        .unwrap();
        assert!(catalog.is_available_for_project(Some(&other)));
        assert!(
            !catalog.is_available_for_project(Some(dir.path())),
            "another project's catalog cannot authorize the default project"
        );
        assert!(services.names().next().is_none());
        let default = Arc::new(Model::default());
        let project = Arc::new(Model::default());
        let switchable = Arc::new(runtime::compose::SwitchableModel::new(project.clone()));
        let selected = switchable.clone();
        let bus = Arc::new(event_bus::EventBus::new(32));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            default.clone(),
        )
        .with_project_models(Arc::new(move |_| Some(selected.clone())));
        let authority = CurrentRecoveryAuthority {
            members: BTreeMap::from([
                (
                    RunId::new(1),
                    RunConfig {
                        project_root: Some(dir.path().join("other")),
                        ..Default::default()
                    },
                ),
                (
                    RunId::new(2),
                    RunConfig {
                        project_root: Some(dir.path().join("other")),
                        ..Default::default()
                    },
                ),
            ]),
        };
        let pins = catalog.pin(&runtime, &authority);
        let canonical: Arc<dyn AgentModel> = project.clone();
        assert!(Arc::ptr_eq(&pins.model(RunId::new(1)).unwrap(), &canonical));
        assert!(Arc::ptr_eq(&pins.model(RunId::new(2)).unwrap(), &canonical));
        assert!(default.0.lock().unwrap().is_empty());
        {
            let published = project.0.lock().unwrap();
            assert_eq!(published.len(), 1);
            assert_eq!(
                published[0].find("test", "model").unwrap().context_window,
                Some(8192)
            );
        }
        let _renewed = catalog.pin(&runtime, &authority);
        assert_eq!(project.0.lock().unwrap().len(), 1);
        let replacement = Arc::new(Model::default());
        switchable.replace(replacement.clone());
        let repinned = catalog.pin(&runtime, &authority);
        let current: Arc<dyn AgentModel> = replacement.clone();
        assert!(Arc::ptr_eq(
            &repinned.model(RunId::new(1)).unwrap(),
            &current
        ));
        assert_eq!(replacement.0.lock().unwrap().len(), 1);
        assert_eq!(project.0.lock().unwrap().len(), 1);
        services.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn health_mode_does_not_discover_or_contact_a_catalog() {
        let dir = test_socket_dir::SplitDir::new();
        let mut services = BackgroundServices::default();
        let options = BootstrapOptions {
            config_dir: dir.path().to_path_buf(),
            ownership_root: dir.sockets().to_path_buf(),
            project_root: dir.path().to_path_buf(),
        };
        let catalog = RecoveryCatalog::preload_roots(
            &options,
            &Default::default(),
            [dir.path().join("other")],
            &mut services,
        )
        .await
        .unwrap();
        assert!(!catalog.is_available());
        assert!(!dir.path().join("catalog").exists());
        assert!(services.names().next().is_none());
    }
}
