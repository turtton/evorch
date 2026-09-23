use providers::ProviderError;

/// Try another candidate on malformed/unsupported requests, transient failures, exhausted quota,
/// or authentication failure.
/// HTTP eligibility depends only on status, not the response body.
pub(super) fn eligible(error: &ProviderError) -> bool {
    match error {
        ProviderError::Timeout
        | ProviderError::Transport { .. }
        | ProviderError::RateLimited { .. } => true,
        ProviderError::Http { status, .. } => {
            matches!(status, 400 | 401 | 402 | 403 | 408 | 429 | 500..=599)
        }
        ProviderError::RetriesExhausted { last, .. } => eligible(last),
        ProviderError::Request(_)
        | ProviderError::InvalidSse { .. }
        | ProviderError::InvalidJson { .. } => false,
    }
}
