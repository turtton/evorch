//! Offline CEF pages for the agent's `inspect_artifact` tool (ADR 0034 B).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;

use browser::agent::PageAgent;
use browser_cef::AgentPage;
use runtime::artifacts::{ArtifactPage, ArtifactPages};

/// Each page has its own renderer process, so only a few runs inspect at once.
const MAX_PAGES: usize = 4;

/// One [`AgentPage`] per run. Pages are closed explicitly, never by a last
/// `Arc` drop, so [`CefArtifactPages::shutdown`] can close them all before
/// CEF shuts down even while tool calls still hold them.
pub struct CefArtifactPages {
    inner: Arc<Inner>,
}

struct Inner {
    // Read-held while a page opens or closes; shutdown write-locks it so CEF
    // never shuts down under one of those operations.
    alive: RwLock<bool>,
    pages: Mutex<HashMap<String, Arc<CefPage>>>,
    timeout: Duration,
}

struct CefPage(AgentPage);

impl ArtifactPage for CefPage {
    fn agent(&self) -> &PageAgent {
        self.0.agent()
    }

    fn take_blocked(&self) -> Vec<String> {
        self.0.take_blocked()
    }

    fn take_dialogs(&self) -> Vec<String> {
        self.0.take_dialogs()
    }
}

impl CefArtifactPages {
    /// `timeout` bounds loading a page and each DevTools command on it.
    pub(super) fn new(timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                alive: RwLock::new(true),
                pages: Mutex::default(),
                timeout,
            }),
        }
    }

    /// Closes every page and refuses new ones; call before CEF shuts down.
    pub(super) fn shutdown(&self) {
        let mut alive = self.inner.alive.write().unwrap_or_else(|e| e.into_inner());
        *alive = false;
        let pages: Vec<_> = self.inner.lock().drain().collect();
        for (_, page) in pages {
            page.0.close();
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<CefPage>>> {
        self.pages.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn open(&self, key: String, html: Vec<u8>) -> Result<Arc<CefPage>, String> {
        let alive = self.alive.read().unwrap_or_else(|e| e.into_inner());
        if !*alive {
            return Err("the GUI is shutting down".into());
        }
        let previous = self.lock().remove(&key);
        if let Some(previous) = previous {
            previous.0.close();
        }
        if self.lock().len() >= MAX_PAGES {
            return Err(format!(
                "{MAX_PAGES} runs are already inspecting artifacts; try again after one closes its page"
            ));
        }
        let page = Arc::new(CefPage(AgentPage::open(html, self.timeout)?));
        let replaced = self.lock().insert(key, Arc::clone(&page));
        if let Some(replaced) = replaced {
            replaced.0.close();
        }
        Ok(page)
    }

    fn close(&self, key: &str) {
        let alive = self.alive.read().unwrap_or_else(|e| e.into_inner());
        if !*alive {
            return;
        }
        let page = self.lock().remove(key);
        if let Some(page) = page {
            page.0.close();
        }
    }
}

#[async_trait::async_trait]
impl ArtifactPages for CefArtifactPages {
    async fn open(&self, key: &str, html: Vec<u8>) -> Result<Arc<dyn ArtifactPage>, String> {
        let inner = Arc::clone(&self.inner);
        let key = key.to_owned();
        let page = tokio::task::spawn_blocking(move || inner.open(key, html))
            .await
            .map_err(|error| format!("opening the artifact page failed: {error}"))??;
        Ok(page)
    }

    fn page(&self, key: &str) -> Option<Arc<dyn ArtifactPage>> {
        self.inner
            .lock()
            .get(key)
            .map(|page| Arc::clone(page) as Arc<dyn ArtifactPage>)
    }

    fn close(&self, key: &str) -> bool {
        if !self.inner.lock().contains_key(key) {
            return false;
        }
        let inner = Arc::clone(&self.inner);
        let key = key.to_owned();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(handle.spawn_blocking(move || inner.close(&key))),
            Err(_) => inner.close(&key),
        }
        true
    }
}
