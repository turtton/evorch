mod cache;
mod metadata;

pub use metadata::{Modalities, ModelMetadata};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;

const ENDPOINT: &str = "https://models.dev/api.json";
const TTL: Duration = Duration::from_secs(24 * 60 * 60);
type Api = BTreeMap<String, metadata::Provider>;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("catalog cache I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid catalog JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("catalog task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("another process is refreshing the catalog")]
    RefreshBusy,
}

#[derive(Debug)]
pub struct ModelCatalog {
    api: Api,
    fetched_at: u64,
    refresh: Option<JoinHandle<Result<Self, CatalogError>>>,
}

impl ModelCatalog {
    /// Returns cached data; stale data schedules one refresh on the current Tokio runtime.
    pub async fn load_or_refresh(cache_dir: &Path) -> Result<Self, CatalogError> {
        Self::load_with(cache_dir, ENDPOINT, false).await
    }

    /// Fetches immediately, falling back to readable cached data on failure.
    pub async fn force_refresh(cache_dir: &Path) -> Result<Self, CatalogError> {
        Self::load_with(cache_dir, ENDPOINT, true).await
    }

    pub fn find(&self, provider: &str, model: &str) -> Option<&ModelMetadata> {
        self.api.get(provider)?.models.get(model)
    }

    /// For duplicate model IDs, the lexicographically first provider ID wins.
    pub fn find_by_model_id(&self, model: &str) -> Option<&ModelMetadata> {
        self.api
            .values()
            .find_map(|provider| provider.models.get(model))
    }

    /// Takes the scheduled refresh handle; dropping it does not cancel the refresh.
    pub fn take_refresh(&mut self) -> Option<JoinHandle<Result<Self, CatalogError>>> {
        self.refresh.take()
    }

    fn is_fresh(&self) -> bool {
        now()
            .checked_sub(self.fetched_at)
            .is_some_and(|age| age < TTL.as_secs())
    }

    async fn load_with(dir: &Path, endpoint: &str, force: bool) -> Result<Self, CatalogError> {
        let cached = match cache::read(dir).await.ok() {
            Some(catalog) if !force && catalog.is_fresh() => return Ok(catalog),
            other => other,
        };
        let lock = match cache::lock(dir).await {
            Ok(lock) => lock,
            Err(error) => return cached.ok_or(error),
        };
        if !force {
            let current = cache::read(dir).await.ok();
            if let Some(current) = current.filter(Self::is_fresh) {
                return Ok(current);
            }
            if let Some(mut stale) = cached {
                let dir = dir.to_owned();
                let endpoint = endpoint.to_owned();
                stale.refresh = Some(tokio::spawn(async move {
                    Self::fetch(&dir, &endpoint, lock).await
                }));
                return Ok(stale);
            }
        }
        match Self::fetch(dir, endpoint, lock).await {
            Ok(catalog) => Ok(catalog),
            Err(error) => cached.ok_or(error),
        }
    }

    async fn fetch(dir: &Path, endpoint: &str, lock: std::fs::File) -> Result<Self, CatalogError> {
        let bytes = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()?
            .get(endpoint)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let dir = dir.to_owned();
        tokio::task::spawn_blocking(move || {
            let api: Api = serde_json::from_slice(&bytes)?;
            let fetched_at = now();
            cache::write(&dir, fetched_at, &bytes)?;
            drop(lock);
            Ok(Self {
                api,
                fetched_at,
                refresh: None,
            })
        })
        .await?
    }
}

/// Returns XDG_CACHE_HOME/evorch, HOME/.cache/evorch, or a temporary fallback.
pub fn default_cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("evorch")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod edge_tests;
