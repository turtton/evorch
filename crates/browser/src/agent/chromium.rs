use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::fetch::EventRequestPaused;
use chromiumoxide::types::{Command, Method, MethodId};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{AgentError, CdpTarget, PageAgent, PageProvider, RequestPolicy};

pub struct LaunchOptions {
    /// `None` detects an installed Chromium/Chrome (honouring `CHROME`).
    pub executable: Option<PathBuf>,
    pub policy: Arc<dyn RequestPolicy>,
    /// Open pages beyond this close the least recently used one.
    pub max_pages: usize,
}

/// A headless Chromium for agents, launched on first use. Each key (a run)
/// gets its own page; every request of every page passes `policy`.
pub struct AgentBrowser {
    options: LaunchOptions,
    state: tokio::sync::Mutex<Option<Running>>,
}

struct Running {
    browser: Browser,
    handler: tokio::task::JoinHandle<()>,
    _profile: tempfile::TempDir,
    pages: VecDeque<(String, Arc<PageAgent>, Page)>,
}

impl AgentBrowser {
    pub fn new(options: LaunchOptions) -> Self {
        Self {
            options,
            state: tokio::sync::Mutex::new(None),
        }
    }

    /// The executable `options` would launch, if one can be found.
    pub fn detect(options: &LaunchOptions) -> Result<PathBuf, AgentError> {
        match &options.executable {
            Some(path) => Ok(path.clone()),
            None => chromiumoxide::detection::default_executable(Default::default())
                .map_err(AgentError::NotFound),
        }
    }

    async fn launch(&self) -> Result<Running, AgentError> {
        let executable = Self::detect(&self.options)?;
        let profile = tempfile::tempdir().map_err(|error| AgentError::Launch(error.to_string()))?;
        let config = BrowserConfig::builder()
            .new_headless_mode()
            .chrome_executable(executable)
            .user_data_dir(profile.path())
            .window_size(1280, 800)
            .launch_timeout(Duration::from_secs(20))
            .request_timeout(Duration::from_secs(30))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--disable-extensions",
                "--mute-audio",
                // Keep cross-origin frames in the page session so its Fetch
                // interception covers them, and refuse popups that would open
                // unintercepted targets.
                "--disable-features=IsolateOrigins,site-per-process",
                "--block-new-web-contents",
                "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
            ])
            .build()
            .map_err(AgentError::Launch)?;
        let (browser, mut events) = Browser::launch(config)
            .await
            .map_err(|error| AgentError::Launch(error.to_string()))?;
        let handler = tokio::spawn(async move { while events.next().await.is_some() {} });
        Ok(Running {
            browser,
            handler,
            _profile: profile,
            pages: VecDeque::new(),
        })
    }

    async fn open(&self, running: &Running) -> Result<(Arc<PageAgent>, Page), AgentError> {
        let page = running
            .browser
            .new_page("about:blank")
            .await
            .map_err(|error| AgentError::Launch(error.to_string()))?;
        let mut paused = page
            .event_listener::<EventRequestPaused>()
            .await
            .map_err(|error| AgentError::Launch(error.to_string()))?;
        let target = Arc::new(ChromiumTarget(page.clone()));
        target
            .call(
                "Fetch.enable",
                json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}),
            )
            .await?;
        let agent = Arc::new(PageAgent::new(target));
        let recorder = Arc::clone(&agent);
        let policy = Arc::clone(&self.options.policy);
        let guarded = page.clone();
        tokio::spawn(async move {
            while let Some(event) = paused.next().await {
                let page = guarded.clone();
                let policy = Arc::clone(&policy);
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    decide(&page, &event, policy.as_ref(), &recorder).await;
                });
            }
        });
        Ok((agent, page))
    }
}

async fn decide(
    page: &Page,
    event: &EventRequestPaused,
    policy: &dyn RequestPolicy,
    agent: &PageAgent,
) {
    let request_id = event.request_id.inner().clone();
    let verdict = match url::Url::parse(&event.request.url) {
        Ok(url) => policy.check(&url).await,
        Err(error) => Err(error.to_string()),
    };
    let (method, params) = match verdict {
        Ok(()) => ("Fetch.continueRequest", json!({"requestId": request_id})),
        Err(reason) => {
            agent.record_blocked(format!("{} ({reason})", event.request.url));
            (
                "Fetch.failRequest",
                json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
            )
        }
    };
    if let Err(error) = page.execute(Raw::new(method, params)).await {
        tracing::debug!(%error, method, "agent browser request decision was not delivered");
    }
}

#[async_trait::async_trait]
impl PageProvider for AgentBrowser {
    async fn page(&self, key: &str) -> Result<Arc<PageAgent>, AgentError> {
        let mut state = self.state.lock().await;
        if state
            .as_ref()
            .is_some_and(|running| running.handler.is_finished())
        {
            *state = None;
        }
        if state.is_none() {
            *state = Some(self.launch().await?);
        }
        let running = state.as_mut().expect("launched above");
        if let Some(index) = running.pages.iter().position(|(owner, ..)| owner == key) {
            let entry = running.pages.remove(index).expect("index from position");
            let agent = Arc::clone(&entry.1);
            running.pages.push_back(entry);
            return Ok(agent);
        }
        let (agent, page) = self.open(running).await?;
        running
            .pages
            .push_back((key.to_owned(), Arc::clone(&agent), page));
        while running.pages.len() > self.options.max_pages.max(1) {
            if let Some((_, _, page)) = running.pages.pop_front() {
                let _ = page.close().await;
            }
        }
        Ok(agent)
    }

    async fn close(&self, key: &str) -> bool {
        let mut state = self.state.lock().await;
        let Some(running) = state.as_mut() else {
            return false;
        };
        let Some(index) = running.pages.iter().position(|(owner, ..)| owner == key) else {
            return false;
        };
        if let Some((_, _, page)) = running.pages.remove(index) {
            let _ = page.close().await;
        }
        true
    }

    fn release(&self, key: &str) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        let Some(running) = state.as_mut() else {
            return;
        };
        let Some(index) = running.pages.iter().position(|(owner, ..)| owner == key) else {
            return;
        };
        if let Some((_, _, page)) = running.pages.remove(index)
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = page.close().await;
            });
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.handler.abort();
    }
}

struct ChromiumTarget(Page);

#[async_trait::async_trait]
impl CdpTarget for ChromiumTarget {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.0
            .execute(Raw::new(method, params))
            .await
            .map(|response| response.result)
            .map_err(|error| AgentError::Cdp {
                method: method.to_owned(),
                detail: error.to_string(),
            })
    }

    async fn navigate(&self, url: &str) -> Result<(), AgentError> {
        self.0
            .goto(url)
            .await
            .map(|_| ())
            .map_err(|error| AgentError::Navigation(error.to_string()))
    }

    async fn settle(&self) -> Result<(), AgentError> {
        self.0
            .wait_for_navigation()
            .await
            .map(|_| ())
            .map_err(|error| AgentError::Navigation(error.to_string()))
    }
}

#[derive(Debug)]
struct Raw {
    method: String,
    params: Value,
}

impl Raw {
    fn new(method: &str, params: Value) -> Self {
        Self {
            method: method.to_owned(),
            params,
        }
    }
}

impl serde::Serialize for Raw {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.params.serialize(serializer)
    }
}

impl Method for Raw {
    fn identifier(&self) -> MethodId {
        self.method.clone().into()
    }
}

impl Command for Raw {
    type Response = Value;
}
