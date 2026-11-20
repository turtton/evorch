//! A real core behind its real Unix socket, with a FAKE observation source.
//!
//! What is real: `unix::connection` (peer check, handshake, routing of data
//! connections), the host's ingress dispatch, the connection registry with its
//! epochs, the observation service and the data plane with its pump.
//! What is fake: the port behind the service (`FakeObservationPort`) and the
//! service's authority (`Authority::fake`). The fake service is installed with
//! a test-only host method, so nothing here proves that production installs
//! `AgentObservationSource`; that is covered by the host's source tests.
//!
//! Every wait is an event: a reply, a record, an end of stream, a channel that
//! reports a finished connection task. Time only moves when a test advances
//! the paused tokio clock, or when the runtime is idle and a timer is due.
#![allow(dead_code, reason = "shared by every test module of this binary")]

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use evorch_core::{backend::HostBackend, unix};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::{mpsc, watch},
    task::AbortHandle,
};
use workbench_client::{ClientConfig, ClientError, UnixTransport, WorkbenchClient};
use workbench_host::{
    areas::browser::observation::{
        ObservedFrame, ObservedPage, fake::FakeObservationPort, service::ObservationService,
    },
    bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams},
    connections::{ConnectionClosedHook, ConnectionHooks},
};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    handshake::BuildInfo,
    ids::{ClientId, ProjectId, RunId, ThreadId},
    namespaces::browser::{
        BrowserCommand, BrowserQuery, BrowserReply, BrowserResult, RendererId,
        data::{self, AckMeta, ChannelOffer, ChunkMeta, EndMeta, ErrorMeta, Kind, PREFIX_LEN},
        observation::{data::AgentFrameStart, *},
    },
    queries::{Query, QueryReply},
};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

static NEXT_DIRECT: AtomicUsize = AtomicUsize::new(0);

pub const PROJECT: &str = "p";
pub const THREAD: &str = "t";

pub fn id(name: &str) -> ClientId {
    ClientId::new(name).unwrap()
}

/// What a test injects around the observation service's own hook. Hooks run in
/// registration order: `first` before the service's, `last` after it.
#[derive(Default)]
pub struct Hooks {
    pub first: Option<ConnectionClosedHook>,
    pub last: Option<ConnectionClosedHook>,
}

pub struct Core {
    pub boot: BootstrappedHost,
    pub real: Option<Real>,
    _dir: test_socket_dir::SplitDir,
    pub socket: PathBuf,
    pub backend: Arc<HostBackend>,
    pub port: Arc<FakeObservationPort>,
    pub connections: Arc<ConnectionHooks>,
    /// The instance epoch the core announces; scopes carry it.
    pub epoch: u64,
    stop: watch::Sender<bool>,
    accepted: Arc<AtomicUsize>,
    kill: Arc<Mutex<Vec<AbortHandle>>>,
    ended: mpsc::UnboundedReceiver<usize>,
    seen_ended: HashSet<usize>,
    server: tokio::task::JoinHandle<()>,
}

impl Core {
    pub async fn start() -> Self {
        Self::start_with(Hooks::default()).await
    }

    pub async fn start_with(hooks: Hooks) -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root,
            ownership_root: dir.sockets().to_path_buf(),
        })
        .await
        .unwrap();
        let backend = Arc::new(HostBackend::new(boot.host.clone(), "observation".into()));
        let epoch = backend.hello().instance_epoch.0;
        let (connections, plane) = {
            let host = boot.host.lock().unwrap();
            (host.connections(), host.renderer_data_plane().unwrap())
        };
        if let Some(hook) = hooks.first {
            connections.register(hook);
        }
        let port = Arc::new(FakeObservationPort::default());
        let service =
            ObservationService::new(Some(port.clone()), plane, connections.clone(), epoch);
        if let Some(hook) = hooks.last {
            connections.register(hook);
        }
        boot.host
            .lock()
            .unwrap()
            .install_agent_observation_for_test(service);

        Self::serve(boot, dir, backend, port, connections, epoch, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn serve(
        boot: BootstrappedHost,
        dir: test_socket_dir::SplitDir,
        backend: Arc<HostBackend>,
        port: Arc<FakeObservationPort>,
        connections: Arc<ConnectionHooks>,
        epoch: u64,
        real: Option<Real>,
    ) -> Self {
        let socket = dir.sockets().join("observation.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (stop, stopped) = watch::channel(false);
        let accepted = Arc::new(AtomicUsize::new(0));
        let kill = Arc::new(Mutex::new(Vec::new()));
        let (done, ended) = mpsc::unbounded_channel();
        let server = {
            let (backend, accepted, kill) = (backend.clone(), accepted.clone(), kill.clone());
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let index = accepted.fetch_add(1, Ordering::SeqCst);
                    let (backend, stopped, done) = (backend.clone(), stopped.clone(), done.clone());
                    let task = tokio::spawn(async move {
                        let _ = unix::connection(stream, backend, stopped).await;
                        let _ = done.send(index);
                    });
                    kill.lock().unwrap().push(task.abort_handle());
                }
            })
        };
        Self {
            boot,
            real,
            _dir: dir,
            socket,
            backend,
            port,
            connections,
            epoch,
            stop,
            accepted,
            kill,
            ended,
            seen_ended: HashSet::new(),
            server,
        }
    }

    /// A core whose observation service is the PRODUCTION one (stored
    /// membership, run phase and Stop request, notifications through the
    /// projection, its lifecycle task) over the fake port, with a REAL run of
    /// the real runtime that the scope of [`Self::real_scope`] names.
    pub async fn start_real() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let project_root = root.join("one");
        std::fs::create_dir_all(&project_root).unwrap();
        let store = SidebarStore::new(root.join("sidebar.json"));
        write_sidebar(&store, &[("p1", &project_root, "t1")]);
        let port = Arc::new(FakeObservationPort::default());
        let model = Arc::new(Controlled::default());
        let boot = BootstrappedHost::open_with_agent_observation_port_for_test(
            BootstrapOptions {
                config_dir: root.clone(),
                project_root: project_root.clone(),
                ownership_root: dir.sockets().to_path_buf(),
            },
            TestSeams {
                model: model.clone(),
                sandbox: Arc::new(Direct),
            },
            port.clone(),
        )
        .await
        .unwrap();
        let backend = Arc::new(HostBackend::new(boot.host.clone(), "observation".into()));
        let epoch = backend.hello().instance_epoch.0;
        let connections = boot.host.lock().unwrap().connections();
        let run = boot
            .runtime
            .delegate_chat(
                "t1",
                runtime::Role::Worker,
                "observe".into(),
                runtime::RunConfig::default(),
            )
            .unwrap();
        let incarnation = boot
            .runtime
            .browser_execution_reader()
            .observe(run)
            .expect("the run is in flight")
            .incarnation;
        let real = Real {
            run,
            incarnation,
            model,
            store,
            project_root,
            writer: boot.composition.storage_writer.clone(),
        };
        real.upsert_thread("p1", &real.project_root);
        Self::serve(boot, dir, backend, port, connections, epoch, Some(real))
    }

    pub fn scope(&self, run: &str, incarnation: &str, page_generation: u64) -> ObservationScope {
        ObservationScope {
            project: ProjectId::new(PROJECT).unwrap(),
            thread: ThreadId::new(THREAD).unwrap(),
            run: RunId::new(run).unwrap(),
            incarnation: RunId::new(incarnation).unwrap(),
            core_epoch: self.epoch,
            browser_generation: 1,
            page_generation,
        }
    }

    /// The index of the connection accepted last. Tests connect one at a time
    /// and wait for the handshake or the channel offer first, so this is theirs.
    pub fn last_accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst) - 1
    }

    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Resolves when the connection task `index` has returned: its lease, slot
    /// and socket are released by then.
    pub async fn wait_ended(&mut self, index: usize) {
        while !self.seen_ended.remove(&index) {
            let ended = self
                .ended
                .recv()
                .await
                .expect("the server outlives the test");
            self.seen_ended.insert(ended);
        }
    }

    /// Aborts the connection task like a killed core worker would: whatever it
    /// owns is dropped where it stands.
    pub fn kill_connection(&self, index: usize) {
        self.kill.lock().unwrap()[index].abort();
    }

    /// Announces the core's shutdown and joins every connection task.
    pub async fn shutdown_and_join(&mut self) {
        self.stop.send_replace(true);
        for index in 0..self.accepted() {
            self.wait_ended(index).await;
        }
    }

    pub async fn connect(&self, name: &str) -> Client {
        let control = WorkbenchClient::connect(
            UnixTransport::connect(&self.socket).await.unwrap(),
            ClientConfig::new(
                id(name),
                BuildInfo {
                    name: "test".into(),
                    version: "1".into(),
                    revision: None,
                },
            ),
        )
        .await
        .unwrap_or_else(|error| panic!("{name} could not connect: {error:?}"));
        Client {
            name: id(name),
            control,
            conn: self.last_accepted(),
        }
    }

    /// A subscribe through the core's own dispatch, without a socket, for a
    /// client whose connection the caller holds open in `connections`: for
    /// after the socket server was stopped.
    pub fn subscribe_direct(&self, client: &str, scope: &ObservationScope) -> ObservationResult {
        use workbench_protocol::{
            envelope::{
                Provenance, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope, Surface,
            },
            ids::{RequestId, SessionId},
        };
        let reply = self.backend.request(
            RequestEnvelope {
                request_id: RequestId::new(format!("direct-{}", uuid::Uuid::new_v4().simple()))
                    .unwrap(),
                session_id: SessionId::new("direct-session").unwrap(),
                body: RequestBody::Command(Command::Browser(
                    BrowserCommand::SubscribeAgentFrames {
                        scope: scope.clone(),
                    },
                )),
            },
            Provenance::human(Surface::Desktop, id(client)),
        );
        match reply.result {
            ReplyResult::Success(ReplyPayload::Command(reply)) => match reply.result.as_deref() {
                Some(CommandResult::Browser(BrowserResult::AgentObservation(result))) => {
                    result.clone()
                }
                other => panic!("unexpected receipt: {other:?}"),
            },
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    /// A data connection that has read the channel offer and nothing else.
    pub async fn data(&self) -> Data {
        let opened = tokio::time::Instant::now();
        let mut stream = UnixStream::connect(&self.socket).await.unwrap();
        stream.write_all(&data::encode_hello()).await.unwrap();
        let Some(prefix) = read_prefix(&mut stream).await else {
            panic!("the core closed the data connection without an offer");
        };
        let mut meta = vec![0u8; prefix.meta_len];
        stream.read_exact(&mut meta).await.unwrap();
        if prefix.kind == Kind::Error {
            panic!("no channel: {:?}", ErrorMeta::decode(&meta).unwrap().code);
        }
        assert_eq!(prefix.kind, Kind::Channel);
        let offer = ChannelOffer::decode(&meta).unwrap();
        assert_eq!(offer.bind_window_ms, data::BIND_WINDOW_MS);
        Data {
            stream,
            channel: offer.channel,
            conn: self.last_accepted(),
            opened,
        }
    }

    /// A data connection the core refuses at once: the record it answers with.
    pub async fn data_refused(&self) -> data::ErrorCode {
        let mut stream = UnixStream::connect(&self.socket).await.unwrap();
        stream.write_all(&data::encode_hello()).await.unwrap();
        match read_item(&mut stream).await {
            Item::Error(code) => code,
            other => panic!("not a refusal: {other:?}"),
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.server.abort();
        for handle in self.kill.lock().unwrap().iter() {
            handle.abort();
        }
    }
}

/// One native client: a control connection with the agent observation calls.
pub struct Client {
    pub name: ClientId,
    pub control: WorkbenchClient,
    pub conn: usize,
}

pub fn code(error: ClientError) -> ErrorCode {
    match error {
        ClientError::Server(error) => error.code,
        other => panic!("not a refusal of the core: {other:?}"),
    }
}

impl Client {
    pub async fn command(&self, command: BrowserCommand) -> Result<ObservationResult, ErrorCode> {
        let reply = self
            .control
            .command(Command::Browser(command))
            .await
            .map_err(code)?;
        match reply.result.as_deref() {
            Some(CommandResult::Browser(BrowserResult::AgentObservation(result))) => {
                Ok(result.clone())
            }
            other => panic!("unexpected receipt: {other:?}"),
        }
    }

    pub async fn query(&self, query: BrowserQuery) -> Result<ObservationReply, ErrorCode> {
        match self
            .control
            .query(Query::Browser(query))
            .await
            .map_err(code)?
        {
            QueryReply::Browser(BrowserReply::AgentObservation(reply)) => Ok(reply),
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    pub async fn list(&self) -> Result<ObservationReply, ErrorCode> {
        self.query(BrowserQuery::ListAgentRuns {
            project: ProjectId::new(PROJECT).unwrap(),
            thread: ThreadId::new(THREAD).unwrap(),
        })
        .await
    }

    pub async fn get(&self, scope: &ObservationScope) -> Result<ObservationReply, ErrorCode> {
        self.query(BrowserQuery::GetAgentObservation {
            scope: scope.clone(),
        })
        .await
    }

    pub async fn subscribe_raw(
        &self,
        scope: &ObservationScope,
    ) -> Result<ObservationResult, ErrorCode> {
        self.command(BrowserCommand::SubscribeAgentFrames {
            scope: scope.clone(),
        })
        .await
    }

    /// A registered subscription and its private token.
    pub async fn subscribe(&self, scope: &ObservationScope) -> Subscribed {
        let ObservationResult::Subscribed {
            subscription,
            state,
        } = self.subscribe_raw(scope).await.unwrap()
        else {
            panic!("not subscribed")
        };
        assert_eq!(state, ObservationState::Starting);
        let token = match self.subscription(&subscription).await.unwrap() {
            ObservationReply::Subscription {
                token: Some(token), ..
            } => token,
            other => panic!("no token: {other:?}"),
        };
        Subscribed {
            subscription,
            token,
        }
    }

    pub async fn subscription(
        &self,
        subscription: &ObservationId,
    ) -> Result<ObservationReply, ErrorCode> {
        self.query(BrowserQuery::GetAgentSubscription {
            subscription: subscription.clone(),
        })
        .await
    }

    pub async fn unsubscribe(
        &self,
        subscription: &ObservationId,
    ) -> Result<ObservationResult, ErrorCode> {
        self.command(BrowserCommand::UnsubscribeAgentFrames {
            subscription: subscription.clone(),
        })
        .await
    }

    /// The bind command of the control connection for `channel`.
    pub async fn bind(
        &self,
        channel: &RendererId,
        subscription: &ObservationId,
        token: &ObservationId,
    ) -> Result<ObservationResult, ErrorCode> {
        self.command(BrowserCommand::BindAgentData {
            channel_id: channel.clone(),
            subscription: subscription.clone(),
            token: token.clone(),
        })
        .await
    }

    /// Subscribe, open a data connection and bind it: a live stream.
    pub async fn live(&self, core: &Core, scope: &ObservationScope) -> Live {
        let sub = self.subscribe(scope).await;
        let data = core.data().await;
        assert_eq!(
            self.bind(&data.channel, &sub.subscription, &sub.token)
                .await
                .unwrap(),
            ObservationResult::Bound {}
        );
        Live { sub, data }
    }
}

pub struct Subscribed {
    pub subscription: ObservationId,
    pub token: ObservationId,
}

pub struct Live {
    pub sub: Subscribed,
    pub data: Data,
}

/// A raw data connection. Reading is by record, so a test sees exactly what
/// the core wrote, including the absence of a closing record.
pub struct Data {
    pub stream: UnixStream,
    pub channel: RendererId,
    pub conn: usize,
    /// The (possibly virtual) instant the channel was offered.
    pub opened: tokio::time::Instant,
}

#[derive(Debug)]
pub enum Item {
    Frame(AgentFrameStart, Vec<u8>),
    Error(data::ErrorCode),
    /// The end of the stream with nothing said.
    Eof,
}

async fn read_prefix(stream: &mut UnixStream) -> Option<data::Prefix> {
    let mut prefix = [0u8; PREFIX_LEN];
    if stream.read(&mut prefix[..1]).await.unwrap_or(0) == 0 {
        return None;
    }
    stream.read_exact(&mut prefix[1..]).await.unwrap();
    Some(data::Prefix::parse(&prefix).unwrap())
}

/// The next whole record or frame. A frame is NOT acknowledged here.
pub async fn read_item(stream: &mut UnixStream) -> Item {
    let Some(prefix) = read_prefix(stream).await else {
        return Item::Eof;
    };
    let mut meta = vec![0u8; prefix.meta_len];
    stream.read_exact(&mut meta).await.unwrap();
    match prefix.kind {
        Kind::Error => Item::Error(ErrorMeta::decode(&meta).unwrap().code),
        Kind::AgentFrameStart => {
            let start = AgentFrameStart::decode(&meta).unwrap();
            let mut rgba = Vec::with_capacity(start.total_bytes as usize);
            loop {
                let prefix = read_prefix(stream).await.expect("a frame ends with End");
                let mut meta = vec![0u8; prefix.meta_len];
                stream.read_exact(&mut meta).await.unwrap();
                match prefix.kind {
                    Kind::Chunk => {
                        let chunk = ChunkMeta::decode(&meta).unwrap();
                        assert_eq!(chunk.offset as usize, rgba.len());
                        let mut body = vec![0u8; prefix.body_len];
                        stream.read_exact(&mut body).await.unwrap();
                        rgba.extend_from_slice(&body);
                    }
                    Kind::End => {
                        assert_eq!(
                            EndMeta::decode(&meta).unwrap().total_bytes as usize,
                            rgba.len()
                        );
                        break;
                    }
                    other => panic!("unexpected record {other:?} inside a frame"),
                }
            }
            Item::Frame(start, rgba)
        }
        other => panic!("unexpected record {other:?}"),
    }
}

pub async fn ack(stream: &mut UnixStream, sequence: u64) {
    stream
        .write_all(&AckMeta { id: sequence }.encode())
        .await
        .unwrap();
}

impl Data {
    pub async fn item(&mut self) -> Item {
        read_item(&mut self.stream).await
    }

    /// The next frame, acknowledged.
    pub async fn frame(&mut self) -> (AgentFrameStart, Vec<u8>) {
        match self.item().await {
            Item::Frame(start, rgba) => {
                ack(&mut self.stream, start.sequence).await;
                (start, rgba)
            }
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    /// The core said why the stream ended and then closed it.
    pub async fn expect_error_then_eof(&mut self, expected: data::ErrorCode) {
        match self.item().await {
            Item::Error(code) => assert_eq!(code, expected),
            other => panic!("expected {expected:?}, got {other:?}"),
        }
        assert!(
            matches!(self.item().await, Item::Eof),
            "the error record is the last"
        );
    }

    pub async fn expect_closed(&mut self) {
        self.expect_error_then_eof(data::ErrorCode::Closed).await;
    }

    /// An unbound channel is closed by the core after the bind window, and
    /// not before. Needs the paused clock: the wait is virtual.
    pub async fn expect_expired_after_the_window(&mut self) {
        self.expect_error_then_eof(data::ErrorCode::Expired).await;
        assert!(
            self.opened.elapsed()
                >= std::time::Duration::from_millis(u64::from(data::BIND_WINDOW_MS)),
            "the channel expired before its bind window"
        );
    }
}

/// Stops the clock. From here only the test (or an idle runtime with a due
/// timer) moves time, and no test waits on a real duration.
pub fn pause() {
    tokio::time::pause();
}

/// A one-pixel page of `scope` with its metadata. `title` identifies the
/// metadata revision, `pixel` the pixels, so a test can tell old from new.
pub fn page(
    scope: &ObservationScope,
    tab: char,
    revision: u64,
    sequence: u64,
    title: &str,
    pixel: [u8; 4],
) -> ObservedPage {
    let tab = ObservationId::new(tab.to_string().repeat(32)).unwrap();
    ObservedPage {
        snapshot: ObservationSnapshot {
            scope: scope.clone(),
            revision,
            state: ObservationState::Live,
            tabs: vec![ObservationTab {
                id: tab.clone(),
                title: title.into(),
                url: "local-only".into(),
            }],
            active_tab: Some(tab.clone()),
            activity: vec![],
        },
        frame: Some(Arc::new(ObservedFrame {
            start: AgentFrameStart {
                scope: scope.clone(),
                tab,
                sequence,
                width: 1,
                height: 1,
                stride: 4,
                total_bytes: 4,
            },
            rgba: Arc::from(pixel),
        })),
    }
}

/// The largest page the protocol allows. Far more than a socket buffer holds,
/// so a reader that stops reading leaves the core's write half way through.
pub fn big_page(scope: &ObservationScope, revision: u64, sequence: u64) -> ObservedPage {
    let mut page = page(scope, 'a', revision, sequence, "big", [0; 4]);
    let frame = Arc::make_mut(page.frame.as_mut().unwrap());
    frame.start.width = MAX_WIDTH;
    frame.start.height = MAX_HEIGHT;
    frame.start.stride = MAX_WIDTH * 4;
    frame.start.total_bytes = MAX_FRAME_BYTES;
    frame.rgba = vec![7; MAX_FRAME_BYTES as usize].into();
    page
}

pub fn with_state(mut page: ObservedPage, state: ObservationState) -> ObservedPage {
    page.snapshot.state = state;
    page
}

pub fn title(reply: &ObservationReply) -> &str {
    match reply {
        ObservationReply::Snapshot(snapshot) => &snapshot.tabs[0].title,
        other => panic!("not a snapshot: {other:?}"),
    }
}

impl Data {
    /// Reads the start record of the next frame and nothing after it.
    pub async fn read_start_only(&mut self) -> AgentFrameStart {
        let prefix = read_prefix(&mut self.stream).await.expect("a frame start");
        assert_eq!(prefix.kind, Kind::AgentFrameStart);
        let mut meta = vec![0u8; prefix.meta_len];
        self.stream.read_exact(&mut meta).await.unwrap();
        AgentFrameStart::decode(&meta).unwrap()
    }

    /// Everything that is left on the stream, up to its end.
    pub async fn rest(&mut self) -> Vec<u8> {
        let mut rest = Vec::new();
        self.stream.read_to_end(&mut rest).await.unwrap();
        rest
    }
}

/// The last record of `bytes` is an `Error` record with `code`.
pub fn ends_with_error(bytes: &[u8], code: data::ErrorCode) -> bool {
    let tail = PREFIX_LEN + ErrorMeta::LEN;
    if bytes.len() < tail {
        return false;
    }
    let tail = &bytes[bytes.len() - tail..];
    let Ok(prefix) = data::Prefix::parse(tail[..PREFIX_LEN].try_into().unwrap()) else {
        return false;
    };
    prefix.kind == Kind::Error
        && ErrorMeta::decode(&tail[PREFIX_LEN..]).is_ok_and(|meta| meta.code == code)
}

/// A frame of the largest size stays half written only if the socket buffer
/// is smaller than it. The core's sockets use the system default; fail loudly
/// where that is not so rather than letting a test pass for the wrong reason.
pub fn assert_frames_exceed_socket_buffers() {
    let default = std::fs::read_to_string("/proc/sys/net/core/wmem_default")
        .expect("wmem_default")
        .trim()
        .parse::<u64>()
        .unwrap();
    assert!(
        default.saturating_mul(2) < MAX_FRAME_BYTES,
        "net.core.wmem_default ({default}) is too large for a half-written frame"
    );
}

// ---- the real authority ------------------------------------------------------

/// The model of the real runtime: it answers, or fails, only when a test says
/// so, so a run stays in flight until the test decides how it ends.
#[derive(Default)]
pub struct Controlled {
    release: tokio::sync::Notify,
    fail: std::sync::atomic::AtomicBool,
}
impl Controlled {
    pub fn answer(&self) {
        self.release.notify_one();
    }
    pub fn fail(&self) {
        self.fail.store(true, Ordering::SeqCst);
        self.release.notify_one();
    }
}
#[async_trait::async_trait]
impl runtime::AgentModel for Controlled {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.release.notified().await;
        if self.fail.load(Ordering::SeqCst) {
            return Err(runtime::RuntimeError::Model {
                reason: "the test made the model fail".into(),
            });
        }
        Ok(providers::ChatResponse {
            usage_reported: true,
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: providers::Usage::default(),
        })
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "agent-observation-transport".into()
    }
}

pub struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

/// Writes the sidebar of the core: one project per entry with its thread.
pub fn write_sidebar(store: &SidebarStore, projects: &[(&str, &std::path::Path, &str)]) {
    let mut document = SidebarDocument::default();
    for (project, path, thread) in projects {
        document
            .sidebar
            .add_project(StateProject::new(*project), *project, path)
            .unwrap();
        document.sidebar.threads.push(ThreadRecord::new(
            StateThread::new(*thread),
            StateProject::new(*project),
            *thread,
        ));
    }
    document.sidebar.refresh_derived();
    let lock = store.lock().unwrap();
    store.write(&lock, &document).unwrap();
}

/// What the real fixture owns: the run of the real runtime, its stored thread
/// and the sidebar the authority reads.
pub struct Real {
    pub run: runtime::RunId,
    pub incarnation: String,
    pub model: Arc<Controlled>,
    pub store: SidebarStore,
    pub project_root: PathBuf,
    pub writer: storage::StorageHandle,
}
impl Real {
    /// Stores thread `t1` under `project`, with the project directory `root`.
    pub fn upsert_thread(&self, project: &str, root: &std::path::Path) {
        self.writer
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: "t1".into(),
                project_id: Some(project.into()),
                project_root: Some(root.to_string_lossy().into_owned()),
                title: Some("t1".into()),
                root_run_id: Some(self.run.to_string()),
                ..Default::default()
            }])
            .unwrap();
    }
}

impl Core {
    /// The scope of the run of the real fixture.
    pub fn real_scope(&self, page_generation: u64) -> ObservationScope {
        let real = self.real.as_ref().expect("a real core");
        ObservationScope {
            project: ProjectId::new("p1").unwrap(),
            thread: ThreadId::new("t1").unwrap(),
            run: RunId::new(real.run.to_string()).unwrap(),
            incarnation: RunId::new(real.incarnation.clone()).unwrap(),
            core_epoch: self.epoch,
            browser_generation: 1,
            page_generation,
        }
    }

    pub fn runtime(&self) -> &runtime::AgentRuntime {
        &self.boot.runtime
    }
}
