use super::observation::source::{self, ExecutionPage, Feed, FeedInner, Publication};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
#[path = "observation/chromium.rs"]
mod observation_source;
use std::time::Duration;

use chromiumoxide::{Browser, BrowserConfig};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::connection::{Connection, Event, SessionMessage};
use super::proxy::Proxy;
use super::tabs::Registry;
use super::{
    AgentError, CdpTarget, DialogKind, PageAgent, PageEvents, PageProvider, RequestPolicy, TabInfo,
};

const WINDOW: [u32; 2] = [1280, 800];
/// The isolated world that reports DOM changes, out of the page's reach.
const WAIT_WORLD: &str = "evorch-wait";
const DOM_BINDING: &str = "__evorchDomChanged";
/// Reports each batch of DOM changes once, through [`DOM_BINDING`].
const DOM_OBSERVER: &str = r#"(() => {
  const notify = globalThis.__evorchDomChanged;
  if (typeof notify !== "function" || globalThis.__evorchObserving) return;
  globalThis.__evorchObserving = true;
  new MutationObserver(() => notify("")).observe(document, {
    subtree: true, childList: true, attributes: true, characterData: true,
  });
})()"#;

pub struct LaunchOptions {
    /// `None` detects an installed Chromium/Chrome (honouring `CHROME`).
    pub executable: Option<PathBuf>,
    pub policy: Arc<dyn RequestPolicy>,
    /// Tabs open across all runs. Going over closes the tabs of the least
    /// recently used other run.
    pub max_pages: usize,
    /// Tabs one run may hold, at most `max_pages`; popups beyond it are
    /// closed as soon as they open.
    pub max_tabs: usize,
}

/// A headless Chromium for agents, launched on first use. Each key (a run)
/// gets its own tabs; every request of every tab passes `policy`.
pub struct AgentBrowser {
    pub execution_stamps: super::observation::ExecutionStamps,
    options: LaunchOptions,
    state: tokio::sync::Mutex<Option<Running>>,
    shared: Arc<Shared>,
}

struct Running {
    _browser: Browser,
    handler: tokio::task::JoinHandle<()>,
    gate: tokio::task::JoinHandle<()>,
    connection: Arc<Connection>,
    _profile: tempfile::TempDir,
    _proxy: Proxy,
}

impl Running {
    fn finished(&self) -> bool {
        self.handler.is_finished() || self.gate.is_finished()
    }
}

struct Shared {
    policy: Arc<dyn RequestPolicy>,
    limits: (usize, usize),
    registry: Mutex<Registry<Arc<TabPage>>>,
    connection: Mutex<Option<Arc<Connection>>>,
    pages: watch::Sender<Vec<String>>,
    changes: watch::Sender<u64>,
    front: Arc<Mutex<Option<String>>>,
    generation: AtomicU64,
    next_page: AtomicU64,
    creation: Mutex<Option<Arc<Creation>>>,
    targets: Mutex<HashMap<String, Weak<TabPage>>>,
    producers: Mutex<HashMap<String, Weak<FeedInner>>>,
}
struct Creation {
    stamp: Option<Arc<super::observation::ExecutionStamp>>,
    browser_generation: u64,
    page_generation: u64,
}

struct TabPage {
    target: String,
    session: String,
    agent: Arc<PageAgent>,
    connection: Arc<Connection>,
    ready: watch::Sender<Option<Result<(), String>>>,
    creation: Option<Arc<Creation>>,
    id: String,
    metadata: Mutex<(String, String)>,
    streaming: Arc<tokio::sync::Mutex<()>>,
}

impl TabPage {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.agent.target().call(method, params).await
    }

    fn raw(
        &self,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value, AgentError>> + Send + 'static {
        self.connection.call(Some(&self.session), method, params)
    }
}

impl AgentBrowser {
    pub fn new(options: LaunchOptions) -> Self {
        let limits = (options.max_pages, options.max_tabs);
        let shared = Arc::new(Shared {
            policy: Arc::clone(&options.policy),
            limits,
            registry: Mutex::new(Registry::new(limits.0, limits.1)),
            connection: Mutex::default(),
            pages: watch::Sender::new(Vec::new()),
            changes: watch::Sender::new(0),
            front: Arc::default(),
            generation: AtomicU64::new(1),
            next_page: AtomicU64::new(1),
            creation: Mutex::default(),
            targets: Mutex::default(),
            producers: Mutex::default(),
        });
        Self {
            execution_stamps: Default::default(),
            options,
            state: tokio::sync::Mutex::new(None),
            shared,
        }
    }

    /// Keys with an open tab, sorted; it changes whenever a run's first tab
    /// opens or its last one closes.
    pub fn pages(&self) -> watch::Receiver<Vec<String>> {
        self.shared.pages.subscribe()
    }

    /// Starts a screencast of `key`'s active tab that moves to whichever tab
    /// becomes active. `on_frame` runs after each frame and once more when
    /// the run's last tab closes, e.g. to request a repaint.
    pub async fn screencast(
        &self,
        key: &str,
        on_frame: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Screencast, AgentError> {
        let feed = self.shared.feed(key, None)?;
        let mut updates = feed.updates.clone();
        let task = tokio::spawn(async move {
            while updates.changed().await.is_ok() {
                on_frame();
                if updates.borrow().is_none() {
                    break;
                }
            }
            on_frame();
        });
        let (seen_tab, page) = self
            .shared
            .active(key)
            .ok_or_else(|| AgentError::NoPage(key.to_owned()))?;
        Ok(Screencast {
            feed,
            shared: Arc::clone(&self.shared),
            key: key.to_owned(),
            seen_tab,
            last_agent: page.agent.clone(),
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

    async fn running<'a>(&self, state: &'a mut Option<Running>) -> Result<&'a Running, AgentError> {
        if state.as_ref().is_some_and(Running::finished) {
            *state = None;
            self.shared.reset();
        }
        if state.is_none() {
            *state = Some(self.launch().await?);
        }
        Ok(state.as_ref().expect("launched above"))
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
            // chromiumoxide also attaches to every tab and would otherwise
            // emulate its own viewport and touch input on it at a moment of
            // its choosing, and accept invalid certificates.
            .viewport(None)
            .respect_https_errors()
            .launch_timeout(Duration::from_secs(20))
            .request_timeout(super::REQUEST_TIMEOUT)
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
        let shared = Arc::clone(&self.shared);
        let (connection, gate) =
            Connection::open(browser.websocket_address(), move |connection, event| {
                shared.on_browser_event(connection, event);
            })
            .await?;
        *lock(&self.shared.connection) = Some(Arc::clone(&connection));
        let running = Running {
            _browser: browser,
            handler,
            gate,
            connection,
            _profile: profile,
            _proxy: proxy,
        };
        // Every page, including popups, attaches here paused, so Fetch is
        // on before its first request.
        running
            .connection
            .call(
                None,
                "Target.setAutoAttach",
                json!({
                    "autoAttach": true,
                    "waitForDebuggerOnStart": true,
                    "flatten": true,
                    "filter": [{"type": "page", "exclude": false}],
                }),
            )
            .await?;
        running
            .connection
            .call(None, "Target.setDiscoverTargets", json!({"discover": true}))
            .await?;
        self.shared.edit(Registry::start);
        Ok(running)
    }
}

impl Shared {
    fn edit<R>(&self, change: impl FnOnce(&mut Registry<Arc<TabPage>>) -> R) -> R {
        let (result, keys) = {
            let mut registry = lock(&self.registry);
            let result = change(&mut registry);
            (result, registry.keys())
        };
        self.pages.send_if_modified(|current| {
            let changed = *current != keys;
            *current = keys;
            changed
        });
        self.changes.send_modify(|generation| *generation += 1);
        result
    }

    fn connection(&self) -> Option<Arc<Connection>> {
        lock(&self.connection).clone()
    }

    fn active(&self, key: &str) -> Option<(String, Arc<TabPage>)> {
        lock(&self.registry).active(key)
    }

    fn reset(&self) {
        *lock(&self.connection) = None;
        self.generation.fetch_add(1, Ordering::Relaxed);
        lock(&self.targets).clear();
        let (max_pages, max_tabs) = self.limits;
        self.edit(|registry| *registry = Registry::new(max_pages, max_tabs));
    }

    fn close_targets(&self, targets: Vec<String>) {
        let Some(connection) = self.connection() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        for target in targets {
            let connection = Arc::clone(&connection);
            runtime.spawn(async move { close_target(&connection, &target).await });
        }
    }

    fn on_browser_event(&self, connection: &Arc<Connection>, event: Option<Event>) {
        let Some(event) = event else {
            let current = lock(&self.connection)
                .as_ref()
                .is_some_and(|open| Arc::ptr_eq(open, connection));
            if current {
                self.reset();
            }
            return;
        };
        if !self
            .connection()
            .is_some_and(|current| Arc::ptr_eq(&current, connection))
        {
            return;
        }
        match event.method.as_str() {
            "Target.targetInfoChanged" => {
                let info = &event.params["targetInfo"];
                if let Some(page) = info["targetId"]
                    .as_str()
                    .and_then(|id| lock(&self.targets).get(id).and_then(Weak::upgrade))
                {
                    *lock(&page.metadata) = (
                        source::bounded(info["title"].as_str().unwrap_or_default()),
                        source::bounded(info["url"].as_str().unwrap_or_default()),
                    );
                    self.changes.send_modify(|revision| *revision += 1);
                }
            }
            "Target.attachedToTarget" => self.attached(connection, &event.params),
            "Target.detachedFromTarget" => {
                *lock(&self.front) = None;
                if let Some(session) = event.params["sessionId"].as_str() {
                    connection.forget(session);
                }
                if let Some(target) = event.params["targetId"].as_str() {
                    self.edit(|registry| registry.detached(target));
                }
            }
            _ => {}
        }
    }

    /// Takes a target that attached paused. Its events are routed before the
    /// next message is read; Fetch is enabled before it may load.
    fn attached(&self, connection: &Arc<Connection>, params: &Value) {
        let info = &params["targetInfo"];
        let (Some(session), Some(target)) =
            (params["sessionId"].as_str(), info["targetId"].as_str())
        else {
            return;
        };
        // Capture immutable input before either page task is spawned. Popups
        // inherit their opener, never the invocation of a later run incarnation.
        let creation = match info["openerId"].as_str() {
            Some(opener) => lock(&self.targets)
                .get(opener)
                .and_then(Weak::upgrade)
                .and_then(|p| p.creation.clone()),
            None => lock(&self.creation).clone(),
        };
        let events = connection.subscribe(session);
        let (counts, loads) = watch::channel(Loads::default());
        let (activity, page_events) = watch::channel(PageEvents::default());
        let (ready, ready_rx) = watch::channel(None);
        // A new window opens in front of the others.
        *lock(&self.front) = Some(target.to_owned());
        let cdp = Arc::new(SessionTarget {
            connection: Arc::clone(connection),
            session: session.to_owned(),
            target: target.to_owned(),
            front: Arc::clone(&self.front),
            loads,
            ready: ready_rx,
            page_events,
            observing: tokio::sync::OnceCell::new(),
        });
        let page = Arc::new(TabPage {
            target: target.to_owned(),
            session: session.to_owned(),
            agent: Arc::new(PageAgent::new(cdp)),
            connection: Arc::clone(connection),
            ready,
            creation,
            id: format!("{:032x}", self.next_page.fetch_add(1, Ordering::Relaxed)),
            metadata: Mutex::new((
                source::bounded(info["title"].as_str().unwrap_or_default()),
                source::bounded(info["url"].as_str().unwrap_or_default()),
            )),
            streaming: Arc::new(tokio::sync::Mutex::new(())),
        });
        lock(&self.targets).retain(|_, page| page.strong_count() > 0);
        lock(&self.targets).insert(target.to_owned(), Arc::downgrade(&page));
        let attached = self
            .edit(|registry| registry.attach(target, info["openerId"].as_str(), Arc::clone(&page)));
        self.close_targets(attached.evicted);
        tokio::spawn(pump(
            events,
            counts,
            activity,
            Arc::clone(&page),
            Arc::clone(&self.policy),
            self.changes.clone(),
        ));
        tokio::spawn(prepare(page, attached.keep));
    }
}

async fn close_target(connection: &Connection, target: &str) {
    if let Err(error) = connection
        .call(None, "Target.closeTarget", json!({"targetId": target}))
        .await
    {
        tracing::debug!(%error, "agent browser tab did not close");
    }
}

/// Lets an attached page run once Fetch guards it. A page refused a place
/// is closed right after: a popup that shares its opener's renderer holds
/// that renderer until it is resumed.
async fn prepare(page: Arc<TabPage>, keep: bool) {
    let guarded = page.raw(
        "Fetch.enable",
        json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}),
    );
    // A noopener popup has no renderer until its navigation commits, and a
    // command for the renderer holds back the session's later ones, so
    // Page.enable follows the resume.
    let outcome = match guarded.await {
        Ok(_) => {
            let resumed = page.raw("Runtime.runIfWaitingForDebugger", json!({}));
            let enabled = page.raw("Page.enable", json!({}));
            // Only in-flight counts are needed, so no response body is kept.
            let network = page.raw(
                "Network.enable",
                json!({"maxTotalBufferSize": 0, "maxResourceBufferSize": 0}),
            );
            match (resumed.await, keep) {
                (Ok(_), false) => Err("the tab was closed: its run is at its tab limit".to_owned()),
                (Ok(_), true) => match (enabled.await, network.await) {
                    (Ok(_), Ok(_)) => Ok(()),
                    (Err(error), _) | (_, Err(error)) => Err(error.to_string()),
                },
                (Err(error), _) => Err(error.to_string()),
            }
        }
        Err(error) => Err(error.to_string()),
    };
    if let Err(error) = &outcome {
        if keep {
            tracing::warn!(%error, "agent browser tab could not be guarded; closing it");
        }
        close_target(&page.connection, &page.target).await;
    }
    page.ready.send_replace(Some(outcome));
}

async fn pump(
    mut events: mpsc::UnboundedReceiver<SessionMessage>,
    counts: watch::Sender<Loads>,
    activity: watch::Sender<PageEvents>,
    page: Arc<TabPage>,
    policy: Arc<dyn RequestPolicy>,
    changes: watch::Sender<u64>,
) {
    // In-flight request ids, with the loader of the document that sent each.
    let mut requests: HashMap<String, String> = HashMap::new();
    while let Some(message) = events.recv().await {
        let Some(event) = message.into_event() else {
            continue;
        };
        if event.method == "Target.targetInfoChanged"
            && event.params["targetInfo"]["targetId"].as_str() == Some(page.target.as_str())
        {
            let info = &event.params["targetInfo"];
            *lock(&page.metadata) = (
                source::bounded(info["title"].as_str().unwrap_or_default()),
                source::bounded(info["url"].as_str().unwrap_or_default()),
            );
            changes.send_modify(|revision| *revision += 1);
        }
        let main_frame = event.params["frameId"].as_str() == Some(page.target.as_str());
        // Headless Chromium can omit the final title notification. This
        // browser-level reply is not queued behind the session's own pump.
        let loaded = matches!(
            event.method.as_str(),
            "Page.domContentEventFired" | "Page.loadEventFired"
        ) || (event.method == "Page.frameStoppedLoading" && main_frame);
        if loaded
            && let Ok(reply) = page
                .connection
                .call(
                    None,
                    "Target.getTargetInfo",
                    json!({"targetId":page.target}),
                )
                .await
            && let Some(info) = reply
                .get("targetInfo")
                .filter(|info| info["targetId"].as_str() == Some(page.target.as_str()))
        {
            *lock(&page.metadata) = (
                source::bounded(info["title"].as_str().unwrap_or_default()),
                source::bounded(info["url"].as_str().unwrap_or_default()),
            );
            changes.send_modify(|revision| *revision += 1);
        }
        let request = || event.params["requestId"].as_str().map(str::to_owned);
        let counted = match event.method.as_str() {
            "Page.frameStartedLoading" if main_frame => {
                counts.send_modify(|loads| loads.loading = true);
                true
            }
            "Page.frameStoppedLoading" if main_frame => {
                counts.send_modify(|loads| loads.loading = false);
                true
            }
            "Page.frameNavigated"
                if event.params["frame"]["id"].as_str() == Some(page.target.as_str()) =>
            {
                let loader = event.params["frame"]["loaderId"].as_str();
                // Requests of the replaced document may never report an end.
                requests.retain(|_, sent_by| Some(sent_by.as_str()) == loader);
                counts.send_modify(|loads| {
                    loads.navigated += 1;
                    loads.loader = loader.map(str::to_owned);
                });
                true
            }
            "Page.navigatedWithinDocument" if main_frame => {
                counts.send_modify(|loads| loads.within_document += 1);
                true
            }
            "Network.requestWillBeSent" => {
                if let Some(id) = request() {
                    let loader = event.params["loaderId"].as_str().unwrap_or_default();
                    requests.insert(id, loader.to_owned());
                }
                true
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                if let Some(id) = request() {
                    requests.remove(&id);
                }
                true
            }
            "Runtime.bindingCalled" => event.params["name"].as_str() == Some(DOM_BINDING),
            _ => false,
        };
        if counted {
            activity.send_modify(|seen| {
                seen.count += 1;
                seen.requests = requests.len();
            });
        }
        match event.method.as_str() {
            "Page.javascriptDialogOpening" => {
                tokio::spawn(answer_dialog(Arc::clone(&page), event.params));
            }
            "Fetch.requestPaused" => {
                tokio::spawn(decide(Arc::clone(&page), event.params, Arc::clone(&policy)));
            }
            _ => {}
        }
    }
}

async fn answer_dialog(page: Arc<TabPage>, params: Value) {
    let kind = match params["type"].as_str() {
        Some("confirm") => DialogKind::Confirm,
        Some("prompt") => DialogKind::Prompt,
        Some("beforeunload") => DialogKind::BeforeUnload,
        _ => DialogKind::Alert,
    };
    let answer = page.agent.on_dialog(
        kind,
        params["message"].as_str().unwrap_or_default(),
        params["defaultPrompt"].as_str(),
    );
    let mut reply = json!({"accept": answer.accept});
    if let Some(text) = answer.prompt_text {
        reply["promptText"] = json!(text);
    }
    if let Err(error) = page.raw("Page.handleJavaScriptDialog", reply).await {
        tracing::debug!(%error, "agent browser dialog answer was not delivered");
    }
}

async fn decide(page: Arc<TabPage>, params: Value, policy: Arc<dyn RequestPolicy>) {
    let request_id = params["requestId"].clone();
    let url = params["request"]["url"].as_str().unwrap_or_default();
    let verdict = match url::Url::parse(url) {
        Ok(url) => policy.check(&url).await,
        Err(error) => Err(error.to_string()),
    };
    let (method, params) = match verdict {
        Ok(()) => ("Fetch.continueRequest", json!({"requestId": request_id})),
        Err(reason) => {
            page.agent.record_blocked(format!("{url} ({reason})"));
            (
                "Fetch.failRequest",
                json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
            )
        }
    };
    if let Err(error) = page.raw(method, params).await {
        tracing::debug!(%error, method, "agent browser request decision was not delivered");
    }
}

#[async_trait::async_trait]
impl PageProvider for AgentBrowser {
    async fn page(&self, key: &str) -> Result<Arc<PageAgent>, AgentError> {
        let stamp = self.execution_stamps.for_creation(key);
        let mut state = self.state.lock().await;
        let running = self.running(&mut state).await?;
        if let Some((_, tab)) = self.shared.edit(|registry| {
            registry.touch(key);
            registry.active(key)
        }) {
            return Ok(Arc::clone(&tab.agent));
        }
        let creation = Arc::new(Creation {
            stamp,
            browser_generation: self.shared.generation.load(Ordering::Relaxed),
            page_generation: self.shared.next_page.fetch_add(1, Ordering::Relaxed),
        });
        *lock(&self.shared.creation) = Some(creation);
        let _creation = PendingCreation(&self.shared);
        let mut changes = self.shared.changes.subscribe();
        self.shared.edit(|registry| registry.creating(1));
        let created = running
            .connection
            .call(
                None,
                "Target.createTarget",
                // A window per tab keeps every tab rendering, so any of
                // them can be screencast.
                json!({"url": "about:blank", "newWindow": true}),
            )
            .await
            .and_then(|created| {
                created["targetId"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| AgentError::Response {
                        method: "Target.createTarget".into(),
                        detail: "missing targetId".into(),
                    })
            });
        let evicted = self.shared.edit(|registry| {
            registry.creating(-1);
            created
                .as_ref()
                .map(|target| registry.claim(key, target))
                .unwrap_or_default()
        });
        let target = match created {
            Ok(target) => target,
            Err(error) => {
                lock(&self.shared.creation).take();
                return Err(AgentError::Launch(error.to_string()));
            }
        };
        self.shared.close_targets(evicted);
        let _ = changes
            .wait_for(|_| {
                let registry = lock(&self.shared.registry);
                registry.active(key).is_some() || !registry.claimed(&target)
            })
            .await;
        lock(&self.shared.creation).take();
        self.shared
            .active(key)
            .map(|(_, tab)| Arc::clone(&tab.agent))
            .ok_or_else(|| AgentError::Launch("the new tab closed before it opened".into()))
    }

    async fn open_page(&self, key: &str) -> Option<Arc<PageAgent>> {
        self.shared
            .active(key)
            .map(|(_, tab)| Arc::clone(&tab.agent))
    }

    async fn close(&self, key: &str) -> bool {
        let targets = self.shared.edit(|registry| registry.remove_run(key));
        let Some(connection) = self.shared.connection() else {
            return !targets.is_empty();
        };
        for target in &targets {
            close_target(&connection, target).await;
        }
        !targets.is_empty()
    }

    fn release(&self, key: &str) {
        let targets = self.shared.edit(|registry| registry.remove_run(key));
        self.shared.close_targets(targets);
    }

    async fn tabs(&self, key: &str) -> Result<Vec<TabInfo>, AgentError> {
        let listed = lock(&self.shared.registry).tabs(key);
        let Some(connection) = self.shared.connection() else {
            return Ok(Vec::new());
        };
        if listed.is_empty() {
            return Ok(Vec::new());
        }
        let targets = connection
            .call(None, "Target.getTargets", json!({}))
            .await?;
        let infos = targets["targetInfos"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(listed
            .into_iter()
            .map(|tab| {
                let info = infos
                    .iter()
                    .find(|info| info["targetId"].as_str() == Some(tab.target.as_str()));
                let field = |name: &str| {
                    info.and_then(|info| info[name].as_str())
                        .unwrap_or_default()
                        .to_owned()
                };
                TabInfo {
                    url: field("url"),
                    title: field("title"),
                    id: tab.id,
                    active: tab.active,
                    opener: tab.opener,
                }
            })
            .collect())
    }

    async fn switch_tab(&self, key: &str, id: &str) -> Result<Arc<PageAgent>, AgentError> {
        self.shared
            .edit(|registry| registry.switch(key, id))
            .map(|tab| Arc::clone(&tab.agent))
            .map_err(AgentError::Refused)
    }

    async fn close_tab(&self, key: &str, id: &str) -> Result<(), AgentError> {
        let target = self
            .shared
            .edit(|registry| registry.close_tab(key, id))
            .map_err(AgentError::Refused)?;
        if let Some(connection) = self.shared.connection() {
            close_target(&connection, &target).await;
        }
        Ok(())
    }

    fn tab_news(&self, key: &str) -> Vec<String> {
        lock(&self.shared.registry).take_news(key)
    }
}

/// A screencast of a run's active tab. Frames stop when it is dropped or the
/// run's last tab closes.
pub struct Screencast {
    feed: Feed,
    shared: Arc<Shared>,
    key: String,
    seen_tab: String,
    last_agent: Arc<PageAgent>,
    task: tokio::task::JoinHandle<()>,
}
impl Screencast {
    pub fn agent(&self) -> Arc<PageAgent> {
        self.shared
            .active(&self.key)
            .map(|(_, tab)| tab.agent.clone())
            .unwrap_or_else(|| self.last_agent.clone())
    }
    pub fn tab(&self) -> String {
        self.shared
            .active(&self.key)
            .map(|(id, _)| id)
            .unwrap_or_default()
    }
    pub async fn next_tab(&mut self) -> Option<String> {
        loop {
            let (now, _) = self.shared.active(&self.key)?;
            if now != self.seen_tab {
                self.seen_tab = now.clone();
                return Some(now);
            }
            self.feed.updates.changed().await.ok()?;
            self.feed.updates.borrow().as_ref()?;
        }
    }
    pub fn poll_frame(&mut self) -> Option<image::RgbaImage> {
        if !self.feed.updates.has_changed().unwrap_or(false) {
            return None;
        }
        let page = self.feed.updates.borrow_and_update();
        let frame = page.as_ref()?.frame.as_ref()?;
        if self.shared.active(&self.key)?.1.id != frame.tab {
            return None;
        }
        image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.to_vec())
    }
    pub async fn next_frame(&mut self) -> Option<image::RgbaImage> {
        loop {
            self.feed.updates.changed().await.ok()?;
            if self.feed.updates.borrow().is_none() {
                return None;
            }
            // changed() marks the version seen; read the coherent record directly.
            let page = self.feed.updates.borrow_and_update();
            if let Some(frame) = page.as_ref().and_then(|page| page.frame.as_ref()) {
                if self.shared.active(&self.key)?.1.id != frame.tab {
                    continue;
                }
                return image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.to_vec());
            }
        }
    }
    pub fn ended(&self) -> bool {
        self.shared.active(&self.key).is_none()
            || self.feed.updates.borrow().is_none()
            || self.feed.updates.has_changed().is_err()
    }
}
impl Drop for Screencast {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.handler.abort();
        self.gate.abort();
    }
}

/// The main frame's loading state, read from the session's ordered events.
/// A tab's first load may start before its events are enabled, so only the
/// latest start or stop is trusted, not counts of them.
#[derive(Debug, Clone, Default)]
struct Loads {
    loading: bool,
    /// The loader of the latest main-frame commit.
    loader: Option<String>,
    navigated: u64,
    within_document: u64,
}

impl Loads {
    fn idle(&self) -> bool {
        !self.loading
    }
}

/// A page's flattened session on the gate connection. Commands wait until
/// the session is guarded and resumed; navigation waits on the main frame's
/// load counts.
struct SessionTarget {
    connection: Arc<Connection>,
    session: String,
    target: String,
    /// The tab whose window is in front, if known. Only that window draws
    /// frames, and input and screenshots wait for a frame, so other tabs
    /// come to the front first.
    front: Arc<Mutex<Option<String>>>,
    loads: watch::Receiver<Loads>,
    ready: watch::Receiver<Option<Result<(), String>>>,
    page_events: watch::Receiver<PageEvents>,
    /// Set once DOM changes of this and later documents are reported.
    observing: tokio::sync::OnceCell<()>,
}

impl SessionTarget {
    /// Observes the DOM from an isolated world, so the page can neither see
    /// the observer nor report changes itself. Bindings report calls only
    /// while the Runtime domain is enabled.
    async fn observe_dom(&self) -> Result<(), AgentError> {
        self.call("Runtime.enable", json!({})).await?;
        self.call(
            "Runtime.addBinding",
            json!({"name": DOM_BINDING, "executionContextName": WAIT_WORLD}),
        )
        .await?;
        self.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": DOM_OBSERVER, "worldName": WAIT_WORLD}),
        )
        .await?;
        let world = self
            .call(
                "Page.createIsolatedWorld",
                json!({"frameId": self.target, "worldName": WAIT_WORLD}),
            )
            .await?;
        self.call(
            "Runtime.evaluate",
            json!({"expression": DOM_OBSERVER, "contextId": world["executionContextId"]}),
        )
        .await?;
        Ok(())
    }

    async fn ready(&self) -> Result<(), AgentError> {
        let mut ready = self.ready.clone();
        let state = ready
            .wait_for(Option::is_some)
            .await
            .map_err(|_| AgentError::Navigation("the page closed".into()))?;
        match state.as_ref() {
            Some(Err(error)) => Err(AgentError::Navigation(error.clone())),
            _ => Ok(()),
        }
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
impl CdpTarget for SessionTarget {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.ready().await?;
        let needs_frames = method.starts_with("Input.")
            || matches!(method, "Page.captureScreenshot" | "Page.startScreencast");
        if needs_frames && lock(&self.front).as_deref() != Some(self.target.as_str()) {
            self.connection
                .call(Some(&self.session), "Page.bringToFront", json!({}))
                .await?;
            *lock(&self.front) = Some(self.target.clone());
        }
        self.connection
            .call(Some(&self.session), method, params)
            .await
    }

    async fn navigate(&self, url: &str) -> Result<(), AgentError> {
        let navigated = self
            .call("Page.navigate", json!({"url": url}))
            .await
            .map_err(|error| AgentError::Navigation(error.to_string()))?;
        if let Some(error) = navigated["errorText"]
            .as_str()
            .filter(|error| !error.is_empty())
        {
            return Err(AgentError::Navigation(error.to_owned()));
        }
        let Some(loader) = navigated["loaderId"].as_str() else {
            return self.settle().await;
        };
        self.wait_for_loads(|now| now.loader.as_deref() == Some(loader) && now.idle())
            .await
    }

    async fn settle(&self) -> Result<(), AgentError> {
        self.wait_for_loads(Loads::idle).await
    }

    async fn page_events(&self) -> Result<Option<watch::Receiver<PageEvents>>, AgentError> {
        self.observing
            .get_or_try_init(|| self.observe_dom())
            .await?;
        Ok(Some(self.page_events.clone()))
    }

    async fn go_to_history_entry(&self, entry_id: i64) -> Result<(), AgentError> {
        let before = self.loads.borrow().clone();
        self.call("Page.navigateToHistoryEntry", json!({"entryId": entry_id}))
            .await?;
        self.wait_for_loads(|now| {
            (now.navigated > before.navigated || now.within_document > before.within_document)
                && now.idle()
        })
        .await
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct PendingCreation<'a>(&'a Shared);
impl Drop for PendingCreation<'_> {
    fn drop(&mut self) {
        lock(&self.0.creation).take();
    }
}
