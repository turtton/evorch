//! Model discovery for a saved provider: what the core asks the provider's
//! endpoint and how the answer is bounded.
//!
//! Everything a fetch needs comes from the saved file and the credential the
//! running core already holds (`routing::compose_provider`: the same token
//! stores and refresh locks as the models the runs use), never from the client.
//! The network wait runs outside every lock of the settings area, under a
//! deadline, so a provider that never answers ends as a failed operation and
//! delays no save.
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use workbench_protocol::{
    ids::OperationId,
    limits::MAX_NAME_BYTES,
    namespaces::settings::{MAX_MODELS, ProviderModelCandidates},
};

/// The longest a fetch may take before it ends as failed.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(25);

/// Why a fetch found nothing. Messages are fixed, bounded sentences: a
/// provider's answer, a URL or a credential is never quoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchFailure {
    /// This kind of provider has no discovery.
    Unsupported,
    Failed(String),
}

pub type FetchFuture = Pin<Box<dyn Future<Output = Result<Vec<String>, FetchFailure>> + Send>>;

/// Ask the provider `name` (as the saved file holds it) for its model ids.
pub type ModelFetcher =
    Arc<dyn Fn(String, config::ProviderProfileConfig) -> FetchFuture + Send + Sync>;

/// The discovery a settings area runs and the deadline it runs under.
#[derive(Clone)]
pub struct ModelFetch {
    pub fetcher: ModelFetcher,
    pub timeout: Duration,
}

impl ModelFetch {
    /// Discovery through the credential store the runs read.
    pub fn production(credentials: Arc<dyn sandbox::CredentialStore>) -> Self {
        Self {
            fetcher: Arc::new(move |name, profile| {
                let credentials = credentials.clone();
                Box::pin(async move { fetch(&name, &profile, &credentials).await })
            }),
            timeout: FETCH_TIMEOUT,
        }
    }

    /// A service that discovers nothing: every fetch is unsupported.
    pub fn unsupported() -> Self {
        Self {
            fetcher: Arc::new(|_, _| Box::pin(async { Err(FetchFailure::Unsupported) })),
            timeout: FETCH_TIMEOUT,
        }
    }
}

fn failed(sentence: &str) -> FetchFailure {
    FetchFailure::Failed(sentence.into())
}

async fn fetch(
    name: &str,
    profile: &config::ProviderProfileConfig,
    credentials: &Arc<dyn sandbox::CredentialStore>,
) -> Result<Vec<String>, FetchFailure> {
    let composed = routing::compose_provider(
        name,
        profile,
        credentials,
        None,
        &routing::ProcessEnv,
        &routing::factory::FactoryOptions::default(),
    )
    .map_err(|error| match error {
        routing::RoutingError::UnsupportedProviderType { .. } => FetchFailure::Unsupported,
        routing::RoutingError::MissingCredential { .. }
        | routing::RoutingError::EmptyCredential { .. }
        | routing::RoutingError::MissingKeyringCredential { .. } => {
            failed("no credential is available for this provider; sign in or set its key first")
        }
        _ => failed("the provider could not be set up"),
    })?;
    let listed = match composed.client.list_models(&composed.auth).await {
        Ok(Some(models)) => Ok(models),
        Ok(None) => providers::list_models(&composed.profile.base_url, &composed.auth).await,
        Err(error) => Err(error),
    };
    listed.map_err(|error| match error {
        providers::ProviderError::Http { status, .. } => {
            FetchFailure::Failed(format!("the provider answered HTTP {status}"))
        }
        providers::ProviderError::RateLimited { .. } => failed("the provider is rate limiting"),
        providers::ProviderError::Timeout => failed("the provider did not answer in time"),
        _ => failed("the provider could not be reached or its answer could not be read"),
    })
}

/// The ids a client may be offered: well-formed, sorted, without duplicates and
/// at most [`MAX_MODELS`] of them (`true`: more were dropped).
pub(super) fn bound(models: Vec<String>) -> (Vec<String>, bool) {
    let mut models: Vec<String> = models
        .into_iter()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty() && id.len() <= MAX_NAME_BYTES && !id.contains(char::is_control))
        .collect();
    models.sort();
    models.dedup();
    let truncated = models.len() > MAX_MODELS;
    models.truncate(MAX_MODELS);
    (models, truncated)
}

pub(super) fn candidates(
    operation_id: OperationId,
    provider: String,
    models: Vec<String>,
) -> ProviderModelCandidates {
    let (models, truncated) = bound(models);
    ProviderModelCandidates {
        operation_id,
        provider,
        models,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_are_sorted_deduplicated_and_bounded_with_the_cut_said() {
        let many: Vec<String> = (0..MAX_MODELS + 5).map(|i| format!("m{i:04}")).collect();
        let (kept, truncated) = bound(many);
        assert_eq!(kept.len(), MAX_MODELS);
        assert!(truncated);
        let (kept, truncated) = bound(vec![
            "b".into(),
            " a ".into(),
            "b".into(),
            String::new(),
            "bad\nid".into(),
            "x".repeat(MAX_NAME_BYTES + 1),
        ]);
        assert_eq!(kept, ["a", "b"]);
        assert!(!truncated);
    }
}
