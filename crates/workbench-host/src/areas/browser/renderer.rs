//! The core's single renderer owner. No transport DTO, catalog or CEF dependency.
//! All waiting runs in an uncounted AreaTask; synchronous handlers only enqueue.
use super::{
    budget::{RendererBudget, TabBudget},
    profiles::Profile,
};
use crate::areas::{AreaTasks, SpawnError, StopSignal};
use renderer_ipc::{
    ClientConfig, ClientError, OpenArtifact, RendererClient, RendererGeneration, Reply, Request,
    TabId, Viewport,
};
pub mod data;
pub use sandbox::SpawnLedger;
use sandbox::{
    ChildKind,
    process::{GroupChild, apply_launch_policy},
};
use std::{
    path::PathBuf,
    sync::{Arc, mpsc as blocking},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OwnerError {
    #[error("renderer resource budget is full")]
    Busy,
    #[error("renderer is unavailable")]
    Unavailable,
    #[error("renderer generation is no longer valid")]
    StaleGeneration,
    #[error("invalid renderer request")]
    Invalid,
    #[error("renderer is stopping")]
    Stopping,
    #[error("renderer request timed out; execution is uncertain")]
    TimedOut,
    #[error("renderer refused the request: {0:?}")]
    Remote(renderer_ipc::ErrorKind),
    #[error("renderer IPC failed: {0}")]
    Ipc(String),
}
impl From<ClientError> for OwnerError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Timeout => Self::TimedOut,
            ClientError::Remote(error) => Self::Remote(error.kind),
            ClientError::Local(_) => Self::Invalid,
            ClientError::Closed(reason) => Self::Ipc(format!("{reason:?}")),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    Starting,
    Ready,
    Idle,
    Stopping,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub phase: Phase,
    pub generation: u64,
    pub tabs: usize,
}
#[derive(Clone)]
pub struct OwnerConfig {
    /// The memory budget of the raw frames of every helper generation of this core.
    pub frame_pool: Arc<renderer_ipc::FramePool>,
    /// Trusted distribution path, never supplied by artifact/client input.
    pub executable: PathBuf,
    pub profiles: PathBuf,
    pub idle: Duration,
    pub hello_timeout: Duration,
    #[cfg(feature = "test-support")]
    pub remove_profile: Option<fn(&std::path::Path) -> std::io::Result<()>>,
    #[cfg(feature = "test-support")]
    pub fail_after_readers: Option<usize>,
    #[cfg(feature = "test-support")]
    pub spawned: Option<blocking::Sender<i32>>,
}
impl OwnerConfig {
    /// Configures the installed helper without starting or probing it. Missing
    /// helpers remain an execution-time error, never a tool-catalog condition.
    pub fn installed(profiles: PathBuf) -> std::io::Result<Self> {
        Ok(Self::new(Self::installed_executable()?, profiles))
    }
    /// Resolve a trusted startup override, otherwise the colocated executable.
    /// Nix wrappers supply the absolute libexec path as a default, preserving an
    /// explicit user override. This performs no filesystem writes or bootstrap.
    pub fn installed_executable() -> std::io::Result<PathBuf> {
        resolve_installed_executable(
            std::env::var_os("EVORCH_RENDERER_EXEC"),
            std::env::current_exe,
        )
    }
    pub fn new(executable: PathBuf, profiles: PathBuf) -> Self {
        Self {
            frame_pool: renderer_ipc::FramePool::new(renderer_ipc::FRAME_BUDGET_BYTES),
            executable,
            profiles,
            idle: Duration::from_secs(30),
            hello_timeout: Duration::from_secs(60),
            #[cfg(feature = "test-support")]
            remove_profile: None,
            #[cfg(feature = "test-support")]
            fail_after_readers: None,
            #[cfg(feature = "test-support")]
            spawned: None,
        }
    }
}
fn resolve_installed_executable(
    configured: Option<std::ffi::OsString>,
    current_exe: impl FnOnce() -> std::io::Result<PathBuf>,
) -> std::io::Result<PathBuf> {
    if let Some(configured) = configured {
        let path = PathBuf::from(configured);
        if !path.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "EVORCH_RENDERER_EXEC must be a nonempty absolute executable path",
            ));
        }
        return Ok(path);
    }
    Ok(current_exe()?.with_file_name("evorch-renderer"))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tab {
    pub generation: u64,
    pub id: TabId,
}
enum Action {
    Open(OpenArtifact, Vec<u8>),
    Resize(Tab, Viewport),
    Streaming(Tab, bool),
    FrameRate(Tab, u8),
    PageRequest(Tab, Request),
    Close(Tab),
    Stop,
}
enum Answer {
    Open(Tab),
    PageReply(Reply, Vec<u8>),
    Done,
}
struct Envelope {
    _body_budget: Option<tokio::sync::OwnedSemaphorePermit>,
    action: Action,
    reply: oneshot::Sender<Result<Answer, OwnerError>>,
}
/// Only the actor adds/removes live tabs, so pending cleanup cannot outgrow the
/// tab budget. Drop callbacks do not need a command or AreaTasks queue slot.
#[derive(Default)]
struct TabCleanup {
    live: std::sync::Mutex<Vec<(Tab, bool)>>,
    requested: tokio::sync::Notify,
    finished: tokio::sync::Notify,
}
impl TabCleanup {
    fn opened(&self, tab: Tab) {
        let mut live = self.live.lock().unwrap();
        debug_assert!(live.len() < renderer_ipc::MAX_TABS);
        live.push((tab, false));
    }
    fn request(&self, tab: Tab) {
        let mut live = self.live.lock().unwrap();
        if let Some((_, pending)) = live.iter_mut().find(|(candidate, _)| *candidate == tab) {
            *pending = true;
            self.requested.notify_one();
        }
    }
    fn take(&self) -> Option<Tab> {
        self.live
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(_, pending)| *pending)
            .map(|(tab, pending)| {
                *pending = false;
                *tab
            })
    }
    fn contains(&self, tab: Tab) -> bool {
        self.live
            .lock()
            .unwrap()
            .iter()
            .any(|(candidate, _)| *candidate == tab)
    }
    fn closed(&self, tab: Tab) {
        self.live
            .lock()
            .unwrap()
            .retain(|(candidate, _)| *candidate != tab);
        self.finished.notify_waiters();
    }
    fn clear(&self) {
        self.live.lock().unwrap().clear();
        self.finished.notify_waiters();
    }
}
#[derive(Clone)]
pub struct RendererOwner {
    body_budget: Arc<tokio::sync::Semaphore>,
    commands: mpsc::Sender<Envelope>,
    status: watch::Receiver<Status>,
    cleanup: Arc<TabCleanup>,
    plane: data::DataPlane,
    frames: FrameSource,
    pub(crate) run_tabs:
        Arc<std::sync::Mutex<std::collections::HashMap<String, super::run_pages::RunTab>>>,
    /// Test-only: the status reported while the actor has published none.
    #[cfg(test)]
    test_status: Arc<std::sync::Mutex<Option<Status>>>,
}
impl RendererOwner {
    /// Installs an idle watcher only. No process, IPC reader or directory yet.
    pub fn new(
        tasks: &AreaTasks,
        ledger: SpawnLedger,
        config: OwnerConfig,
    ) -> Result<Self, SpawnError> {
        let (commands, rx) = mpsc::channel(16);
        let (status, view) = watch::channel(Status {
            phase: Phase::Stopped,
            generation: 0,
            tabs: 0,
        });
        let cleanup = Arc::new(TabCleanup::default());
        let actor_cleanup = cleanup.clone();
        let frames = FrameSource::default();
        let actor_frames = frames.clone();
        tasks.spawn("renderer-owner", move |stop| {
            Actor::new(ledger, config, status, stop, actor_cleanup, actor_frames).run(rx)
        })?;
        Ok(Self {
            body_budget: Arc::new(tokio::sync::Semaphore::new(64 * 1024 * 1024)),
            commands,
            status: view,
            cleanup,
            plane: data::DataPlane::default(),
            frames,
            run_tabs: Arc::default(),
            #[cfg(test)]
            test_status: Arc::default(),
        })
    }
    pub fn status(&self) -> Status {
        let status = *self.status.borrow();
        // The override stands in only for a helper that never ran; once the
        // actor publishes a status of its own, that one is the truth.
        #[cfg(test)]
        if status.phase == Phase::Stopped
            && status.generation == 0
            && let Some(installed) = *self.test_status.lock().unwrap()
        {
            return installed;
        }
        status
    }
    /// The data connections of this owner: channels, bindings and leases.
    pub fn data_plane(&self) -> data::DataPlane {
        self.plane.clone()
    }
    /// The connection the raw frames of `tab` arrive on, for a frame lease.
    pub(crate) fn frame_source(&self, tab: Tab) -> Option<Arc<RendererClient>> {
        self.frames.get(tab.generation)
    }
    /// Test-only: stands in for a helper that came up. Only the actor
    /// publishes a status and a frame connection, and no helper runs in the
    /// unit tests.
    #[cfg(test)]
    pub(crate) fn install_ready_helper(&self, generation: u64, client: Arc<RendererClient>) {
        self.frames.set(generation, client);
        *self.test_status.lock().unwrap() = Some(Status {
            phase: Phase::Ready,
            generation,
            tabs: 0,
        });
    }
    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }
    async fn call(&self, action: Action) -> Result<Answer, OwnerError> {
        let body_budget = if let Action::Open(_, body) = &action {
            let bytes = u32::try_from(body.len()).map_err(|_| OwnerError::Busy)?;
            Some(
                self.body_budget
                    .clone()
                    .try_acquire_many_owned(bytes)
                    .map_err(|_| OwnerError::Busy)?,
            )
        } else {
            None
        };
        let (reply, rx) = oneshot::channel();
        self.commands
            .try_send(Envelope {
                action,
                reply,
                _body_budget: body_budget,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => OwnerError::Busy,
                _ => OwnerError::Stopping,
            })?;
        rx.await.unwrap_or(Err(OwnerError::Stopping))
    }
    pub async fn open(&self, open: OpenArtifact, body: Vec<u8>) -> Result<Tab, OwnerError> {
        // Validate before enqueueing potentially large data.
        Request::OpenArtifact(open.clone())
            .validate(body.len())
            .map_err(|_| OwnerError::Invalid)?;
        match self.call(Action::Open(open, body)).await? {
            Answer::Open(tab) => Ok(tab),
            _ => Err(OwnerError::Unavailable),
        }
    }
    pub async fn close(&self, tab: Tab) -> Result<(), OwnerError> {
        self.call(Action::Close(tab)).await.map(|_| ())
    }
    pub async fn resize(&self, tab: Tab, viewport: Viewport) -> Result<(), OwnerError> {
        self.call(Action::Resize(tab, viewport)).await.map(|_| ())
    }
    pub async fn set_streaming(&self, tab: Tab, enabled: bool) -> Result<(), OwnerError> {
        self.call(Action::Streaming(tab, enabled)).await.map(|_| ())
    }
    pub async fn set_frame_rate(&self, tab: Tab, fps: u8) -> Result<(), OwnerError> {
        self.call(Action::FrameRate(tab, fps)).await.map(|_| ())
    }
    pub async fn stop(&self) -> Result<(), OwnerError> {
        self.call(Action::Stop).await.map(|_| ())
    }
    /// The already-owned actor reclaims the tab, even when both work queues are full.
    pub(crate) fn close_later(&self, tab: Tab) {
        self.cleanup.request(tab);
    }
    /// Cancellation only drops the wait; it cannot withdraw cleanup ownership.
    pub(crate) async fn close_wait(&self, tab: Tab) -> Result<(), OwnerError> {
        self.close_later(tab);
        let mut status = self.subscribe();
        loop {
            let finished = self.cleanup.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            if !self.cleanup.contains(tab) {
                return Ok(());
            }
            if self.commands.is_closed() {
                return Err(OwnerError::Stopping);
            }
            tokio::select! {
                _ = &mut finished => {}
                changed = status.changed() => {
                    if changed.is_err() {
                        return Err(OwnerError::Stopping);
                    }
                }
            }
        }
    }
    async fn page_request(
        &self,
        tab: Tab,
        request: Request,
    ) -> Result<(Reply, Vec<u8>), OwnerError> {
        request.validate(0).map_err(|_| OwnerError::Invalid)?;
        match self.call(Action::PageRequest(tab, request)).await? {
            Answer::PageReply(reply, body) => Ok((reply, body)),
            _ => Err(OwnerError::Unavailable),
        }
    }
    pub async fn cdp(
        &self,
        tab: Tab,
        method: String,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, OwnerError> {
        let (reply, _) = self
            .page_request(
                tab,
                Request::Cdp {
                    tab: tab.id,
                    method,
                    params,
                    timeout_ms: 30_000,
                },
            )
            .await?;
        match reply {
            Reply::Cdp { result } => Ok(result),
            _ => Err(OwnerError::Unavailable),
        }
    }
    pub async fn settle(&self, tab: Tab) -> Result<(), OwnerError> {
        self.page_request(
            tab,
            Request::Settle {
                tab: tab.id,
                timeout_ms: 30_000,
            },
        )
        .await
        .map(|_| ())
    }
    pub async fn notices(&self, tab: Tab) -> Result<renderer_ipc::Notices, OwnerError> {
        match self
            .page_request(tab, Request::TakeNotices { tab: tab.id })
            .await?
            .0
        {
            Reply::Notices(notices) => Ok(notices),
            _ => Err(OwnerError::Unavailable),
        }
    }
    pub async fn capture(&self, tab: Tab) -> Result<Vec<u8>, OwnerError> {
        let (reply, body) = self
            .page_request(
                tab,
                Request::Capture {
                    tab: tab.id,
                    timeout_ms: 30_000,
                },
            )
            .await?;
        if !matches!(reply, Reply::Png) {
            return Err(OwnerError::Unavailable);
        }
        Ok(body)
    }
}
type RpcReply = Result<(Reply, Vec<u8>), ClientError>;
struct Work {
    request: Request,
    body: Vec<u8>,
    timeout: Duration,
    reply: oneshot::Sender<RpcReply>,
}
type LiveClient = Option<(u64, Arc<RendererClient>)>;
/// The frame connection of the live helper generation, readable without the
/// actor so a lease can be made under the service lock. Cleared before the
/// generation is stopped.
#[derive(Clone, Default)]
struct FrameSource(Arc<std::sync::Mutex<LiveClient>>);
impl FrameSource {
    fn set(&self, generation: u64, client: Arc<RendererClient>) {
        *self.0.lock().unwrap() = Some((generation, client));
    }
    fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
    fn get(&self, generation: u64) -> Option<Arc<RendererClient>> {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(live, _)| *live == generation)
            .map(|(_, client)| client.clone())
    }
}
struct Generation {
    child: GroupChild,
    client: Arc<RendererClient>,
    work: Option<blocking::Sender<Work>>,
    ledger: SpawnLedger,
    workers: Vec<sandbox::ledger::WorkerId>,
    profile: Option<Profile>,
}
impl Generation {
    /// Synchronous spawn transaction. Admission never crosses an await/Hello.
    fn spawn(
        ledger: &SpawnLedger,
        config: &OwnerConfig,
        generation: u64,
    ) -> Result<(Self, Option<OwnerError>), OwnerError> {
        #[cfg(feature = "test-support")]
        let profile = config.remove_profile.map_or_else(
            || Profile::create(&config.profiles),
            |remove| Profile::create_with(&config.profiles, remove),
        );
        #[cfg(not(feature = "test-support"))]
        let profile = Profile::create(&config.profiles);
        let mut profile = profile.map_err(|_| OwnerError::Unavailable)?;
        let channels = renderer_ipc::stdio_channels().map_err(|_| OwnerError::Unavailable)?;
        let mut client_config = ClientConfig::new(RendererGeneration(generation));
        client_config.hello_timeout = config.hello_timeout;
        client_config.frame_pool = config.frame_pool.clone();
        let (client, jobs) =
            RendererClient::prepare(channels.control, channels.frames, client_config)?;
        let client = Arc::new(client);
        let mut command = tokio::process::Command::new(&config.executable);
        command
            .arg("--cache-dir")
            .arg(&profile.path)
            .current_dir(&profile.path)
            .env_clear()
            .env("HOME", &profile.path)
            .env(
                "TMPDIR",
                if cfg!(target_os = "linux") {
                    // Chromium binds temporary Unix sockets: keep the pathname
                    // short while still resolving inside this private profile.
                    std::path::Path::new("/proc/self/cwd")
                } else {
                    profile.path.as_path()
                },
            )
            .env(
                "EVORCH_CEF_HELPER",
                config.executable.with_file_name("evorch-cef-helper"),
            )
            .stdin(channels.helper_stdin)
            .stdout(channels.helper_stdout)
            .stderr(std::process::Stdio::null());
        // Loader paths are deployment configuration, never credentials or DISPLAY.
        for name in ["LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        apply_launch_policy(command.as_std_mut());
        let admission = ledger.admit().map_err(|_| OwnerError::Stopping)?;
        let child = admission
            .spawn_group(ChildKind::Renderer, Some("offline-renderer"), &mut command)
            .map_err(|_| OwnerError::Unavailable)?;
        profile.mark_launched();
        #[cfg(feature = "test-support")]
        if let Some(spawned) = &config.spawned {
            let _ = spawned.send(child.id().unwrap() as i32);
        }
        drop(command); // close duplicate helper endpoints even on later failure
        let mut owned = Self {
            child,
            client: client.clone(),
            work: None,
            ledger: ledger.clone(),
            workers: Vec::new(),
            profile: Some(profile),
        };
        for (index, job) in jobs.into_iter().enumerate() {
            #[cfg(feature = "test-support")]
            if config.fail_after_readers == Some(index) {
                return Ok((owned, Some(OwnerError::Unavailable)));
            }
            #[cfg(not(feature = "test-support"))]
            let _ = index;
            match admission.spawn_registered_worker_thread("renderer-reader", move || job.run()) {
                Ok(id) => owned.workers.push(id),
                Err(_) => return Ok((owned, Some(OwnerError::Unavailable))),
            }
        }
        #[cfg(feature = "test-support")]
        if config.fail_after_readers == Some(2) {
            return Ok((owned, Some(OwnerError::Unavailable)));
        }
        let (work, requests) = blocking::channel::<Work>();
        let ipc_worker = admission.spawn_registered_worker_thread("renderer-ipc", move || {
            while let Ok(work) = requests.recv() {
                let result = client.request(work.request, &work.body, work.timeout);
                let _ = work.reply.send(result);
            }
            client.close();
            client.wait_readers();
        });
        match ipc_worker {
            Ok(id) => owned.workers.push(id),
            Err(_) => return Ok((owned, Some(OwnerError::Unavailable))),
        }
        owned.work = Some(work);
        drop(admission);
        Ok((owned, None))
    }
    async fn rpc(
        &self,
        request: Request,
        body: Vec<u8>,
        timeout: Duration,
        stop: &mut StopSignal,
    ) -> Result<(Reply, Vec<u8>), OwnerError> {
        let (reply, rx) = oneshot::channel();
        self.work
            .as_ref()
            .ok_or(OwnerError::Unavailable)?
            .send(Work {
                request,
                body,
                timeout,
                reply,
            })
            .map_err(|_| OwnerError::Unavailable)?;
        tokio::select! { biased;
            _ = stop.stopped() => Err(OwnerError::Stopping),
            _ = self.child.exited() => Err(OwnerError::Unavailable),
            result = rx => result.map_err(|_| OwnerError::Unavailable)?.map_err(Into::into),
        }
    }
    async fn reclaim(&mut self) -> Result<(), OwnerError> {
        self.client.close();
        self.work.take();
        // GroupChild pins the leader until group kill, then reaps it. Never
        // waitpid(-1), set subreaper, or signal a saved numeric pgid here.
        self.child
            .kill_and_wait()
            .await
            .map_err(|_| OwnerError::Unavailable)?;
        // Completion of a loop is not a thread join. Keep these IDs across
        // cancellation/failure and wait for actual ledger-owned joins before
        // releasing the profile or admitting the next helper generation.
        self.ledger
            .join_worker_ids(&self.workers)
            .await
            .map_err(|_| OwnerError::Unavailable)?;
        if let Some(profile) = self.profile.as_mut() {
            // Keep this owner until deletion OR durable isolation succeeds.
            // Failed deletion then belongs to the bounded persistent queue.
            profile.remove().map_err(|_| OwnerError::Unavailable)?;
            self.profile.take();
        }
        Ok(())
    }
}
impl Drop for Generation {
    fn drop(&mut self) {
        self.client.close();
        self.work.take(); /* GroupChild retains cleanup ownership even on cancellation. */
    }
}
struct Actor {
    ledger: SpawnLedger,
    config: OwnerConfig,
    status: watch::Sender<Status>,
    stop: StopSignal,
    cleanup: Arc<TabCleanup>,
    live: Option<Generation>,
    frames: FrameSource,
    budget: RendererBudget,
    generation: u64,
    next_tab: u64,
    idle: Option<tokio::time::Instant>,
    retry: Option<tokio::time::Instant>,
    failures: u32,
}
impl Actor {
    fn new(
        ledger: SpawnLedger,
        config: OwnerConfig,
        status: watch::Sender<Status>,
        stop: StopSignal,
        cleanup: Arc<TabCleanup>,
        frames: FrameSource,
    ) -> Self {
        Self {
            frames,
            ledger,
            config,
            status,
            stop,
            cleanup,
            live: None,
            budget: RendererBudget::default(),
            generation: 0,
            next_tab: 0,
            idle: None,
            retry: None,
            failures: 0,
        }
    }
    fn phase(&self, phase: Phase) {
        self.status.send_replace(Status {
            phase,
            generation: self.generation,
            tabs: self.budget.len(),
        });
    }
    async fn end(&mut self, graceful: bool, failed: bool) -> Result<(), OwnerError> {
        self.phase(Phase::Stopping);
        self.frames.clear();
        if let Some(live) = &mut self.live {
            if graceful && !self.stop.is_stopped() && !self.ledger.is_fenced() {
                let _ = live
                    .rpc(
                        Request::Shutdown,
                        Vec::new(),
                        Duration::from_secs(1),
                        &mut self.stop,
                    )
                    .await;
            }
            // Keep the generation on reap failure; never start a new one.
            live.reclaim().await?;
        }
        self.live = None;
        self.budget = RendererBudget::default();
        self.cleanup.clear();
        self.idle = None;
        if failed {
            self.failures = (self.failures + 1).min(6);
            self.retry =
                Some(tokio::time::Instant::now() + Duration::from_secs(1 << (self.failures - 1)));
        }
        self.phase(if failed {
            Phase::Failed
        } else {
            Phase::Stopped
        });
        Ok(())
    }
    fn check_backoff(&self) -> Result<(), OwnerError> {
        if self.live.is_none()
            && self
                .retry
                .is_some_and(|retry| tokio::time::Instant::now() < retry)
        {
            return Err(OwnerError::Busy);
        }
        Ok(())
    }
    async fn start(&mut self) -> Result<(), OwnerError> {
        if self.live.is_some() {
            return Ok(());
        }
        self.check_backoff()?;
        self.generation += 1;
        self.phase(Phase::Starting);
        let (live, error) = Generation::spawn(&self.ledger, &self.config, self.generation)?;
        self.frames.set(self.generation, live.client.clone());
        self.live = Some(live);
        if let Some(error) = error {
            return Err(error);
        }
        let (reply, _) = self
            .rpc(
                Request::Hello {
                    protocol: renderer_ipc::PROTOCOL_VERSION,
                },
                Vec::new(),
                self.config.hello_timeout,
            )
            .await?;
        if !matches!(
            reply,
            Reply::Hello {
                protocol: renderer_ipc::PROTOCOL_VERSION,
                ..
            }
        ) {
            return Err(OwnerError::Unavailable);
        }
        self.live
            .as_ref()
            .and_then(|v| v.profile.as_ref())
            .ok_or(OwnerError::Unavailable)?
            .confirm()
            .map_err(|_| OwnerError::Unavailable)?;
        self.phase(Phase::Ready);
        Ok(())
    }
    async fn rpc(
        &mut self,
        request: Request,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<(Reply, Vec<u8>), OwnerError> {
        self.live
            .as_ref()
            .ok_or(OwnerError::Unavailable)?
            .rpc(request, body, timeout, &mut self.stop)
            .await
    }
    fn tab(&self, tab: Tab) -> Result<TabBudget, OwnerError> {
        if tab.generation != self.generation {
            return Err(OwnerError::StaleGeneration);
        }
        self.budget.get(tab.id)
    }
    async fn action(&mut self, action: Action) -> Result<Answer, OwnerError> {
        if self.stop.is_stopped() || self.ledger.is_fenced() {
            return Err(OwnerError::Stopping);
        }
        match action {
            Action::Open(mut open, body) => {
                // Rejected attempts during backoff must not reserve ghost tabs.
                self.check_backoff()?;
                self.next_tab += 1;
                open.tab = TabId(self.next_tab);
                let reservation = TabBudget {
                    viewport: open.viewport,
                    fps: open.frame_rate,
                    streaming: true,
                };
                self.budget.reserve(open.tab, reservation)?; // includes initial paint
                self.start().await?;
                let tab = Tab {
                    generation: self.generation,
                    id: open.tab,
                };
                let wait =
                    Duration::from_millis(u64::from(open.timeout_ms)) + Duration::from_secs(10);
                let (reply, _) = self.rpc(Request::OpenArtifact(open), body, wait).await?;
                if !matches!(reply, Reply::Opened { .. }) {
                    return Err(OwnerError::Unavailable);
                }
                self.idle = None;
                self.failures = 0;
                self.cleanup.opened(tab);
                self.phase(Phase::Ready);
                return Ok(Answer::Open(tab));
            }
            Action::Resize(tab, viewport) => {
                let mut reservation = self.tab(tab)?;
                reservation.viewport = viewport;
                self.budget.reserve(tab.id, reservation)?;
                self.rpc(
                    Request::Resize {
                        tab: tab.id,
                        viewport,
                    },
                    Vec::new(),
                    Duration::from_secs(10),
                )
                .await?;
            }
            Action::Streaming(tab, enabled) => {
                let mut reservation = self.tab(tab)?;
                reservation.streaming = enabled;
                self.budget.reserve(tab.id, reservation)?;
                self.rpc(
                    Request::SetStreaming {
                        tab: tab.id,
                        enabled,
                    },
                    Vec::new(),
                    Duration::from_secs(10),
                )
                .await?;
            }
            Action::FrameRate(tab, fps) => {
                let mut reservation = self.tab(tab)?;
                reservation.fps = fps;
                self.budget.reserve(tab.id, reservation)?;
                self.rpc(
                    Request::SetFrameRate {
                        tab: tab.id,
                        frame_rate: fps,
                    },
                    Vec::new(),
                    Duration::from_secs(10),
                )
                .await?;
            }
            Action::PageRequest(tab, request) => {
                self.tab(tab)?;
                let (reply, body) = self
                    .rpc(request, Vec::new(), Duration::from_secs(40))
                    .await?;
                return Ok(Answer::PageReply(reply, body));
            }
            Action::Close(tab) => {
                self.tab(tab)?;
                self.rpc(
                    Request::CloseTab { tab: tab.id },
                    Vec::new(),
                    Duration::from_secs(10),
                )
                .await?;
                self.budget.remove(tab.id);
                self.cleanup.closed(tab);
                if self.budget.is_empty() {
                    if self.budget.used_4k {
                        self.end(true, false).await?;
                    } else {
                        self.idle = Some(tokio::time::Instant::now() + self.config.idle);
                        self.phase(Phase::Idle);
                    }
                } else {
                    self.phase(Phase::Ready);
                }
            }
            Action::Stop => self.end(true, false).await?,
        }
        Ok(Answer::Done)
    }
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Envelope>,
    ) -> Result<(), crate::bootstrap::BootstrapError> {
        loop {
            if self.stop.is_stopped() {
                break;
            }
            if let Some(tab) = self.cleanup.take() {
                if self.action(Action::Close(tab)).await.is_err()
                    && self.end(false, true).await.is_err()
                {
                    break;
                }
                continue;
            }
            let idle = self.idle;
            let event = tokio::select! { biased;
                _ = self.stop.stopped() => None,
                _ = async { if let Some(live) = &self.live { let _ = live.child.exited().await; } else { std::future::pending::<()>().await; } } => {
                    if self.end(false,true).await.is_err() { break; } continue;
                }
                _ = self.cleanup.requested.notified() => continue,
                command = commands.recv() => command,
                _ = async { if let Some(deadline) = idle { tokio::time::sleep_until(deadline).await; } else { std::future::pending::<()>().await; } } => {
                    if self.end(true,false).await.is_err() { break; } continue;
                }
            };
            let Some(command) = event else {
                break;
            };
            if command.reply.is_closed() {
                continue;
            }
            let page_request = matches!(command.action, Action::PageRequest(..));
            let result = self.action(command.action).await;
            if result.as_ref().is_err_and(|e| {
                matches!(
                    e,
                    OwnerError::Ipc(_) | OwnerError::Unavailable | OwnerError::Stopping
                ) || (!page_request && matches!(e, OwnerError::Remote(_) | OwnerError::TimedOut))
            }) && self.end(false, true).await.is_err()
            {
                let _ = command.reply.send(result);
                break;
            }
            if let Err(Ok(Answer::Open(tab))) = command.reply.send(result) {
                // A cancelled open never strands a private tab or its budget.
                if self.action(Action::Close(tab)).await.is_err()
                    && self.end(false, true).await.is_err()
                {
                    break;
                }
            }
        }
        commands.close();
        // Existing core drain stages close the fence and request AreaTasks stop.
        // No graceful stage is added here, even if an IPC operation is pending.
        self.end(false, false)
            .await
            .map_err(|e| crate::bootstrap::BootstrapError::from(e.to_string()))
    }
}

#[cfg(test)]
mod installation_tests {
    use super::*;
    #[test]
    fn cleanup_only_accepts_live_tabs_and_does_not_retarget_a_new_generation() {
        let cleanup = TabCleanup::default();
        let tabs = (0..renderer_ipc::MAX_TABS)
            .map(|id| Tab {
                generation: 1,
                id: TabId(id as u64),
            })
            .collect::<Vec<_>>();
        for tab in &tabs {
            cleanup.opened(*tab);
        }
        for id in 100..1100 {
            cleanup.request(Tab {
                generation: 1,
                id: TabId(id),
            });
        }
        assert!(cleanup.take().is_none());
        for tab in &tabs {
            cleanup.request(*tab);
            cleanup.request(*tab);
        }
        let mut requested = Vec::new();
        while let Some(tab) = cleanup.take() {
            requested.push(tab);
            cleanup.closed(tab);
        }
        assert_eq!(requested, tabs);
        let replacement = Tab {
            generation: 2,
            id: tabs[0].id,
        };
        cleanup.opened(replacement);
        cleanup.request(tabs[0]);
        assert!(cleanup.take().is_none());
        cleanup.request(replacement);
        assert_eq!(cleanup.take(), Some(replacement));
        cleanup.clear();
        assert!(!cleanup.contains(replacement));
    }
    #[test]
    fn startup_override_wins_without_probing_the_running_executable() {
        let configured = PathBuf::from("/nix/store/distribution/libexec/renderer with spaces");
        let resolved =
            resolve_installed_executable(Some(configured.clone().into_os_string()), || {
                panic!("an explicit startup path must not need current_exe")
            })
            .unwrap();
        assert_eq!(resolved, configured);
    }
    #[test]
    fn invalid_override_is_not_a_path_search_or_colocated_fallback() {
        for invalid in ["", "renderer", "./renderer", "../bin/renderer"] {
            let error = resolve_installed_executable(Some(invalid.into()), || {
                panic!("invalid startup configuration must fail closed")
            })
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }
    #[test]
    fn unconfigured_resolution_uses_the_colocated_candidate_and_propagates_failure() {
        assert_eq!(
            resolve_installed_executable(None, || Ok("/core/bin/core".into())).unwrap(),
            PathBuf::from("/core/bin/evorch-renderer")
        );
        assert!(
            resolve_installed_executable(None, || Err(std::io::Error::other("no executable")))
                .is_err()
        );
    }
    #[tokio::test]
    async fn central_binding_shares_one_owner_store_and_stable_provider_without_spawning() {
        let temp = tempfile::tempdir().unwrap();
        let profiles = temp.path().join("profiles");
        let tasks = AreaTasks::new(tokio::runtime::Handle::current());
        let ledger = SpawnLedger::new();
        let owner = RendererOwner::new(
            &tasks,
            ledger.clone(),
            OwnerConfig::new(temp.path().join("not-installed"), profiles.clone()),
        )
        .unwrap();
        let store = Arc::new(
            runtime::artifacts::ArtifactStore::new(temp.path().join("artifacts")).unwrap(),
        );
        let mut areas = crate::areas::Areas::default();
        areas.browser.renderer = Some(owner.clone());
        areas.bind_artifacts(store.clone());
        assert!(Arc::ptr_eq(areas.artifacts.store.as_ref().unwrap(), &store));
        assert!(
            owner
                .commands
                .same_channel(&areas.artifacts.renderer.as_ref().unwrap().commands)
        );
        assert!(Arc::ptr_eq(
            &areas.artifact_pages().unwrap(),
            &areas.artifact_pages().unwrap()
        ));
        assert_eq!(owner.status().phase, Phase::Stopped);
        assert!(ledger.children().is_empty());
        assert!(!profiles.exists());
        ledger.close_fence();
        tasks.request_stop();
        tasks.shutdown().await.unwrap();
        ledger.join_workers().await.unwrap();
        assert!(!profiles.exists());
    }
}
