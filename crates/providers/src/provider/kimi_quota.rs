//! Read Kimi Code subscription windows using the configured coding API key.
//!
//! Wire formats follow MoonshotAI/kimi-code packages/oauth/src/managed-usage.ts
//! and MoonshotAI/kimi-cli src/kimi_cli/ui/shell/usage.py (legacy plans).

use super::codex::quota::QuotaError;
use crate::ProviderAuth;
use chrono::{DateTime, Utc};
use std::time::{Duration, SystemTime};

mod wire;

#[derive(Debug, Clone, PartialEq)]
pub struct KimiQuotaWindow {
    pub label: String,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct KimiQuotaSnapshot {
    pub windows: Vec<KimiQuotaWindow>,
    pub stale: bool,
    pub fetched_at: DateTime<Utc>,
    pub last_error: Option<QuotaError>,
}

/// Endpoint overrides are user provider configuration, never model input.
pub struct KimiQuotaClient {
    endpoint: String,
    http: reqwest::Client,
}

impl KimiQuotaClient {
    /// # Errors
    /// Rejects invalid base URLs and HTTP client configuration failures.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, QuotaError> {
        let mut url = reqwest::Url::parse(base_url)
            .map_err(|_| QuotaError::Protocol("invalid Kimi base URL"))?;
        if !matches!(url.scheme(), "https" | "http")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(QuotaError::Protocol("invalid Kimi base URL"));
        }
        url.set_path(&format!("{}/usages", url.path().trim_end_matches('/')));
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent(concat!("evorch/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| QuotaError::HttpTransport)?;
        Ok(Self {
            endpoint: url.into(),
            http,
        })
    }

    /// # Errors
    /// Returns sanitized credential, transport, status, or wire-format errors.
    pub async fn fetch_quota(&self, auth: &ProviderAuth) -> Result<KimiQuotaSnapshot, QuotaError> {
        if auth.api_key.trim().is_empty() {
            return Err(QuotaError::Credentials);
        }
        let mut response = self
            .http
            .get(&self.endpoint)
            .bearer_auth(&auth.api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(transport_error)?;
        match response.status().as_u16() {
            200 => {}
            401 | 403 => return Err(QuotaError::ReauthenticationRequired),
            status => return Err(QuotaError::HttpStatus(status)),
        }
        // Quota payloads are small; never let an unexpected response consume unbounded memory.
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
                return Err(QuotaError::Protocol("Kimi quota response too large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let raw = serde_json::from_slice(&bytes)
            .map_err(|_| QuotaError::Protocol("invalid Kimi quota JSON"))?;
        Ok(KimiQuotaSnapshot {
            windows: wire::parse(&raw)?,
            stale: false,
            fetched_at: SystemTime::now().into(),
            last_error: None,
        })
    }
}

fn transport_error(error: reqwest::Error) -> QuotaError {
    if error.is_timeout() {
        QuotaError::Timeout
    } else {
        QuotaError::HttpTransport
    }
}
