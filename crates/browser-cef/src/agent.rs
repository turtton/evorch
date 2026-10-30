//! Agent access to an artifact through CEF's in-process DevTools (ADR 0034):
//! the same [`PageAgent`] that drives external Chromium, on an offline page.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use browser::agent::{AgentError, CdpTarget, PageAgent};
use serde_json::Value;

use crate::artifact::{ArtifactContext, NetLog};
use crate::browser::{Dialogs, OsrBrowser, Shared};
use crate::devtools::DevTools;
use crate::offline::{ArtifactBundle, INDEX};
use crate::snapshot::SNAPSHOT_SIZE;

/// A [`CdpTarget`] on one artifact page. It cannot navigate, so the page
/// never leaves the artifact's offline context.
pub struct CefTarget {
    devtools: DevTools,
    shared: Arc<Shared>,
    timeout: Duration,
}

#[async_trait::async_trait]
impl CdpTarget for CefTarget {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.devtools
            .call_async(method, params, self.timeout)
            .await
            .map_err(|detail| AgentError::Cdp {
                method: method.to_owned(),
                detail,
            })
    }

    async fn navigate(&self, _url: &str) -> Result<(), AgentError> {
        Err(AgentError::Navigation(
            "an artifact page cannot leave its artifact".into(),
        ))
    }

    /// Waits for a load that is already under way; a load the input has not
    /// started yet is not awaited.
    async fn settle(&self) -> Result<(), AgentError> {
        let shared = Arc::clone(&self.shared);
        let timeout = self.timeout;
        let settled = tokio::task::spawn_blocking(move || {
            shared.wait(Some(timeout), |shared| {
                let status = shared.status();
                status.closed || !status.loading
            })
        })
        .await
        .unwrap_or(false);
        if settled {
            Ok(())
        } else {
            Err(AgentError::Navigation(
                "the page did not finish loading".into(),
            ))
        }
    }
}

/// An artifact opened offline in a browser no window shows, at
/// [`SNAPSHOT_SIZE`], for an agent to read and operate.
pub struct AgentPage {
    agent: Arc<PageAgent>,
    target: Arc<CefTarget>,
    log: Arc<NetLog>,
    reported: Mutex<Reported>,
    browser: OsrBrowser,
}

#[derive(Default)]
struct Reported {
    requests: usize,
    navigations: usize,
    popups: usize,
    dialogs: usize,
}

impl AgentPage {
    /// Loads `html` and waits up to `timeout` for it to load and paint;
    /// `timeout` also bounds every later DevTools command.
    pub fn open(html: Vec<u8>, timeout: Duration) -> Result<Self, String> {
        let context = ArtifactContext::create(ArtifactBundle::html(html))?;
        let log = Arc::clone(&context.log);
        let browser = OsrBrowser::create(
            context,
            &ArtifactContext::url(INDEX),
            SNAPSHOT_SIZE,
            1.0,
            Dialogs::Suppress,
            10,
        )?;
        browser
            .wait_settled(timeout)
            .map_err(|error| format!("the artifact did not settle: {error}"))?;
        let target = Arc::new(CefTarget {
            devtools: DevTools::attach(&browser)?,
            shared: Arc::clone(&browser.shared),
            timeout,
        });
        Ok(Self {
            agent: Arc::new(PageAgent::new(Arc::clone(&target) as Arc<dyn CdpTarget>)),
            target,
            log,
            reported: Mutex::default(),
            browser,
        })
    }

    pub fn agent(&self) -> &PageAgent {
        &self.agent
    }

    /// What the offline policy refused since the last call.
    pub fn take_blocked(&self) -> Vec<String> {
        let mut guard = self.reported.lock().unwrap_or_else(|e| e.into_inner());
        let reported = &mut *guard;
        let requests = self.log.blocked.lock().unwrap_or_else(|e| e.into_inner());
        let status = self.browser.status();
        let mut blocked = Vec::new();
        for (label, entries, seen) in [
            ("request", &requests[..], &mut reported.requests),
            (
                "navigation",
                &status.blocked_navigations[..],
                &mut reported.navigations,
            ),
            ("popup", &status.blocked_popups[..], &mut reported.popups),
        ] {
            blocked.extend(
                entries
                    .iter()
                    .skip(*seen)
                    .map(|url| format!("{label} {url} (artifacts are offline)")),
            );
            *seen = entries.len();
        }
        blocked
    }

    /// Dialogs the page opened since the last call. Nobody sees them: alerts
    /// are closed, confirm and prompt dismissed and beforeunload left.
    pub fn take_dialogs(&self) -> Vec<String> {
        let mut reported = self.reported.lock().unwrap_or_else(|e| e.into_inner());
        let status = self.browser.status();
        let dialogs = status.suppressed_dialogs[reported.dialogs..].to_vec();
        reported.dialogs = status.suppressed_dialogs.len();
        dialogs
    }

    /// Detaches the agent and closes the browser. Agent calls made afterwards
    /// fail, so a retained [`PageAgent`] never reaches CEF after shutdown.
    pub fn close(&self) {
        self.target.devtools.detach();
        self.browser.close();
    }
}

impl Drop for AgentPage {
    fn drop(&mut self) {
        self.close();
    }
}
