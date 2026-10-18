//! Agent-facing page operations (ADR 0034). Everything here speaks raw CDP
//! through [`CdpTarget`], so the same layer drives an external Chromium and
//! CEF's in-process DevTools.

mod chromium;
mod keys;
mod page;
mod snapshot;

use std::sync::Arc;

pub use chromium::{AgentBrowser, LaunchOptions};
pub use page::PageAgent;
pub use snapshot::{MAX_SNAPSHOT_LINES, Snapshot, render};

/// One page reachable over CDP.
#[async_trait::async_trait]
pub trait CdpTarget: Send + Sync {
    /// Sends one command to the page session and returns its `result` object.
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, AgentError>;

    /// Loads `url` and resolves once the new document has loaded.
    async fn navigate(&self, url: &str) -> Result<(), AgentError>;

    /// Waits for a navigation the last input may have started. Targets that
    /// cannot track navigations return immediately.
    async fn settle(&self) -> Result<(), AgentError> {
        Ok(())
    }
}

/// Decides whether the page may load a URL. Every request of an agent page,
/// including redirects and subresources, passes through it.
#[async_trait::async_trait]
pub trait RequestPolicy: Send + Sync {
    /// `Err` carries the reason shown to the model.
    async fn check(&self, url: &url::Url) -> Result<(), String>;
}

/// Hands out one page per key (a run) and releases it when the run ends.
#[async_trait::async_trait]
pub trait PageProvider: Send + Sync {
    async fn page(&self, key: &str) -> Result<Arc<PageAgent>, AgentError>;

    /// Closes the key's page; returns whether one was open.
    async fn close(&self, key: &str) -> bool;

    /// Closes the key's page without waiting, for synchronous teardown paths.
    fn release(&self, key: &str);
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("CDP {method} failed: {detail}")]
    Cdp { method: String, detail: String },
    #[error("unknown ref {0}; take a new browser_snapshot and use a ref from it")]
    UnknownRef(String),
    #[error("element {0} has no visible box to click")]
    NotVisible(String),
    #[error("unsupported key {0:?}")]
    UnknownKey(String),
    #[error("no Chromium or Chrome executable found ({0}); install one or set CHROME to its path")]
    NotFound(String),
    #[error("browser launch failed: {0}")]
    Launch(String),
    #[error("navigation failed: {0}")]
    Navigation(String),
    #[error("invalid CDP response for {method}: {detail}")]
    Response { method: String, detail: String },
}
