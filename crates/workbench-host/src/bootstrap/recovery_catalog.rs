//! Synchronous publication on canonical project model pins, with owned refresh.
use super::super::{BootstrapError, BootstrapOptions, services::BackgroundServices};
use runtime::{AgentModel, AgentRuntime, CurrentRecoveryAuthority, RecoveryModels};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Published {
    catalog: Option<Arc<catalog::ModelCatalog>>,
    models: Vec<Arc<dyn AgentModel>>,
}
/// Process-local current catalog, never serialized or restored from a recipe.
#[derive(Clone, Default)]
pub struct RecoveryCatalog(Arc<Mutex<Published>>);
impl RecoveryCatalog {
    /// Await bounded catalog loading BEFORE scanning or constructing recovery
    /// authorities. Any stale refresh is owned by the host's service lifetime.
    /// Legacy embedded bootstrap does not call this entry point.
    pub async fn preload(
        options: &BootstrapOptions,
        config: &config::Config,
        services: &mut BackgroundServices,
    ) -> Result<Self, BootstrapError> {
        let directory = options.config_dir.join("catalog");
        if config.providers.is_empty() && !directory.join("models-dev.json").exists() {
            return Ok(Self::default()); // Explicit unconfigured health mode.
        }
        let mut catalog = catalog::ModelCatalog::load_or_refresh(&directory).await?;
        let refresh = catalog.take_refresh();
        let current = Self::default();
        current.publish(catalog);
        if let Some(refresh) = refresh {
            let target = current.clone();
            services.start("recovery-catalog", |stop| async move {
                // Do not detach/abort an async refresh containing blocking cache
                // I/O. Shutdown joins it, even when stop precedes its completion.
                if let Ok(Ok(catalog)) = refresh.await {
                    target.publish(catalog);
                }
                let _ = stop.await;
                Ok(())
            });
        }
        Ok(current)
    }
    pub fn is_available(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalog
            .is_some()
    }
    fn publish(&self, catalog: catalog::ModelCatalog) {
        let catalog = Arc::new(catalog);
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for model in &state.models {
            model.publish_model_catalog(catalog.clone());
        }
        state.catalog = Some(catalog);
    }
    /// Call before pure descriptor validation and retain these exact pins for
    /// adoption/activation. A reload requires a new call (not an old wrapper).
    /// This never calls admission or complete and never starts provider work.
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
        for run in authority.members.keys() {
            let Some(model) = models.model(*run) else {
                continue;
            };
            if state.models.iter().any(|known| Arc::ptr_eq(known, &model)) {
                continue;
            }
            if let Some(catalog) = &state.catalog {
                model.publish_model_catalog(catalog.clone());
            }
            state.models.push(model);
        }
        models
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
    async fn publication_uses_the_canonical_project_context_without_admission() {
        let dir = tempfile::tempdir().unwrap();
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
            ownership_root: dir.path().join("owners"),
            project_root: dir.path().to_path_buf(),
        };
        let mut services = BackgroundServices::default();
        let catalog = RecoveryCatalog::preload(&options, &Default::default(), &mut services)
            .await
            .unwrap();
        assert!(catalog.is_available());
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
        let dir = tempfile::tempdir().unwrap();
        let mut services = BackgroundServices::default();
        let options = BootstrapOptions {
            config_dir: dir.path().to_path_buf(),
            ownership_root: dir.path().join("owners"),
            project_root: dir.path().to_path_buf(),
        };
        let catalog = RecoveryCatalog::preload(&options, &Default::default(), &mut services)
            .await
            .unwrap();
        assert!(!catalog.is_available());
        assert!(!dir.path().join("catalog").exists());
        assert!(services.names().next().is_none());
    }
}
