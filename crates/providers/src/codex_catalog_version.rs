//! Shared Codex client-version discovery for catalog and inference requests.
//!
//! Inference clients pin the resolved version for their lifetime to preserve
//! prompt-cache affinity, even when the shared resolver cache expires.
//!
//! GitHub release titles are cosmetic (`0.156.1`); stable CLI tags are
//! `rust-v0.156.1`. Resolve only those tags, without sending Codex credentials.
//! The cache is process-local: successes live for 24 hours, failures retry after
//! five minutes and retain the last successful version. Restarting falls back
//! to the bundled version when GitHub is unavailable.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;

use crate::ProviderError;
use crate::http::{build_http_client, map_request_error};

/// Known catalog version. Sol/Luna require at least 0.155.0.
pub const CODEX_MODELS_FALLBACK_VERSION: &str = "0.156.1";
const RELEASES_URL: &str = "https://api.github.com/repos/openai/codex/releases";
const SUCCESS_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const RETRY_TTL: Duration = Duration::from_secs(5 * 60);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PAGES: usize = 5;
// GitHub's release list includes large bodies: 100 entries exceeded 27 MiB
// on 2026-09-23, while 10 entries stayed below 3 MiB.
const PAGE_SIZE: usize = 10;
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

/// Resolved Codex client version, with a visible fallback notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexCatalogVersion {
    pub version: String,
    pub warning: Option<String>,
}

/// Stable client version attached to Codex inference requests.
/// Resolved once per client so request headers stay stable for prompt-cache affinity.
#[derive(Clone, Debug)]
pub enum CodexClientVersion {
    /// Resolve the latest stable Codex CLI release once per client (production default).
    Resolve(Arc<CodexCatalogVersionResolver>),
    /// Pin an exact version (offline tests / pinned deployments).
    Fixed(CodexCatalogVersion),
}

impl Default for CodexClientVersion {
    fn default() -> Self {
        Self::Resolve(CodexCatalogVersionResolver::shared())
    }
}

impl CodexClientVersion {
    pub async fn resolve(&self) -> CodexCatalogVersion {
        match self {
            Self::Fixed(version) => version.clone(),
            Self::Resolve(resolver) => resolver.resolve().await,
        }
    }
}

#[derive(Debug)]
struct CachedVersion {
    value: CodexCatalogVersion,
    checked_at: Instant,
}

/// Shared, single-flight version resolver. The URL is injectable for offline tests.
#[derive(Debug)]
pub struct CodexCatalogVersionResolver {
    releases_url: String,
    cache: Mutex<Option<CachedVersion>>,
}

impl Default for CodexCatalogVersionResolver {
    fn default() -> Self {
        Self::new(RELEASES_URL)
    }
}

impl CodexCatalogVersionResolver {
    pub fn new(releases_url: impl Into<String>) -> Self {
        Self {
            releases_url: releases_url.into(),
            cache: Mutex::new(None),
        }
    }

    /// Reuse successful lookups across profiles and editor reopenings.
    pub fn shared() -> Arc<Self> {
        static RESOLVER: std::sync::OnceLock<Arc<CodexCatalogVersionResolver>> =
            std::sync::OnceLock::new();
        RESOLVER.get_or_init(|| Arc::new(Self::default())).clone()
    }

    /// GitHub errors must not prevent an authenticated model-catalog fetch.
    pub async fn resolve(&self) -> CodexCatalogVersion {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            let ttl = if cached.value.warning.is_some() {
                RETRY_TTL
            } else {
                SUCCESS_TTL
            };
            if cached.checked_at.elapsed() < ttl {
                return cached.value.clone();
            }
        }
        let value = match tokio::time::timeout(LOOKUP_TIMEOUT, self.fetch_latest()).await {
            Ok(Ok(version)) => CodexCatalogVersion {
                version,
                warning: None,
            },
            _ => {
                let version = cache.as_ref().map_or_else(
                    || CODEX_MODELS_FALLBACK_VERSION.to_owned(),
                    |cached| cached.value.version.clone(),
                );
                let source = if cache.is_some() {
                    "cached/bundled"
                } else {
                    "bundled"
                };
                CodexCatalogVersion {
                    warning: Some(format!(
                        "Could not check the latest Codex release; using {source} client version {version}. New models may be missing."
                    )),
                    version,
                }
            }
        };
        *cache = Some(CachedVersion {
            value: value.clone(),
            checked_at: Instant::now(),
        });
        value
    }

    async fn fetch_latest(&self) -> Result<String, ProviderError> {
        let client = build_http_client(Some(LOOKUP_TIMEOUT))?;
        // GitHub lists releases newest first. Skip pages containing only preview
        // or non-CLI releases; compare numeric versions on the first eligible
        // page rather than relying on tag/title lexicographic order.
        for page in 1..=MAX_PAGES {
            let mut response = client
                .get(&self.releases_url)
                .query(&[("per_page", PAGE_SIZE), ("page", page)])
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header(reqwest::header::USER_AGENT, "evorch-codex-catalog")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .send()
                .await
                .map_err(map_request_error)?;
            if !response.status().is_success() {
                // No remote error body is needed for the fallback notice.
                return Err(ProviderError::Request("Codex release lookup failed".into()));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(map_request_error)? {
                if body.len().saturating_add(chunk.len()) > MAX_PAGE_BYTES {
                    return Err(ProviderError::Request(
                        "Codex release page too large".into(),
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            let releases: Vec<Release> =
                serde_json::from_slice(&body).map_err(|error| ProviderError::InvalidJson {
                    detail: error.to_string(),
                })?;
            if let Some(version) = releases.iter().filter_map(Release::stable_version).max() {
                // Never regress below the bundled known-good catalog version.
                let floor = parse_version(CODEX_MODELS_FALLBACK_VERSION).expect("bundled version");
                let [major, minor, patch] = version.max(floor);
                return Ok(format!("{major}.{minor}.{patch}"));
            }
            if releases.len() < PAGE_SIZE {
                break;
            }
        }
        Err(ProviderError::Request(
            "No stable Codex CLI release found".into(),
        ))
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

impl Release {
    fn stable_version(&self) -> Option<[u64; 3]> {
        if self.draft || self.prerelease {
            return None;
        }
        parse_version(self.tag_name.strip_prefix("rust-v")?)
    }
}

// Only canonical stable SemVer cores are accepted: no suffixes, leading zeros,
// signs, whitespace, or extra components. Numeric tuple order is SemVer order.
fn parse_version(version: &str) -> Option<[u64; 3]> {
    let mut components = version.split('.');
    let mut result = [0; 3];
    for part in &mut result {
        let text = components.next()?;
        if text.is_empty()
            || !text.bytes().all(|c| c.is_ascii_digit())
            || (text.len() > 1 && text.starts_with('0'))
        {
            return None;
        }
        *part = text.parse().ok()?;
    }
    components.next().is_none().then_some(result)
}

#[cfg(test)]
#[path = "codex_catalog_version_tests.rs"]
mod tests;
