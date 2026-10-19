//! Agent-facing page operations (ADR 0034). Everything here speaks raw CDP
//! through [`CdpTarget`], so the same layer drives an external Chromium and
//! CEF's in-process DevTools.

mod chromium;
mod connection;
mod keys;
mod page;
mod proxy;
mod snapshot;
mod tabs;

use std::net::SocketAddr;
use std::sync::Arc;

pub use chromium::{AgentBrowser, LaunchOptions, Screencast};
pub use page::{Activity, DialogAnswer, DialogKind, PageAgent, UploadTarget};
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

    /// Shows the session history entry `entry_id` (an `id` from
    /// `Page.getNavigationHistory`) and resolves once it has loaded.
    async fn go_to_history_entry(&self, entry_id: i64) -> Result<(), AgentError> {
        self.call(
            "Page.navigateToHistoryEntry",
            serde_json::json!({"entryId": entry_id}),
        )
        .await?;
        self.settle().await
    }
}

/// Decides where an agent page may connect. The Fetch domain asks `check`
/// for every request it can pause; the proxy that carries all of the page's
/// traffic, including WebSockets and worker fetches, asks `resolve` and
/// connects only to the addresses it returns.
#[async_trait::async_trait]
pub trait RequestPolicy: Send + Sync {
    /// `Err` carries the reason shown to the model.
    async fn check(&self, url: &url::Url) -> Result<(), String>;

    /// Resolves `host:port` for a connection whose payload is TLS (`tls`) or
    /// plain HTTP and WebSocket traffic.
    async fn resolve(&self, host: &str, port: u16, tls: bool) -> Result<Vec<SocketAddr>, String>;
}

/// Hands out pages per key (a run) and releases them when the run ends. A
/// run starts with one tab; tabs its pages open join it, and the tools act
/// on its active tab.
#[async_trait::async_trait]
pub trait PageProvider: Send + Sync {
    async fn page(&self, key: &str) -> Result<Arc<PageAgent>, AgentError>;

    /// The key's page if one is open; never launches or opens one.
    async fn open_page(&self, key: &str) -> Option<Arc<PageAgent>>;

    /// Closes the key's page; returns whether one was open.
    async fn close(&self, key: &str) -> bool;

    /// Closes the key's page without waiting, for synchronous teardown paths.
    fn release(&self, key: &str);

    /// The key's open tabs, in the order they opened.
    async fn tabs(&self, _key: &str) -> Result<Vec<TabInfo>, AgentError> {
        Ok(Vec::new())
    }

    /// Makes tab `id` (such as `t2`) the key's active tab and returns it.
    async fn switch_tab(&self, _key: &str, _id: &str) -> Result<Arc<PageAgent>, AgentError> {
        Err(AgentError::Refused("this browser has a single tab".into()))
    }

    /// Closes tab `id`; closing the last one closes the key's page.
    async fn close_tab(&self, _key: &str, _id: &str) -> Result<(), AgentError> {
        Err(AgentError::Refused("this browser has a single tab".into()))
    }

    /// Tabs the key's pages opened or closed since the last call.
    fn tab_news(&self, _key: &str) -> Vec<String> {
        Vec::new()
    }
}

/// One tab of a run, as [`PageProvider::tabs`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    pub id: String,
    pub url: String,
    pub title: String,
    pub active: bool,
    /// The tab whose page opened this one.
    pub opener: Option<String>,
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
    #[error("no open browser page for {0}")]
    NoPage(String),
    #[error("browser launch failed: {0}")]
    Launch(String),
    #[error("navigation failed: {0}")]
    Navigation(String),
    #[error("{0}")]
    Refused(String),
    #[error("invalid CDP response for {method}: {detail}")]
    Response { method: String, detail: String },
}
