//! Account-scoped authentication. HTTP clients are reused, session caches are not.
use std::sync::{Arc, Mutex};

use super::super::{
    oauth::DeviceAuthClient,
    session::CodexSessionManager,
    tokens::{CodexTokenStore, TokenBundle},
};
use super::{CodexQuota, QuotaConfig, QuotaError, wire};
use crate::ProviderError;

pub(super) async fn fetch(
    http: &reqwest::Client,
    config: &QuotaConfig,
    store: Arc<dyn CodexTokenStore>,
    rejected: &mut Option<TokenBundle>,
) -> Result<CodexQuota, QuotaError> {
    let bundle = store
        .load()
        .map_err(|_| QuotaError::Credentials)?
        .ok_or(QuotaError::Credentials)?;
    if rejected.as_ref() == Some(&bundle) {
        return Err(QuotaError::ReauthenticationRequired);
    }
    *rejected = None;
    let observed = Arc::new(ObservedStore {
        inner: store,
        last: Mutex::new(None),
    });
    let session = CodexSessionManager::new(
        observed.clone(),
        DeviceAuthClient::new(&config.auth_base_url, http.clone()),
    );
    let result = async {
        let token = session.current().await.map_err(map_session_error)?;
        match wire::fetch_authorized(http, config, &token).await {
            Err(QuotaError::ReauthenticationRequired) => {
                let recovered = session
                    .refresh_after_unauthorized(&token)
                    .await
                    .map_err(map_session_error)?;
                wire::fetch_authorized(http, config, &recovered).await
            }
            result => result,
        }
    }
    .await;
    if matches!(result, Err(QuotaError::ReauthenticationRequired)) {
        // Remember the bundle actually used, not a later login that might race
        // this failure. Do not repeat a rejected refresh on subsequent polls.
        *rejected = observed
            .last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
    }
    result
}

// Observe the session's loads/saves without taking the non-reentrant refresh
// lock here. Only the session owns that lock, across reload/refresh/save.
struct ObservedStore {
    inner: Arc<dyn CodexTokenStore>,
    last: Mutex<Option<TokenBundle>>,
}
impl CodexTokenStore for ObservedStore {
    fn load(&self) -> Result<Option<TokenBundle>, ProviderError> {
        let bundle = self.inner.load()?;
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = bundle.clone();
        Ok(bundle)
    }
    fn save(&self, bundle: &TokenBundle) -> Result<(), ProviderError> {
        self.inner.save(bundle)?;
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(bundle.clone());
        Ok(())
    }
    fn refresh_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.inner.refresh_lock()
    }
}

fn map_session_error(error: ProviderError) -> QuotaError {
    match error {
        ProviderError::Http {
            status: 400 | 401 | 403,
            ..
        } => QuotaError::ReauthenticationRequired,
        ProviderError::Http { status, .. } => QuotaError::HttpStatus(status),
        ProviderError::RateLimited { .. } => QuotaError::HttpStatus(429),
        ProviderError::Timeout => QuotaError::Timeout,
        ProviderError::Transport { .. } => QuotaError::HttpTransport,
        _ => QuotaError::Credentials,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_error_mapping_keeps_transport_statuses_and_discards_details() {
        for status in [400, 401, 403] {
            assert!(matches!(
                map_session_error(ProviderError::Http {
                    status,
                    body: "SECRET".into()
                }),
                QuotaError::ReauthenticationRequired
            ));
        }
        for status in [408, 425, 429, 500, 503] {
            assert!(
                matches!(map_session_error(ProviderError::Http { status, body: "SECRET".into() }), QuotaError::HttpStatus(s) if s == status)
            );
        }
        assert!(matches!(
            map_session_error(ProviderError::Timeout),
            QuotaError::Timeout
        ));
        assert!(matches!(
            map_session_error(ProviderError::RateLimited { retry_after: None }),
            QuotaError::HttpStatus(429)
        ));
        assert!(matches!(
            map_session_error(ProviderError::Transport {
                message: "SECRET".into()
            }),
            QuotaError::HttpTransport
        ));
        for error in [
            ProviderError::Request("SECRET".into()),
            ProviderError::InvalidJson {
                detail: "SECRET".into(),
            },
        ] {
            let mapped = map_session_error(error);
            assert!(matches!(mapped, QuotaError::Credentials));
            assert!(!format!("{mapped:?}").contains("SECRET"));
        }
    }
}
