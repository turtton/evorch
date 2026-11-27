use std::borrow::Cow;

use super::*;

#[derive(Clone)]
pub(super) struct RunHealth {
    pub(super) shared: Arc<routing::ProviderHealth>,
    pub(super) image: Arc<routing::ProviderHealth>,
}

impl RunHealth {
    fn new(policy: &config::CooldownConfig) -> Self {
        Self {
            shared: Arc::new(routing::ProviderHealth::new(policy.clone())),
            image: Arc::new(routing::ProviderHealth::default()),
        }
    }
}

impl RoutedModel {
    // Like affinity, entries live until this model is dropped. Clone the Arcs before
    // routing: the map lock must never overlap affinity locks or provider calls.
    pub(super) fn run_health(&self, run_id: &str) -> RunHealth {
        self.health_by_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(run_id.to_owned())
            .or_insert_with(|| RunHealth::new(&self.cooldown_policy))
            .clone()
    }

    pub(super) fn existing_run_health(&self, run_id: &str) -> Option<RunHealth> {
        self.health_by_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(run_id)
            .cloned()
    }

    // Inspection, checkpoint validation and previews must not register pseudo runs.
    pub(super) fn existing_run_router(
        &self,
        run_id: &str,
        requires_tools: bool,
    ) -> Cow<'_, routing::Router> {
        let router = self.router_for(requires_tools);
        match self.existing_run_health(run_id) {
            Some(health) => Cow::Owned(router.clone().with_health(health.shared)),
            None => Cow::Borrowed(router),
        }
    }

    pub(super) fn resolve_unscoped_binding(
        &self,
        role: Role,
        category: Option<&str>,
    ) -> Result<(config::types::agents::ResolvedAgentBinding, ChainChoice), RuntimeError> {
        self.resolve_binding_in(
            &mut SessionAffinity::default(),
            "",
            role,
            category,
            &self.router,
        )
    }
}
