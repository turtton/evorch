//! Output-limit catalog generations pinned per run.
//!
//! A run's `max_tokens` is part of its request settings, so it must not change after the
//! run started (prompt cache contract). Refreshing models.dev therefore publishes a new
//! *generation* for runs admitted afterwards, while every admitted run keeps resolving
//! limits from the generation it was pinned to.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::sync::OnceCell;

use super::RoutedModel;

/// One loaded (or lazily loadable) view of the models.dev data.
struct Generation {
    /// `None` inside means the catalog was unavailable when this generation loaded.
    catalog: OnceCell<Option<Arc<catalog::ModelCatalog>>>,
}

impl Generation {
    fn lazy() -> Arc<Self> {
        Arc::new(Self {
            catalog: OnceCell::new(),
        })
    }

    fn loaded(catalog: Arc<catalog::ModelCatalog>) -> Arc<Self> {
        Arc::new(Self {
            catalog: OnceCell::new_with(Some(Some(catalog))),
        })
    }

    /// Concurrent callers share one load; no lock is held while it runs.
    async fn catalog(
        &self,
        load: impl std::future::Future<Output = Option<Arc<catalog::ModelCatalog>>>,
    ) -> Option<Arc<catalog::ModelCatalog>> {
        self.catalog.get_or_init(|| load).await.clone()
    }

    fn unavailable(&self) -> bool {
        matches!(self.catalog.get(), Some(None))
    }
}

struct State {
    /// Generation for runs that have not been pinned yet.
    current: Arc<Generation>,
    /// Generation each admitted run resolves against, for as long as the run is known.
    pinned: BTreeMap<String, Arc<Generation>>,
}

/// Shared output-limit catalog state: the generation new runs start with, and the
/// generation every known run is pinned to. Clones share the same state, so a
/// reconfigured [`RoutedModel`] keeps serving runs of the model it replaces.
#[derive(Clone)]
pub struct OutputLimitCatalogs(Arc<Mutex<State>>);

impl Default for OutputLimitCatalogs {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputLimitCatalogs {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(State {
            current: Generation::lazy(),
            pinned: BTreeMap::new(),
        })))
    }

    /// Makes `catalog` the generation of runs pinned from now on. Runs already pinned,
    /// including those whose first request has not been sent, keep their generation.
    pub fn publish(&self, catalog: Arc<catalog::ModelCatalog>) {
        self.lock().current = Generation::loaded(catalog);
    }

    /// The catalog new runs will resolve against, once it has been loaded or published.
    pub fn current_catalog(&self) -> Option<Arc<catalog::ModelCatalog>> {
        self.lock().current.catalog.get().cloned().flatten()
    }

    /// Reconfiguration carries this state over. A generation that failed to load is
    /// replaced for future runs so the new model retries, as a fresh model would.
    pub fn retry_unavailable(&self) {
        let mut state = self.lock();
        if state.current.unavailable() {
            state.current = Generation::lazy();
        }
    }

    /// Returns the run's generation, pinning it to the current one on first sight.
    fn pin(&self, run_id: &str) -> Arc<Generation> {
        let mut state = self.lock();
        if let Some(pinned) = state.pinned.get(run_id) {
            return Arc::clone(pinned);
        }
        let current = Arc::clone(&state.current);
        state.pinned.insert(run_id.to_owned(), Arc::clone(&current));
        current
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn is_pinned_to_current(&self, run_id: &str) -> Option<bool> {
        let state = self.lock();
        state
            .pinned
            .get(run_id)
            .map(|pinned| Arc::ptr_eq(pinned, &state.current))
    }
}

impl RoutedModel {
    /// The shared output-limit state; clone it to hand to a replacement model.
    pub(super) fn output_limit_state(&self) -> OutputLimitCatalogs {
        self.output_limits
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(super) fn adopt_output_limit_state(&self, shared: &OutputLimitCatalogs) {
        shared.retry_unavailable();
        *self
            .output_limits
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = shared.clone();
    }

    pub(super) async fn metadata_catalog(&self) -> Option<&Arc<catalog::ModelCatalog>> {
        self.metadata_catalog
            .get_or_init(|| async {
                if self.provider_configs.is_empty() {
                    None
                } else {
                    let directory = self
                        .metadata_catalog_dir
                        .clone()
                        .unwrap_or_else(catalog::default_cache_dir);
                    crate::model_resolve::load_catalog(&directory)
                        .await
                        .map(Arc::new)
                }
            })
            .await
            .as_ref()
    }

    /// Pins `run_id` to the current generation unless it is pinned already.
    pub(super) fn pin_output_limits(&self, run_id: &str) {
        self.output_limit_state().pin(run_id);
    }

    /// Pins `run_id` to a generation (if not yet pinned) and returns its catalog.
    pub(super) async fn pinned_catalog(&self, run_id: &str) -> Option<Arc<catalog::ModelCatalog>> {
        let generation = self.output_limit_state().pin(run_id);
        generation
            .catalog(async { self.metadata_catalog().await.cloned() })
            .await
    }

    pub(super) async fn output_limit(
        &self,
        run_id: &str,
        route: &routing::ResolvedRoute,
        explicit: Option<u32>,
    ) -> Option<u64> {
        // Pinned even when the caller overrides the limit, so a later request of the
        // same run cannot start from a newer generation than its first one.
        let generation = self.output_limit_state().pin(run_id);
        if let Some(value) = explicit {
            return Some(u64::from(value));
        }
        let protocol = self.providers.get(&route.profile)?.profile.api_protocol;
        // Codex backend omits max_output_tokens. The factory accepts `openai-responses` only
        // for `openai` and pins `openai-codex` to `openai-codex-responses`, so protocol suffices.
        if !matches!(
            protocol,
            model::ApiProtocol::AnthropicMessages
                | model::ApiProtocol::OpenAiCompletions
                | model::ApiProtocol::OpenAiResponses
        ) {
            return None;
        }
        let profile = self.provider_configs.get(&route.profile)?;
        let (base_model, _) = config::types::provider::parse_model_speed(&route.model_id);
        let entry = profile
            .models
            .iter()
            .filter(|entry| entry.enabled)
            .find(|entry| entry.id == route.model_id)
            .or_else(|| {
                profile
                    .models
                    .iter()
                    .find(|entry| entry.enabled && entry.id == base_model)
            })
            .cloned()
            .unwrap_or_else(|| config::ModelEntryConfig::enabled(base_model));
        let resolved = crate::model_resolve::resolve_model_metadata(
            &entry,
            &self.model_presets,
            None,
            Some(&route.profile),
            Some(profile.provider_type),
        );
        if resolved.max_output_tokens.is_some() {
            return resolved.max_output_tokens;
        }
        let catalog = generation
            .catalog(async { self.metadata_catalog().await.cloned() })
            .await;
        crate::model_resolve::resolve_model_metadata(
            &entry,
            &self.model_presets,
            catalog.as_deref(),
            Some(&route.profile),
            Some(profile.provider_type),
        )
        .max_output_tokens
    }
}
