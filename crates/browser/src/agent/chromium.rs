use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::fetch::EventRequestPaused;
use chromiumoxide::cdp::browser_protocol::page::{
    DialogType, EventFrameStartedLoading, EventFrameStoppedLoading, EventJavascriptDialogOpening,
    EventNavigatedWithinDocument, EventScreencastFrame, ScreencastFrameAckParams,
    StartScreencastFormat, StartScreencastParams, StopScreencastParams,
};
use chromiumoxide::cdp::browser_protocol::target::CreateTargetParams;
use chromiumoxide::types::{Command, Method, MethodId};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::proxy::Proxy;
use super::{AgentError, CdpTarget, DialogKind, PageAgent, PageProvider, RequestPolicy};

const WINDOW: [u32; 2] = [1280, 800];

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
    pages: watch::Sender<Vec<String>>,
    /// Keys `release` could not close because the state was busy; the next
    /// holder of the state closes them.
    released: std::sync::Mutex<Vec<String>>,
}

struct Running {
    browser: Browser,
    handler: tokio::task::JoinHandle<()>,
    _profile: tempfile::TempDir,
    _proxy: Proxy,
    pages: VecDeque<(String, Arc<PageAgent>, Page)>,
}

impl AgentBrowser {
    pub fn new(options: LaunchOptions) -> Self {
        Self {
            options,
            state: tokio::sync::Mutex::new(None),
            pages: watch::Sender::new(Vec::new()),
            released: std::sync::Mutex::default(),
        }
    }

    /// Keys with an open page, sorted; it changes whenever a page opens or closes.
    pub fn pages(&self) -> watch::Receiver<Vec<String>> {
        self.pages.subscribe()
    }

    async fn close_released(&self, running: &mut Running) {
        let keys = std::mem::take(
            &mut *self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for key in keys {
            if let Some(index) = running.pages.iter().position(|(owner, ..)| *owner == key)
                && let Some((_, _, page)) = running.pages.remove(index)
            {
                let _ = page.close().await;
            }
        }
    }

    fn publish(&self, running: Option<&Running>) {
        let mut keys: Vec<String> = running
            .into_iter()
            .flat_map(|running| running.pages.iter().map(|(key, ..)| key.clone()))
            .collect();
        keys.sort();
        self.pages.send_if_modified(|current| {
            let changed = *current != keys;
            *current = keys;
            changed
        });
    }

    /// Starts a screencast of `key`'s open page. `on_frame` runs after each
    /// frame and once more when the page closes, e.g. to request a repaint.
    pub async fn screencast(
        &self,
        key: &str,
        on_frame: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Screencast, AgentError> {
        let mut state = self.state.lock().await;
        if let Some(running) = state.as_mut() {
            self.close_released(running).await;
            self.publish(Some(running));
        }
        let (agent, page) = state
            .as_ref()
            .and_then(|running| running.pages.iter().find(|(owner, ..)| owner == key))
            .map(|(_, agent, page)| (Arc::clone(agent), page.clone()))
            .ok_or_else(|| AgentError::NoPage(key.to_owned()))?;
        drop(state);
        let cdp = |error: chromiumoxide::error::CdpError| AgentError::Cdp {
            method: "Page.startScreencast".into(),
            detail: error.to_string(),
        };
        let mut events = page
            .event_listener::<EventScreencastFrame>()
            .await
            .map_err(cdp)?;
        page.execute(
            StartScreencastParams::builder()
                .format(StartScreencastFormat::Jpeg)
                .quality(60)
                .max_width(WINDOW[0])
                .max_height(WINDOW[1])
                .build(),
        )
        .await
        .map_err(cdp)?;
        let (frames_tx, frames) = watch::channel(None);
        let acknowledging = page.clone();
        let task = tokio::spawn(async move {
            while let Some(frame) = events.next().await {
                match crate::cdp::decode_jpeg(&frame.data, WINDOW) {
                    Ok(image) => {
                        frames_tx.send_replace(Some(image));
                    }
                    Err(error) => tracing::debug!(%error, "agent screencast frame dropped"),
                }
                on_frame();
                if acknowledging
                    .execute(ScreencastFrameAckParams::new(frame.session_id))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            drop(frames_tx);
            on_frame();
        });
        Ok(Screencast {
            agent,
            frames,
            page,
            runtime: tokio::runtime::Handle::current(),
            task,
        })
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
        let proxy = Proxy::start(Arc::clone(&self.options.policy)).await?;
        let config = BrowserConfig::builder()
            .new_headless_mode()
            .chrome_executable(executable)
            .user_data_dir(profile.path())
            .window_size(WINDOW[0], WINDOW[1])
            .launch_timeout(Duration::from_secs(20))
            .request_timeout(Duration::from_secs(30))
            // chromiumoxide adds the leading `--` and merges repeated keys
            // such as `disable-features`, so switches are bare keys here.
            .arg((
                "proxy-server",
                format!("http://{}", proxy.address()).as_str(),
            ))
            .args([
                "no-first-run",
                "no-default-browser-check",
                "disable-background-networking",
                "disable-component-update",
                "disable-sync",
                "disable-extensions",
                "mute-audio",
                // Refuse popups, which would open targets without Fetch
                // interception.
                "block-new-web-contents",
                // QUIC and direct WebRTC UDP would bypass the proxy.
                "disable-quic",
            ])
            .args([
                // Keep cross-origin frames in the page session so its Fetch
                // interception covers them.
                ("disable-features", "IsolateOrigins,site-per-process"),
                // Back and forward then always load the page again, which
                // they wait for like any other navigation.
                ("disable-features", "BackForwardCache"),
                // Loopback is otherwise bypassed implicitly; every connection
                // has to meet the policy at the proxy.
                ("proxy-bypass-list", "<-loopback>"),
                ("force-webrtc-ip-handling-policy", "disable_non_proxied_udp"),
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
            _proxy: proxy,
            pages: VecDeque::new(),
        })
    }

    async fn open(&self, running: &Running) -> Result<(Arc<PageAgent>, Page), AgentError> {
        let page = running
            .browser
            .new_page({
                // A window per page keeps every page rendering, so any of
                // them can be screencast.
                let mut target = CreateTargetParams::new("about:blank");
                target.new_window = Some(true);
                target
            })
            .await
            .map_err(|error| AgentError::Launch(error.to_string()))?;
        let mut paused = page
            .event_listener::<EventRequestPaused>()
            .await
            .map_err(|error| AgentError::Launch(error.to_string()))?;
        let mut dialogs = page
            .event_listener::<EventJavascriptDialogOpening>()
            .await
            .map_err(|error| AgentError::Launch(error.to_string()))?;
        let target = Arc::new(ChromiumTarget::new(page.clone()).await?);
        target
            .call(
                "Fetch.enable",
                json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}),
            )
            .await?;
        let agent = Arc::new(PageAgent::new(target));
        let answerer = Arc::clone(&agent);
        let answering = page.clone();
        tokio::spawn(async move {
            while let Some(event) = dialogs.next().await {
                let kind = match event.r#type {
                    DialogType::Alert => DialogKind::Alert,
                    DialogType::Confirm => DialogKind::Confirm,
                    DialogType::Prompt => DialogKind::Prompt,
                    DialogType::Beforeunload => DialogKind::BeforeUnload,
                };
                let answer =
                    answerer.on_dialog(kind, &event.message, event.default_prompt.as_deref());
                let mut params = json!({"accept": answer.accept});
                if let Some(text) = answer.prompt_text {
                    params["promptText"] = json!(text);
                }
                if let Err(error) = answering
                    .execute(Raw::new("Page.handleJavaScriptDialog", params))
                    .await
                {
                    tracing::debug!(%error, "agent browser dialog answer was not delivered");
                }
            }
        });
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
            self.publish(None);
        }
        if state.is_none() {
            *state = Some(self.launch().await?);
        }
        let running = state.as_mut().expect("launched above");
        self.close_released(running).await;
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
        self.publish(Some(running));
        Ok(agent)
    }

    async fn open_page(&self, key: &str) -> Option<Arc<PageAgent>> {
        let state = self.state.lock().await;
        state
            .as_ref()?
            .pages
            .iter()
            .find(|(owner, ..)| owner == key)
            .map(|(_, agent, _)| Arc::clone(agent))
    }

    async fn close(&self, key: &str) -> bool {
        let mut state = self.state.lock().await;
        let Some(running) = state.as_mut() else {
            return false;
        };
        self.close_released(running).await;
        self.publish(Some(running));
        let Some(index) = running.pages.iter().position(|(owner, ..)| owner == key) else {
            return false;
        };
        if let Some((_, _, page)) = running.pages.remove(index) {
            let _ = page.close().await;
        }
        self.publish(Some(running));
        true
    }

    fn release(&self, key: &str) {
        let Ok(mut state) = self.state.try_lock() else {
            self.released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(key.to_owned());
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
        self.publish(Some(running));
    }
}

/// A screencast of one agent page. Frames stop when it is dropped or the
/// page closes.
pub struct Screencast {
    agent: Arc<PageAgent>,
    frames: watch::Receiver<Option<image::RgbaImage>>,
    page: Page,
    runtime: tokio::runtime::Handle,
    task: tokio::task::JoinHandle<()>,
}

impl Screencast {
    /// The observed page, for its [`PageAgent::activity`].
    pub fn agent(&self) -> &Arc<PageAgent> {
        &self.agent
    }

    /// The latest frame, once per change.
    pub fn poll_frame(&mut self) -> Option<image::RgbaImage> {
        match self.frames.has_changed() {
            Ok(true) => self.frames.borrow_and_update().clone(),
            Ok(false) | Err(_) => None,
        }
    }

    /// Waits for the next frame; `None` once the page has closed.
    pub async fn next_frame(&mut self) -> Option<image::RgbaImage> {
        self.frames.changed().await.ok()?;
        self.frames.borrow_and_update().clone()
    }

    /// Whether the page closed, so no further frames will arrive.
    pub fn ended(&self) -> bool {
        self.frames.has_changed().is_err()
    }
}

impl Drop for Screencast {
    fn drop(&mut self) {
        self.task.abort();
        let page = self.page.clone();
        self.runtime.spawn(async move {
            let _ = page.execute(StopScreencastParams::default()).await;
        });
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.handler.abort();
    }
}

/// Loads of a page's main frame seen so far. Chromium alternates starting
/// and stopping a frame's load, so the counts agree whenever the frame is
/// idle, whichever order the separate event streams are read in.
#[derive(Debug, Clone, Copy, Default)]
struct Loads {
    started: u64,
    stopped: u64,
    within_document: u64,
}

impl Loads {
    fn idle(&self) -> bool {
        self.started <= self.stopped
    }
}

/// chromiumoxide's `wait_for_navigation` never sees `Page.frameStoppedLoading`,
/// so after a same-document navigation, which has no `load`, it waits
/// forever. The page's own load counts decide instead.
struct ChromiumTarget {
    page: Page,
    loads: watch::Receiver<Loads>,
}

impl ChromiumTarget {
    async fn new(page: Page) -> Result<Self, AgentError> {
        let launch = |error: chromiumoxide::error::CdpError| AgentError::Launch(error.to_string());
        let main = page
            .mainframe()
            .await
            .map_err(launch)?
            .ok_or_else(|| AgentError::Launch("the page has no main frame".into()))?;
        let mut started = page
            .event_listener::<EventFrameStartedLoading>()
            .await
            .map_err(launch)?;
        let mut stopped = page
            .event_listener::<EventFrameStoppedLoading>()
            .await
            .map_err(launch)?;
        let mut within = page
            .event_listener::<EventNavigatedWithinDocument>()
            .await
            .map_err(launch)?;
        let (counts, loads) = watch::channel(Loads::default());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(event) = started.next() => if event.frame_id == main {
                        counts.send_modify(|loads| loads.started += 1);
                    },
                    Some(event) = stopped.next() => if event.frame_id == main {
                        counts.send_modify(|loads| loads.stopped += 1);
                    },
                    Some(event) = within.next() => if event.frame_id == main {
                        counts.send_modify(|loads| loads.within_document += 1);
                    },
                    else => break,
                }
            }
        });
        Ok(Self { page, loads })
    }

    async fn wait_for_loads(&self, done: impl FnMut(&Loads) -> bool) -> Result<(), AgentError> {
        self.loads
            .clone()
            .wait_for(done)
            .await
            .map(|_| ())
            .map_err(|_| AgentError::Navigation("the page closed".into()))
    }
}

#[async_trait::async_trait]
impl CdpTarget for ChromiumTarget {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.page
            .execute(Raw::new(method, params))
            .await
            .map(|response| response.result)
            .map_err(|error| AgentError::Cdp {
                method: method.to_owned(),
                detail: error.to_string(),
            })
    }

    async fn navigate(&self, url: &str) -> Result<(), AgentError> {
        self.page
            .goto(url)
            .await
            .map(|_| ())
            .map_err(|error| AgentError::Navigation(error.to_string()))
    }

    async fn settle(&self) -> Result<(), AgentError> {
        self.wait_for_loads(Loads::idle).await
    }

    async fn go_to_history_entry(&self, entry_id: i64) -> Result<(), AgentError> {
        let before = *self.loads.borrow();
        self.call("Page.navigateToHistoryEntry", json!({"entryId": entry_id}))
            .await?;
        self.wait_for_loads(|now| {
            (now.stopped > before.stopped || now.within_document > before.within_document)
                && now.idle()
        })
        .await
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
