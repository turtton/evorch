use std::sync::Arc;

use runtime::compose::{CompositionError, RoutedModel, compose_routed_model};

#[derive(Clone)]
pub struct ProductionModel {
    pub load_options: config::LoadOptions,
    pub credential_store: Arc<dyn sandbox::CredentialStore>,
    pub bus: Arc<event_bus::EventBus>,
    pub env: Arc<dyn routing::EnvLookup>,
}

pub fn compose_production_model(
    config: &config::Config,
    context: &ProductionModel,
) -> Result<Arc<RoutedModel>, CompositionError> {
    compose_routed_model(
        config,
        routing::ComposeDeps {
            credential_store: context.credential_store.clone(),
            event_bus: Some(context.bus.clone()),
            env: context.env.clone(),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions::default(),
        },
    )
}

impl ProductionModel {
    pub fn reload(&self) -> Result<Arc<RoutedModel>, String> {
        let config = config::Config::load(&self.load_options).map_err(|error| error.to_string())?;
        compose_production_model(&config, self).map_err(|error| error.to_string())
    }
}

/// Models for projects other than the startup one, composed on first use from
/// each project's selected role profile and reloaded after every settings save.
#[derive(Clone)]
pub struct ProjectModels {
    base: ProductionModel,
    models: Arc<
        std::sync::Mutex<
            std::collections::BTreeMap<std::path::PathBuf, Arc<runtime::compose::SwitchableModel>>,
        >,
    >,
}

impl ProjectModels {
    /// `base` supplies credentials and events; its project directory is replaced per project.
    pub fn new(base: ProductionModel) -> Self {
        Self {
            base,
            models: Arc::default(),
        }
    }

    fn context(&self, root: &std::path::Path) -> ProductionModel {
        ProductionModel {
            load_options: config::LoadOptions {
                project_dir: Some(root.to_path_buf()),
                ..self.base.load_options.clone()
            },
            ..self.base.clone()
        }
    }

    fn compose(&self, root: &std::path::Path) -> Arc<dyn runtime::AgentModel> {
        match self.context(root).reload() {
            Ok(model) => model,
            Err(error) => {
                tracing::error!(%error, root = %root.display(), "project model composition failed");
                Arc::new(runtime::compose::UnconfiguredModel)
            }
        }
    }

    /// The model runs of `root` use; the startup project keeps the runtime's own model.
    pub fn model_for(
        &self,
        root: &std::path::Path,
    ) -> Option<Arc<runtime::compose::SwitchableModel>> {
        if self.base.load_options.project_dir.as_deref() == Some(root) {
            return None;
        }
        let mut models = self
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(Arc::clone(models.entry(root.to_path_buf()).or_insert_with(
            || Arc::new(runtime::compose::SwitchableModel::new(self.compose(root))),
        )))
    }

    /// Recomposes one project's model after its configuration changed.
    pub fn reload(&self, root: &std::path::Path) -> Result<(), String> {
        let model = self
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .cloned();
        if let Some(model) = model {
            model.replace(self.context(root).reload()?);
        }
        Ok(())
    }

    /// Recomposes every project model after a user configuration change.
    pub fn reload_all(&self) -> Result<(), String> {
        let roots: Vec<_> = self
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        roots.iter().try_for_each(|root| self.reload(root))
    }

    pub fn resolver(&self) -> runtime::ProjectModelResolver {
        let models = self.clone();
        Arc::new(move |root| {
            models
                .model_for(root)
                .map(|model| model as Arc<dyn runtime::AgentModel>)
        })
    }
}

/// Recomposes the startup model and every project model after a settings save.
pub fn reload_models(
    main: Option<&(ProductionModel, Arc<runtime::compose::SwitchableModel>)>,
    projects: Option<&ProjectModels>,
) -> Result<(), String> {
    if let Some((context, model)) = main {
        model.replace(context.reload()?);
    }
    projects.map_or(Ok(()), ProjectModels::reload_all)
}
