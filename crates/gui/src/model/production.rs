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
            catalog: model::ModelCatalog::builtin(),
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
