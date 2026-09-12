//! Account quota acquisition; call `fetch_quota` repeatedly to poll with caching.

mod rpc;
mod wire;

use super::tokens::CodexTokenStore;
use chrono::{DateTime, Utc};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub window_duration: Duration,
    pub resets_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexQuota {
    pub plan: Option<String>,
    pub primary: Option<QuotaWindow>,
    pub secondary: Option<QuotaWindow>,
    pub code_review: Option<QuotaWindow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaSource {
    AppServer,
    Wham,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum QuotaError {
    #[error("app-server process I/O: {0}")]
    Process(std::io::ErrorKind),
    #[error("quota request timed out")]
    Timeout,
    #[error("invalid quota protocol: {0}")]
    Protocol(&'static str),
    #[error("app-server RPC error {0}")]
    Rpc(i64),
    #[error("quota credentials unavailable or invalid")]
    Credentials,
    #[error("quota HTTP transport failed")]
    HttpTransport,
    #[error("quota HTTP status {0}")]
    HttpStatus(u16),
    #[error("both quota sources failed: app-server ({app_server}); wham ({wham})")]
    Sources {
        app_server: Box<Self>,
        wham: Box<Self>,
    },
}

#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    pub quota: CodexQuota,
    pub source: QuotaSource,
    pub stale: bool,
    pub fetched_at: DateTime<Utc>,
    pub last_error: Option<QuotaError>,
}

/// Command/endpoint overrides are trusted configuration, never model input.
#[derive(Debug, Clone)]
pub struct QuotaConfig {
    pub app_server_program: PathBuf,
    pub app_server_args: Vec<String>,
    pub wham_endpoint: String,
    pub timeout: Duration,
    pub refresh_interval: Duration,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            app_server_program: "codex".into(),
            app_server_args: vec!["app-server".into(), "--stdio".into()],
            wham_endpoint: "https://chatgpt.com/backend-api/wham/usage".into(),
            timeout: Duration::from_secs(10),
            refresh_interval: Duration::from_secs(60),
        }
    }
}

impl QuotaConfig {
    pub fn retry_delay(&self, failures: u32) -> Duration {
        self.refresh_interval
            .clamp(Duration::from_secs(30), Duration::from_secs(600))
            .saturating_mul(2_u32.saturating_pow(failures.min(10)))
            .min(Duration::from_secs(600))
    }
}

/// One client per account. Recreate on account switches to discard cached data.
/// The app-server uses its own Codex login; supply the matching token store.
pub struct CodexQuotaClient {
    config: QuotaConfig,
    store: Arc<dyn CodexTokenStore>,
    http: reqwest::Client,
    snapshot: Option<QuotaSnapshot>,
    last_error: Option<QuotaError>,
    attempted_at: Option<Instant>,
    failures: u32,
}

impl CodexQuotaClient {
    /// # Errors
    /// Returns `HttpTransport` if TLS/client setup fails.
    pub fn new(config: QuotaConfig, store: Arc<dyn CodexTokenStore>) -> Result<Self, QuotaError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.timeout)
            .build()
            .map_err(|_| QuotaError::HttpTransport)?;
        Ok(Self {
            config,
            store,
            http,
            snapshot: None,
            last_error: None,
            attempted_at: None,
            failures: 0,
        })
    }

    pub fn poll_interval(&self) -> Duration {
        self.config.retry_delay(self.failures)
    }

    /// Fetch from app-server, then WHAM on any primary error. Within the polling
    /// interval return cache; after failures retain the last good data as stale.
    /// No background task is spawned. Dropping the future kills its child process.
    /// # Errors
    /// Returns both source errors when no good snapshot has ever been acquired.
    pub async fn fetch_quota(&mut self) -> Result<QuotaSnapshot, QuotaError> {
        if self
            .attempted_at
            .is_some_and(|at| at.elapsed() < self.poll_interval())
        {
            if let Some(snapshot) = &self.snapshot {
                return Ok(snapshot.clone());
            }
            if let Some(error) = &self.last_error {
                return Err(error.clone());
            }
        }
        let result = match rpc::fetch(&self.config).await {
            Ok(quota) => Ok((quota, QuotaSource::AppServer)),
            Err(app_server) => wire::fetch_wham(&self.http, &self.config, self.store.as_ref())
                .await
                .map(|quota| (quota, QuotaSource::Wham))
                .map_err(|wham| QuotaError::Sources {
                    app_server: Box::new(app_server),
                    wham: Box::new(wham),
                }),
        };
        self.attempted_at = Some(Instant::now());
        match result {
            Ok((quota, source)) => {
                self.failures = 0;
                self.last_error = None;
                let snapshot = QuotaSnapshot {
                    quota,
                    source,
                    stale: false,
                    fetched_at: SystemTime::now().into(),
                    last_error: None,
                };
                self.snapshot = Some(snapshot.clone());
                Ok(snapshot)
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                self.last_error = Some(error.clone());
                match &mut self.snapshot {
                    Some(snapshot) => {
                        snapshot.stale = true;
                        snapshot.last_error = Some(error);
                        Ok(snapshot.clone())
                    }
                    None => Err(error),
                }
            }
        }
    }
}
