//! Optional provider-owned web search, separate from the conversation request.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{ObservationContext, ProviderAuth, ProviderError, Usage};

/// Search-only request using the selected provider's existing session.
#[derive(Clone)]
pub struct HostedWebSearchRequest {
    pub model: String,
    pub query: String,
    pub max_results: Option<u32>,
    pub reasoning_effort: Option<String>,
    pub observation: Option<ObservationContext>,
    /// Called once upon receipt of valid completed usage, before content validation.
    /// The returned response repeats usage for metadata, not additional accounting.
    pub usage_sink: Option<Arc<dyn Fn(Usage) + Send + Sync>>,
}

/// A source consulted or cited by hosted search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedWebSearchCitation {
    pub url: String,
    pub title: String,
}

/// Completed hosted search and its already-accounted usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedWebSearchResponse {
    pub response_id: Option<String>,
    pub text: String,
    pub citations: Vec<HostedWebSearchCitation>,
    pub usage: Usage,
}

/// Only explicit tool unavailability permits search-provider fallback.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum HostedWebSearchError {
    #[error("hosted web_search is unsupported")]
    Unsupported,
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// Optional capability discovered through [`crate::ProviderClient`].
#[async_trait]
pub trait HostedWebSearch: Send + Sync {
    async fn search(
        &self,
        auth: &ProviderAuth,
        request: &HostedWebSearchRequest,
    ) -> Result<HostedWebSearchResponse, HostedWebSearchError>;
}
