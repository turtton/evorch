//! A search-only adapter over the exact provider selected for this invocation.

use std::sync::Arc;

use providers::{HostedWebSearchError, HostedWebSearchRequest, ProviderError, Usage};
use tools::search::{SearchError, SearchOptions, SearchProvider, SearchResults};

use super::{AgentInvocationContext, Role, RoutedModel, RuntimeError, ToolSpec};

pub(super) fn resolve(
    model: &RoutedModel,
    invocation: &AgentInvocationContext,
    role: Role,
    tools: &[ToolSpec],
    usage_sink: Arc<dyn Fn(Usage) + Send + Sync>,
) -> Result<Option<Arc<dyn SearchProvider>>, RuntimeError> {
    // Reuse the invocation snapshot and the run's pinned fallback affinity.
    // No model preference receiver or process-wide credential lookup is involved.
    let resolved = model.resolve_invocation(invocation, role, tools)?;
    let provider = model
        .providers
        .get(&resolved.route.profile)
        .ok_or_else(|| RuntimeError::Model {
            reason: "resolved search provider profile is unavailable".into(),
        })?;
    if provider.client.hosted_web_search().is_none() {
        return Ok(None);
    }
    let (model_id, _) = config::types::provider::parse_model_speed(&resolved.route.model_id);
    Ok(Some(Arc::new(HostedSearchProvider {
        client: Arc::clone(&provider.client),
        auth: provider.auth.clone(),
        request: HostedWebSearchRequest {
            model: model_id.into(),
            query: String::new(),
            max_results: None,
            reasoning_effort: resolved.reasoning_effort,
            observation: Some(providers::ObservationContext {
                run_id: invocation.run_id.clone(),
                purpose: event_bus::RequestPurpose::WebSearch,
            }),
            usage_sink: Some(usage_sink),
        },
    })))
}

struct HostedSearchProvider {
    client: Arc<dyn providers::ProviderClient>,
    auth: providers::ProviderAuth,
    request: HostedWebSearchRequest,
}

#[async_trait::async_trait]
impl SearchProvider for HostedSearchProvider {
    fn name(&self) -> &str {
        "codex"
    }

    fn uses_credentials(&self) -> bool {
        true
    }

    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<SearchResults, SearchError> {
        let capability = self.client.hosted_web_search().ok_or_else(|| {
            SearchError::Protocol("selected hosted-search capability is unavailable".into())
        })?;
        let mut request = self.request.clone();
        request.query = query.to_owned();
        request.max_results = options.max_results;
        let response =
            capability
                .search(&self.auth, &request)
                .await
                .map_err(|error| match error {
                    HostedWebSearchError::Unsupported => SearchError::CodexUnsupported,
                    HostedWebSearchError::Provider(error) => search_error(error),
                })?;
        let limit = usize::try_from(options.max_results.unwrap_or(10).clamp(1, 10)).unwrap_or(10);
        let mut seen = std::collections::HashSet::new();
        let citations: Vec<_> = response
            .citations
            .into_iter()
            .filter(|citation| seen.insert(citation.url.clone()))
            .take(limit)
            .collect();
        let mut content = response.text;
        for citation in &citations {
            use std::fmt::Write;
            // Writing into String is infallible.
            let _ = write!(
                content,
                "\n\nTitle: {}\nURL: {}",
                citation.title, citation.url
            );
        }
        Ok(SearchResults {
            content,
            result_count: citations.len(),
            request_id: response.response_id,
            usage: Some(serde_json::json!(response.usage)),
        })
    }
}

fn search_error(error: ProviderError) -> SearchError {
    // Never expose upstream error bodies, URLs, or credentials in tool output.
    match error {
        ProviderError::RateLimited { .. } => SearchError::HttpStatus(429),
        ProviderError::Http { status, .. } => SearchError::HttpStatus(status),
        ProviderError::Timeout => SearchError::Timeout,
        ProviderError::RetriesExhausted { last, .. } => search_error(*last),
        ProviderError::InvalidSse { .. } | ProviderError::InvalidJson { .. } => {
            SearchError::Protocol("invalid hosted-search response".into())
        }
        ProviderError::Request(_) | ProviderError::Transport { .. } => {
            SearchError::Transport("hosted-search request failed".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_error_mapping_preserves_fallback_classes_without_returning_private_detail() {
        for (error, fallback) in [
            (
                ProviderError::Http {
                    status: 400,
                    body: "private-token".into(),
                },
                false,
            ),
            (
                ProviderError::Http {
                    status: 403,
                    body: "private-token".into(),
                },
                false,
            ),
            (
                ProviderError::Http {
                    status: 503,
                    body: "private-token".into(),
                },
                true,
            ),
            (ProviderError::RateLimited { retry_after: None }, true),
            (ProviderError::Timeout, true),
            (
                ProviderError::InvalidSse {
                    detail: "private-token".into(),
                },
                false,
            ),
            (
                ProviderError::InvalidJson {
                    detail: "private-token".into(),
                },
                false,
            ),
            (ProviderError::Request("private-token".into()), false),
            (
                ProviderError::Transport {
                    message: "private-token".into(),
                },
                false,
            ),
            (
                ProviderError::RetriesExhausted {
                    attempts: 2,
                    last: Box::new(ProviderError::Timeout),
                },
                true,
            ),
        ] {
            let result = search_error(error);
            assert_eq!(result.is_fallback_trigger(), fallback);
            assert!(!result.to_string().contains("private-token"));
        }
    }
}
