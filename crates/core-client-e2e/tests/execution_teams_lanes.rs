//! The desktop (the CEF GUI crate's shared connection and features, drawn and clicked with
//! egui_kittest) and a Web client against the production core composition
//! (`BootstrappedHost`, so the areas run their own event tasks): a click on the screen
//! reaches the core, what the core did comes back to the screen, and the Web client agrees.
//! Only the model and the per-run sandbox are replaced. The execution, teams and lanes
//! screens never start a CEF browser, so none is started here.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use gui::core_client::{
    CoreClientScreen, Shared,
    connection::{Action, serve},
    features::{FeatureAction, FeatureRequest, FeatureView, Selection},
};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::commands::{Command, ComposerRole, ContinueChat, SendChat, ThreadTarget};
use workbench_protocol::namespaces::{
    ConversationCommand, ExecutionCommand, ExecutionQuery, ExecutionReply, LanesQuery, LanesReply,
    WorkspaceQuery, WorkspaceReply,
    execution::{GoalPhase, RunDetailsQuery, RunScope},
    lanes::ListLanes,
    teams::{TeamTaskRef, TeamsQuery, TeamsReply},
};
use workbench_protocol::{envelope::*, ids::*, queries::*};

/// Every turn only says it is idle, so a conversation run saves its context and waits. While
/// the gate (`.1`) is closed a turn does not answer at all: its run stays live and running.
struct HeldModel(
    tokio::sync::Notify,
    tokio::sync::watch::Sender<bool>,
    std::sync::atomic::AtomicUsize,
);

#[async_trait]
impl runtime::AgentModel for HeldModel {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.0.notify_one();
        let _ = self.1.subscribe().wait_for(|open| *open).await;
        Ok(providers::ChatResponse {
            usage_reported: true,
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "idle".into(),
                }],
            },
            usage: Default::default(),
            finish_reason: providers::FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "idle-model".into()
    }
}

/// The runs' sandbox is not what is under test, and bwrap is not available everywhere.
struct Unisolated;

impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

const LEAD: &str = "lead";
/// The height at which the lanes screen's lists scroll (its own bound, to see the list from
/// outside).
const LIST_HEIGHT: f32 = 240.0;
/// The vertical inner margin of a Subagents run card (`SP_2`).
const CARD_PADDING: f32 = 8.0;
const OBJECTIVE: &str = "e2e objective";

struct World {
    boot: BootstrappedHost,
    backend: Arc<evorch_core::backend::HostBackend>,
    model: Arc<HeldModel>,
    project: ProjectId,
    lead: runtime::RunId,
    shared: Arc<Mutex<Shared>>,
    actions: tokio::sync::mpsc::Sender<Action>,
    root: String,
    socket: std::path::PathBuf,
    http: reqwest::Client,
    _instance: evorch_core::instance::Instance,
    // Dropped last: the sockets and the database live below it.
    _web: workbench_web::WebServer,
    _dir: test_socket_dir::SplitDir,
}

fn brief() -> event_bus::LaneBrief {
    brief_of(OBJECTIVE)
}

fn brief_of(objective: &str) -> event_bus::LaneBrief {
    serde_json::from_value(serde_json::json!({
        "objective": objective, "inputs": [], "deliverables": ["patch"],
        "acceptance_criteria": ["review"], "constraints": [], "workspace": {},
        "report_policy": "attached"
    }))
    .unwrap()
}

fn request(
    backend: &evorch_core::backend::HostBackend,
    id: &str,
    body: RequestBody,
) -> ReplyPayload {
    let reply = backend.request(
        RequestEnvelope {
            request_id: RequestId::new(id).unwrap(),
            session_id: SessionId::new("session").unwrap(),
            body,
        },
        Provenance::human(Surface::Cli, ClientId::new("client").unwrap()),
    );
    match reply.result {
        ReplyResult::Success(payload) => payload,
        ReplyResult::Error(error) => panic!("{id} failed: {error:?}"),
    }
}

/// Start a chat of `thread` the way a client does.
fn start_chat(
    backend: &evorch_core::backend::HostBackend,
    project: &ProjectId,
    thread: &str,
) -> runtime::RunId {
    let ReplyPayload::Command(reply) = request(
        backend,
        &format!("start-{thread}"),
        RequestBody::Command(Command::Conversation(ConversationCommand::SendChat(
            SendChat {
                project: project.clone(),
                thread: ThreadId::new(thread).unwrap(),
                submission_id: SubmissionId::new(format!("sub-{thread}")).unwrap(),
                composer_role: ComposerRole::Orchestrator,
                text: "coordinate".into(),
                skills: Vec::new(),
                attachments: Vec::new(),
                model_preference: None,
                fork_seed: None,
            },
        ))),
    ) else {
        panic!("expected a command reply");
    };
    reply.run_id.unwrap().as_str().parse().unwrap()
}

impl World {
    async fn web(&self, id: &str, query: Query) -> QueryReply {
        let reply: ReplyEnvelope = self
            .http
            .post(format!("{}/api/v1/request", self.root))
            .header("Origin", &self.root)
            .json(&RequestEnvelope {
                request_id: RequestId::new(id).unwrap(),
                session_id: SessionId::new("web").unwrap(),
                body: RequestBody::Query(query),
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        match reply.result {
            ReplyResult::Success(ReplyPayload::Query(reply)) => reply,
            other => panic!("{other:?}"),
        }
    }

    async fn web_lanes(
        &self,
        id: &str,
        detached: bool,
    ) -> workbench_protocol::views::Page<workbench_protocol::namespaces::lanes::LaneSummary> {
        let QueryReply::Lanes(LanesReply::Lanes(page)) = self
            .web(
                id,
                Query::Lanes(LanesQuery::ListLanes(ListLanes {
                    lead_thread: ThreadId::new(LEAD).unwrap(),
                    policy: None,
                    detached,
                    page: Default::default(),
                })),
            )
            .await
        else {
            panic!("lanes expected");
        };
        page
    }

    async fn web_execution(
        &self,
        id: &str,
    ) -> workbench_protocol::namespaces::execution::ThreadExecution {
        let QueryReply::Execution(ExecutionReply::ThreadExecution(execution)) = self
            .web(
                id,
                Query::Execution(ExecutionQuery::GetThreadExecution(ThreadTarget {
                    thread: ThreadId::new(LEAD).unwrap(),
                })),
            )
            .await
        else {
            panic!("thread execution expected");
        };
        *execution
    }
}

/// A new desktop connection to the core, the way a started (or restarted) GUI makes it.
async fn connect_gui(
    socket: std::path::PathBuf,
    root_dir: &std::path::Path,
) -> (Arc<Mutex<Shared>>, tokio::sync::mpsc::Sender<Action>) {
    let identity = core_instance::ConfigIdentity::new(&root_dir.join("gui-identity")).unwrap();
    let launcher = core_launcher::Launcher::with_socket(
        identity,
        core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
        gui::core_client::client_config(),
        socket,
    )
    .unwrap();
    let (actions, rx) = tokio::sync::mpsc::channel(64);
    let shared = Arc::new(Mutex::new(Shared::default()));
    tokio::spawn(serve(
        launcher,
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    let changed = shared.lock().unwrap().changed.clone();
    loop {
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        if shared.lock().unwrap().connected {
            break;
        }
        notification.await;
    }
    (shared, actions)
}

/// Until the GUI's projection holds every run of `runs`. A window drawn after that opens a
/// subagent pane for each of them at once, as it does for subagents found running.
async fn cached(shared: &Arc<Mutex<Shared>>, runs: &[runtime::RunId]) {
    let changed = shared.lock().unwrap().changed.clone();
    loop {
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        let held = {
            let state = shared.lock().unwrap();
            runs.iter()
                .all(|run| state.cache.runs.contains_key(&run.to_string()))
        };
        if held {
            return;
        }
        notification.await;
    }
}

/// Waits until `run` has finished its turn and waits for input, which is when its context is saved.
async fn waiting(runtime: &runtime::AgentRuntime, run: runtime::RunId) {
    while runtime.inspect_agent(run).map(|agent| agent.phase).ok()
        != Some(event_bus::AgentRunPhase::Waiting)
    {
        tokio::task::yield_now().await;
    }
}

/// Until the model has been asked `turns` times in all: every run that was started is then in
/// its (held) turn.
async fn turns_reached(model: &HeldModel, turns: usize) {
    while model.2.load(std::sync::atomic::Ordering::SeqCst) < turns {
        model.0.notified().await;
    }
}

/// Until the run is in its turn (the held model does not answer), so that its state, and with
/// it a listing's snapshot, no longer changes.
async fn running(runtime: &runtime::AgentRuntime, run: runtime::RunId) {
    while runtime.inspect_agent(run).map(|agent| agent.phase).ok()
        != Some(event_bus::AgentRunPhase::Running)
    {
        tokio::task::yield_now().await;
    }
}

async fn start() -> World {
    // The sockets live below the directory, so the path stays short.
    let dir = test_socket_dir::SplitDir::new();
    let root_dir = dir.path().canonicalize().unwrap();
    std::fs::write(
        root_dir.join("sidebar.json"),
        serde_json::json!({"version":workspace_state::SIDEBAR_SCHEMA_VERSION,"projects":[],"threads":[]}).to_string(),
    )
    .unwrap();
    let model = Arc::new(HeldModel(
        tokio::sync::Notify::new(),
        tokio::sync::watch::channel(true).0,
        std::sync::atomic::AtomicUsize::new(0),
    ));
    let boot = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root_dir.clone(),
            project_root: root_dir.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        },
        TestSeams {
            model: model.clone(),
            sandbox: Arc::new(Unisolated),
        },
    )
    .await
    .unwrap();

    let identity = core_instance::ConfigIdentity::new(&root_dir.join("gui-identity")).unwrap();
    let instance = evorch_core::instance::Instance::acquire(
        &core_instance::LaunchGuard::acquire(&identity).unwrap(),
        dir.sockets(),
    )
    .unwrap();
    let backend = Arc::new(evorch_core::backend::HostBackend::new(
        boot.host.clone(),
        instance.identity.clone(),
    ));
    let world_parts = (boot, backend, model);
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let socket_backend = world_parts.1.clone();
    tokio::spawn(async move {
        let _keep = _stop;
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let backend = socket_backend.clone();
            let stopped = stopped.clone();
            tokio::spawn(async move {
                let _ = evorch_core::unix::connection(stream, backend, stopped).await;
            });
        }
    });
    let socket = instance.socket.clone();
    let (shared, actions) = connect_gui(socket.clone(), &root_dir).await;
    let web_server = workbench_web::WebServer::start(
        workbench_web::WebServerConfig {
            enabled: true,
            bind: ([127, 0, 0, 1], 0).into(),
            ..Default::default()
        },
        world_parts.1.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let root = format!("http://{}", web_server.local_addr());

    let (boot, backend, model) = world_parts;
    let ReplyPayload::Query(QueryReply::Workspace(WorkspaceReply::Projects(projects))) = request(
        &backend,
        "projects",
        RequestBody::Query(Query::Workspace(WorkspaceQuery::ListProjects(
            Default::default(),
        ))),
    ) else {
        panic!("expected the project list");
    };
    let project = projects.items[0].id.clone();
    let lead = start_chat(&backend, &project, LEAD);
    model.0.notified().await;
    waiting(&boot.runtime, lead).await;
    World {
        boot,
        backend,
        model,
        project,
        lead,
        shared,
        actions,
        root,
        socket,
        http: reqwest::Client::new(),
        _instance: instance,
        _web: web_server,
        _dir: dir,
    }
}

impl World {
    /// A GUI that connects after the core already holds state: what a restarted window sees.
    async fn restarted_desktop(&self) -> Desktop {
        let (shared, actions) = connect_gui(self.socket.clone(), self._dir.path()).await;
        Desktop::new(shared, actions, self.project.clone())
    }

    fn desktop(&self) -> Desktop {
        Desktop::new(
            self.shared.clone(),
            self.actions.clone(),
            self.project.clone(),
        )
    }
}

/// How long one drawn frame lasts, as a running app's. egui_kittest's default (a quarter
/// of a second) is also each frame's predicted time, which egui takes off every delayed
/// repaint: a delay under a quarter second, such as the theme spinner's
/// (`theme::widgets::paint_spinner`) or a poll's, would read as a frame asked for at once,
/// and a wait would draw for ever while subagents run.
const FRAME: f32 = 1.0 / 60.0;

/// How many steps [`Desktop::flush`] runs: the second lays out what the first measured.
/// A step draws one frame per queued event, so this is the frame count only when nothing
/// is queued, as inside [`Desktop::until`].
const FRAMES_PER_FLUSH: usize = 2;

/// How many frames a click waits at most for its target to hold still under the pointer.
const SETTLE_FRAMES: usize = 10;
/// How often [`Desktop::click_until_sent`] presses a target that did not take the press.
const MAX_CLICK_ATTEMPTS: usize = 20;

/// How many frames in a row a wait draws, the screen asking for each at once, before it
/// names itself as a wait that may never end.
const FRAMES_BEFORE_NOTICE: usize = 1000;

/// The desktop's feature screens, drawn and clicked with egui_kittest. Every wait is on a
/// change the connection reported or on a frame the screen asked for at once; a repaint
/// the screen asked for after a delay (the theme spinner's, a poll's) is not followed, so
/// no wait may depend on one. egui's own `egui::Spinner` (`ui.spinner()`) asks for its
/// next frame at once instead, so a wait keeps drawing while one is on screen and names
/// itself after [`FRAMES_BEFORE_NOTICE`] frames.
struct Desktop {
    harness: Harness<'static>,
    shared: Arc<Mutex<Shared>>,
    thread: Arc<Mutex<Option<String>>>,
    asked: Arc<Mutex<Vec<FeatureAction>>>,
    actions: tokio::sync::mpsc::Sender<Action>,
    /// Set when the whole page is drawn: what the page asked for, before it reaches the core.
    inbox: Option<tokio::sync::mpsc::Receiver<Action>>,
    /// The commands the page asked for and that were not sent (the page's own controls).
    commands: Vec<&'static str>,
    /// The commands the features sent to the core, by name, for a wait's report.
    sent: Vec<String>,
}

impl Desktop {
    /// The whole window the client draws (the area tabs and the conversation beside them)
    /// for `thread`. Its queries reach the core; its commands are only recorded.
    fn page(
        shared: Arc<Mutex<Shared>>,
        actions: tokio::sync::mpsc::Sender<Action>,
        project: ProjectId,
        thread: &str,
    ) -> Self {
        let (outbox, inbox) = tokio::sync::mpsc::channel(64);
        let mut screen = CoreClientScreen::new(
            shared.clone(),
            outbox,
            Some(project),
            Some(ThreadId::new(thread).unwrap()),
        );
        let harness = Harness::builder()
            .with_size(egui::vec2(1024.0, 768.0))
            .with_step_dt(FRAME)
            .build_ui(move |ui| screen.ui(ui));
        Self {
            harness,
            shared,
            thread: Arc::new(Mutex::new(None)),
            asked: Arc::new(Mutex::new(Vec::new())),
            actions,
            inbox: Some(inbox),
            commands: Vec::new(),
            sent: Vec::new(),
        }
    }

    fn new(
        shared: Arc<Mutex<Shared>>,
        actions: tokio::sync::mpsc::Sender<Action>,
        project: ProjectId,
    ) -> Self {
        let thread = Arc::new(Mutex::new(None::<String>));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let harness = {
            let (shared, thread, asked) = (shared.clone(), thread.clone(), asked.clone());
            Harness::builder()
                .with_size(egui::vec2(1024.0, 768.0))
                .with_step_dt(FRAME)
                .build_ui(move |ui| {
                    let selection = Selection {
                        project: Some(project.clone()),
                        thread: thread
                            .lock()
                            .unwrap()
                            .as_deref()
                            .map(|id| ThreadId::new(id).unwrap()),
                    };
                    let mut state = shared.lock().unwrap();
                    let state = &mut *state;
                    let view = FeatureView {
                        connected: state.connected,
                        capabilities: &state.capabilities,
                        cache: &state.cache,
                        selection: &selection,
                    };
                    let frame = state.features.render(ui, view);
                    asked.lock().unwrap().extend(frame.actions);
                })
        };
        Self {
            harness,
            shared,
            thread,
            asked,
            actions,
            inbox: None,
            commands: Vec::new(),
            sent: Vec::new(),
        }
    }

    fn select(&self, thread: &str) {
        *self.thread.lock().unwrap() = Some(thread.to_owned());
    }

    async fn flush(&mut self) {
        self.harness.run_steps(FRAMES_PER_FLUSH);
        let asked = std::mem::take(&mut *self.asked.lock().unwrap());
        for action in asked {
            if let FeatureRequest::Command(command) = &action.request {
                self.sent.push(command_name(command));
            }
            self.actions.send(Action::Feature(action)).await.unwrap();
        }
        if let Some(inbox) = self.inbox.as_mut() {
            while let Ok(action) = inbox.try_recv() {
                match &action {
                    Action::Command {
                        command: Command::Conversation(ConversationCommand::ContinueChat(_)),
                        ..
                    } => self.commands.push("continue"),
                    Action::Command {
                        command: Command::Execution(ExecutionCommand::StopChat(_)),
                        ..
                    } => self.commands.push("stop"),
                    Action::Command {
                        command:
                            Command::Conversation(
                                ConversationCommand::SendChat(_)
                                | ConversationCommand::CreateExecution(_),
                            ),
                        ..
                    } => self.commands.push("send"),
                    Action::Command { .. } => self.commands.push("other"),
                    Action::Feature(FeatureAction {
                        request: FeatureRequest::Command(command),
                        ..
                    }) => {
                        self.sent.push(command_name(command));
                        self.actions.send(action).await.unwrap();
                    }
                    _ => self.actions.send(action).await.unwrap(),
                }
            }
        }
    }

    fn shows(&self, label: &str) -> bool {
        self.harness.query_all_by_label(label).next().is_some()
    }

    fn shows_part(&self, part: &str) -> bool {
        self.harness
            .query_all_by_label_contains(part)
            .next()
            .is_some()
    }

    /// Draws the screen again on each change the connection reports, and again at once while
    /// the screen asks for another frame at once (a layout that settles a frame later, a
    /// tab opened by the frame before, a request that waited for room, egui's own
    /// `egui::Spinner`), until `done`. A repaint asked for after a delay (the theme
    /// spinner's, a poll's) is deliberately not followed: `done` must not depend on one.
    /// It has no time limit of its own: a wait that is never met is stopped by the test runner, and so that the run then names it, the wait
    /// prints its caller's line, the status and the commands recorded so far whenever it
    /// blocks on the connection with those changed since it last printed them, and when
    /// it has drawn [`FRAMES_BEFORE_NOTICE`] frames in a row without blocking.
    #[track_caller]
    fn until<'d>(
        &'d mut self,
        done: impl Fn(&Desktop) -> bool + 'd,
    ) -> impl std::future::Future<Output = ()> + 'd {
        let waited_at = std::panic::Location::caller();
        async move {
            let changed = self.shared.lock().unwrap().changed.clone();
            let mut told = None;
            let mut frames = 0;
            loop {
                let notification = changed.notified();
                tokio::pin!(notification);
                notification.as_mut().enable();
                self.flush().await;
                if done(self) {
                    return;
                }
                if self.wants_frame() {
                    frames += FRAMES_PER_FLUSH;
                    if frames >= FRAMES_BEFORE_NOTICE {
                        let first = frames < FRAMES_BEFORE_NOTICE + FRAMES_PER_FLUSH;
                        self.tell(waited_at, Some(frames), &mut told, first);
                    }
                    tokio::task::yield_now().await;
                    continue;
                }
                frames = 0;
                self.tell(waited_at, None, &mut told, false);
                notification.await;
            }
        }
    }

    /// Prints what a wait is doing, with the status and the commands recorded so far, when
    /// it switched between drawing and blocking or those changed since `told`, or `always`.
    /// `drawn` is how many frames it has drawn in a row, or `None` when it blocks on the
    /// connection.
    fn tell(
        &self,
        waited_at: &std::panic::Location<'_>,
        drawn: Option<usize>,
        told: &mut Option<(bool, String, Vec<&'static str>, Vec<String>)>,
        always: bool,
    ) {
        let now = (
            drawn.is_none(),
            self.shared.lock().unwrap().status.clone(),
            self.commands.clone(),
            self.sent.clone(),
        );
        if always || told.as_ref() != Some(&now) {
            let doing = match drawn {
                Some(frames) => format!("has drawn {frames} frames in a row"),
                None => "waits on the connection".to_owned(),
            };
            eprintln!(
                "the wait at {waited_at} {doing}; status: {:?}; commands: {:?}; sent: {:?}",
                now.1, now.2, now.3
            );
            *told = Some(now);
        }
    }

    /// The last frame asked for the next one at once, as a running app would draw it. Only
    /// with a short [`FRAME`] does this tell such a frame from a delayed repaint: egui takes
    /// the frame's predicted time off a repaint's delay, and egui_kittest predicts its own
    /// step, a quarter second by default, so every shorter delay would read as zero.
    fn wants_frame(&self) -> bool {
        self.harness
            .output()
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| viewport.repaint_delay.is_zero())
    }

    async fn click(&mut self, label: &str) {
        self.press(|d| d.harness.get_by_label(label).rect().center())
            .await;
    }

    /// Clicks `label` until the screen has sent a command for it. Now and then a press
    /// reaches nothing although the target did not move (observed with the goal band while
    /// its phase changes; the exact egui cause is not pinned down). Such a click has no
    /// effect to undo, so it is repeated; a click that did land is seen as the command it
    /// sent and is never repeated. The attempts are counted, not timed.
    async fn click_until_sent(&mut self, label: &str) {
        for _ in 0..MAX_CLICK_ATTEMPTS {
            let before = self.sent.len();
            self.click(label).await;
            // The command leaves on the frame after the press that queued it.
            self.flush().await;
            if self.sent.len() > before {
                return;
            }
        }
        panic!(
            "the click on {label:?} sent no command in {MAX_CLICK_ATTEMPTS} attempts; sent: {:?}",
            self.sent
        );
    }

    /// Clicks where `at` finds the target once it holds still. A click queued at a centre
    /// measured in the frame before lands wherever that point is when it is pressed, and a
    /// reply arriving in between may move the layout (a line above appearing or going), so
    /// the pointer rests on the target, re-measured each frame, until it stays put, and is
    /// pressed there; pressing elsewhere than it rests would drag the column instead.
    async fn press(&mut self, at: impl Fn(&Desktop) -> egui::Pos2) {
        let mut pos = at(self);
        for _ in 0..SETTLE_FRAMES {
            self.harness.hover_at(pos);
            self.harness.step();
            let now = at(self);
            if now == pos {
                break;
            }
            pos = now;
        }
        for pressed in [true, false] {
            self.harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            self.harness.step();
        }
        self.flush().await;
    }

    /// Brings the dock tab named `label` forward with a press near its start: a tab bar wider
    /// than its leaf clips the tabs at its end, so a tab's centre may lie outside the window.
    async fn click_tab(&mut self, label: &str) {
        let rect = self.harness.get_by_label(label).rect();
        let pos = egui::pos2(
            (rect.left() + 10.0).min(Self::window().right() - 1.0),
            rect.center().y,
        );
        self.harness.hover_at(pos);
        for pressed in [true, false] {
            self.harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        self.flush().await;
    }

    async fn click_button(&mut self, label: &str) {
        self.press(|d| {
            d.harness
                .get_by_role_and_label(egui::accesskit::Role::Button, label)
                .rect()
                .center()
        })
        .await;
    }

    /// The window the screens are drawn in.
    fn window() -> egui::Rect {
        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1024.0, 768.0))
    }

    /// The centre of the first label that contains one of `parts` and lies in the window: a
    /// point outside every list, where a scroll moves the window itself.
    fn anchor(&self, parts: &[&str]) -> Option<egui::Pos2> {
        parts.iter().find_map(|part| {
            self.harness
                .query_all_by_label_contains(part)
                .map(|node| node.rect().center())
                .find(|center| Self::window().contains(*center))
        })
    }

    /// The rectangle of the bounded list whose first row is `first_row`: where it starts, and
    /// the height at which it scrolls.
    fn list_of(&self, first_row: &str) -> egui::Rect {
        let top_left = self
            .harness
            .query_all_by_label(first_row)
            .next()
            .expect("the list's first row")
            .rect()
            .min;
        egui::Rect::from_min_size(top_left, egui::vec2(1024.0, LIST_HEIGHT))
    }

    /// Whether the first label named `label` lies inside `region` and the window.
    fn within(&self, label: &str, region: egui::Rect) -> bool {
        self.harness
            .query_all_by_label(label)
            .next()
            .is_some_and(|node| {
                let center = node.rect().center();
                region.contains(center) && Self::window().contains(center)
            })
    }

    /// Scrolls over `hover` until `done`, as a person scrolls to a control, and lets go
    /// there, so nothing still scrolls under the click that follows.
    ///
    /// The scroll is a touchpad gesture (a start, moves, an end), not wheel notches: egui
    /// spreads a notch over the frames after it, by each frame's length, so at [`FRAME`]
    /// most of a notch would still be pending when `done` first holds and would move the
    /// screen under the next click. A gesture's move is applied whole in the frame it
    /// arrives, and its end leaves nothing pending, so the screen stays where `done` saw it.
    async fn wheel_until(
        &mut self,
        control: &str,
        hover: impl Fn(&Desktop) -> egui::Pos2,
        done: impl Fn(&Desktop) -> bool,
    ) {
        let mut touching = false;
        for _ in 0..400 {
            if done(self) {
                if !touching {
                    return;
                }
                self.scroll_event(egui::TouchPhase::End, 0.0);
                touching = false;
                self.flush().await;
                continue;
            }
            self.harness.hover_at(hover(self));
            if !touching {
                self.scroll_event(egui::TouchPhase::Start, 0.0);
                touching = true;
            }
            self.scroll_event(egui::TouchPhase::Move, -120.0);
            self.flush().await;
        }
        let at = self
            .harness
            .query_all_by_label(control)
            .map(|node| node.rect())
            .collect::<Vec<_>>();
        panic!(
            "scrolling did not bring \"{control}\" into view: it is at {at:?}, hovering at {:?}",
            hover(self)
        );
    }

    fn scroll_event(&mut self, phase: egui::TouchPhase, dy: f32) {
        self.harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, dy),
            phase,
            modifiers: egui::Modifiers::NONE,
        });
    }

    /// Scrolls the window (not a list) until the first label named `label` is in view.
    async fn scroll_window_to(&mut self, label: &str, anchors: &[&str]) {
        self.wheel_until(
            label,
            |d| d.anchor(anchors).unwrap_or(Self::window().center()),
            |d| d.within(label, Self::window()),
        )
        .await;
    }

    /// Scrolls the list whose first row is `first_row` until the first label named `label`
    /// is inside the list's visible height.
    async fn scroll_list_to(&mut self, first_row: &str, label: &str) {
        self.scroll_list_inset_to(first_row, 0.0, label).await;
    }

    /// [`Desktop::scroll_list_to`] for a list of cards: the first card's name sits
    /// [`CARD_PADDING`] below the list's top, so the list ends that much sooner, and the
    /// label's centre must also clear the card's bottom edge by a few points.
    async fn scroll_cards_to(&mut self, first_card: &str, label: &str) {
        self.scroll_list_inset_to(first_card, CARD_PADDING + 4.0, label)
            .await;
    }

    async fn scroll_list_inset_to(&mut self, first_row: &str, inset: f32, label: &str) {
        let mut list = self.list_of(first_row);
        list.max.y -= inset;
        self.wheel_until(
            label,
            |_| list.min + egui::vec2(10.0, 10.0),
            |d| d.within(label, list),
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn clicks_on_runs_and_goals_reach_the_core_and_its_answers_come_back() {
    let world = start().await;
    let child = world
        .boot
        .runtime
        .delegate_background_as_child(
            world.lead,
            runtime::Role::Worker,
            "child task",
            runtime::RunConfig {
                project_root: Some(world._dir.path().canonicalize().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
    world.model.0.notified().await;
    world
        .boot
        .runtime
        .create_thread_goal(
            LEAD,
            world.lead,
            "ship it".into(),
            vec!["tests pass".into()],
        )
        .unwrap();

    // The lead's subagents are the first level, as the Web client lists them.
    let mut desktop = world.desktop();
    desktop.select(LEAD);
    let QueryReply::Execution(ExecutionReply::RunDetails(children)) = world
        .web(
            "web-children",
            Query::Execution(ExecutionQuery::ListRunDetails(RunDetailsQuery {
                thread: ThreadId::new(LEAD).unwrap(),
                scope: RunScope::Children {
                    parent: RunId::new(world.lead.to_string()).unwrap(),
                },
                page: Default::default(),
            })),
        )
        .await
    else {
        panic!("run details expected");
    };
    assert_eq!(children.items.len(), 1);
    assert_eq!(children.items[0].summary.id.as_str(), child.to_string());
    let child_name = children.items[0].name.clone();
    desktop.until(|d| d.shows(&child_name)).await;

    // Goal review: the conversation band's toggle sends the command, the core flips the goal,
    // and the band and the Subagents tab's status say so. The band is drawn by the whole window only;
    // it shares the column's state, so the selection and the goal already read stay.
    let before = world.web_execution("goal-before").await.goal.unwrap();
    assert!(!before.review_enabled);
    assert_ne!(
        before.phase,
        GoalPhase::Complete,
        "a completed goal's toggle is disabled"
    );
    let mut page = Desktop::page(
        world.shared.clone(),
        world.actions.clone(),
        world.project.clone(),
        LEAD,
    );
    page.until(|d| d.shows("Goal: ship it")).await;
    page.click_until_sent("Enable independent review before completion")
        .await;
    page.until(|d| {
        d.shows("Disable independent review before completion") && d.shows("Goal review: accepted")
    })
    .await;
    let after = world.web_execution("goal-after").await.goal.unwrap();
    assert!(after.review_enabled);

    for run in [child, world.lead] {
        world.boot.runtime.cancel(run).unwrap();
        let _ = world.boot.runtime.wait(run).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lane_operations_on_the_screen_reach_the_ledger_and_survive_a_thread_switch_and_a_reconnect()
 {
    let world = start().await;
    let lane = world
        .boot
        .runtime
        .open_lane(world.lead, "e2e-open", brief())
        .unwrap();
    waiting(&world.boot.runtime, lane.root_run_id).await;
    // The workspace puts both threads in the lead's project; the ledger requires that before a
    // detached lane can be attached to its lead again.
    let side = world.boot.storage_writer().clone();
    let project_root = world._dir.path().canonicalize().unwrap();
    side.clone()
        .upsert_threads_host(&[LEAD, lane.thread_id.as_str()].map(|thread| {
            storage::threads::ThreadUpsert {
                thread_id: thread.to_owned(),
                project_id: Some(world.project.as_str().to_owned()),
                project_root: Some(project_root.to_string_lossy().into_owned()),
                ..Default::default()
            }
        }))
        .unwrap();
    let mut desktop = world.desktop();
    desktop.select(LEAD);

    // The lane is shown as the Web client lists it.
    let listed = world.web_lanes("web-lanes", false).await;
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].lane_id.as_str(), lane.lane_id);
    desktop.until(|d| d.shows(OBJECTIVE)).await;

    // Detach from the screen: the core's event, not a reload, brings the lane to the
    // detached section and takes it out of the lead's list.
    desktop.click("Detach lane").await;
    desktop
        .until(|d| d.shows(&format!("Detached: {OBJECTIVE}")))
        .await;
    assert!(world.web_lanes("web-managed", false).await.items.is_empty());
    assert_eq!(world.web_lanes("web-detached", true).await.items.len(), 1);

    // Another thread and back, then a new connection: the core still offers it.
    desktop.select("some-other-thread");
    desktop
        .until(|d| !d.shows(&format!("Detached: {OBJECTIVE}")))
        .await;
    desktop.select(LEAD);
    desktop
        .until(|d| d.shows(&format!("Detached: {OBJECTIVE}")))
        .await;
    let mut restarted = world.restarted_desktop().await;
    restarted.select(LEAD);
    restarted
        .until(|d| d.shows(&format!("Detached: {OBJECTIVE}")))
        .await;

    // Reattach from the screen: the lead's current root run is read first, then sent.
    desktop.click("Reattach").await;
    desktop
        .until(|d| d.shows("Detach lane") && !d.shows_part("Detached: "))
        .await;
    assert_eq!(
        world.web_lanes("web-reattached", false).await.items.len(),
        1
    );
    assert!(world.web_lanes("web-none", true).await.items.is_empty());

    // The operator stops the lead: the open lane shows its notices held, and the refusal of a
    // resume while the stop stands comes back to the screen. Continuing frees them.
    desktop.click(OBJECTIVE).await;
    desktop.until(|d| d.shows_part("management revision")).await;
    world
        .boot
        .runtime
        .stop(world.lead, runtime::StopScope::SelfOnly)
        .unwrap();
    desktop
        .until(|d| d.shows_part("The lead was stopped"))
        .await;
    desktop.click("Resume notifications").await;
    desktop
        .until(|d| d.shows_part("Resume notifications: ") && d.shows_part("lead_stopped"))
        .await;
    let ReplyPayload::Command(_) = request(
        &world.backend,
        "continue",
        RequestBody::Command(Command::Conversation(ConversationCommand::ContinueChat(
            ContinueChat {
                project: world.project.clone(),
                thread: ThreadId::new(LEAD).unwrap(),
                composer_role: ComposerRole::Orchestrator,
                model_preference: None,
            },
        ))),
    ) else {
        panic!("expected a command reply");
    };
    desktop
        .until(|d| !d.shows_part("The lead was stopped"))
        .await;
    for run in [lane.root_run_id, world.lead] {
        let _ = world.boot.runtime.cancel(run);
    }
}

/// The lane ledger's own record of closes and of the close notices, read straight from the
/// database the core writes.
fn ledger(world: &World, lane_id: &str) -> (Vec<String>, i64) {
    let db = rusqlite::Connection::open_with_flags(
        world
            ._dir
            .path()
            .canonicalize()
            .unwrap()
            .join("evorch-events.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let kinds = db
        .prepare("SELECT kind FROM lane_closes WHERE lane_id=?1 ORDER BY rowid")
        .unwrap()
        .query_map([lane_id], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let notices = db
        .query_row(
            "SELECT COUNT(*) FROM lane_notifications WHERE lane_id=?1 AND event_kind='closed'",
            [lane_id],
            |row| row.get(0),
        )
        .unwrap();
    (kinds, notices)
}

impl World {
    async fn web_close_preview(
        &self,
        id: &str,
        lane: &str,
    ) -> workbench_protocol::namespaces::lanes::LaneClosePreview {
        let QueryReply::Lanes(LanesReply::LaneClosePreview(preview)) = self
            .web(
                id,
                Query::Lanes(LanesQuery::GetLaneClosePreview {
                    lane_id: workbench_protocol::lane_attention::LaneId::new(lane).unwrap(),
                }),
            )
            .await
        else {
            panic!("a close preview was expected");
        };
        *preview
    }
}

impl Desktop {
    async fn type_reason(&mut self, text: &str) {
        let input = self.harness.get_by_role(egui::accesskit::Role::TextInput);
        input.focus();
        self.flush().await;
        self.harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .type_text(text);
        self.flush().await;
    }

    /// The close dialog is open and has read the lane from the core.
    #[track_caller]
    fn until_close_dialog_read<'d>(
        &'d mut self,
        title: &'d str,
    ) -> impl std::future::Future<Output = ()> + 'd {
        self.until(move |d| d.shows(title) && !d.shows_part("Reading the lane from the ledger"))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lane_closed_from_the_screen_is_the_ledgers_record_for_every_client_and_a_resend_adds_none()
 {
    use workbench_protocol::commands::CommandResult;
    use workbench_protocol::lane_management::{CloseLane, HumanLaneCloseKind, LaneCloseRecordKind};
    use workbench_protocol::namespaces::{LanesCommand, LanesResult};

    let world = start().await;
    let lane = world
        .boot
        .runtime
        .open_lane(world.lead, "e2e-close", brief())
        .unwrap();
    waiting(&world.boot.runtime, lane.root_run_id).await;
    let mut desktop = world.desktop();
    desktop.select(LEAD);
    desktop.until(|d| d.shows(OBJECTIVE)).await;
    let phase = |world: &World| {
        (
            world
                .boot
                .runtime
                .inspect_agent(lane.root_run_id)
                .unwrap()
                .phase,
            world.boot.runtime.inspect_agent(world.lead).unwrap().phase,
        )
    };
    let phases = phase(&world);

    // Another client's reading of what the dialog is about to show.
    let seen = world.web_close_preview("web-preview", &lane.lane_id).await;
    assert!(seen.ready.is_none() && seen.current_close.is_none());
    assert_eq!(seen.objective, OBJECTIVE);

    // Cancelling changes nothing.
    desktop.click("Close by human decision…").await;
    desktop
        .until_close_dialog_read("Close lane (unverified)")
        .await;
    assert!(desktop.shows_part("without a result to read"));
    desktop.click("Cancel").await;
    desktop.until(|d| !d.shows("Close lane (unverified)")).await;
    assert_eq!(ledger(&world, &lane.lane_id), (vec![], 0));

    // Close it with a reason.
    desktop.click("Close by human decision…").await;
    desktop
        .until_close_dialog_read("Close lane (unverified)")
        .await;
    desktop.type_reason("checked by hand").await;
    desktop.click("Close lane").await;
    desktop
        .until(|d| d.shows_part("Lane ended: ") && d.shows("Lane close: closed (unverified)"))
        .await;
    assert_eq!(
        ledger(&world, &lane.lane_id),
        (vec!["human_closed".to_owned()], 1)
    );
    // Another client sees the same record in the listing and in the preview.
    let listed = world.web_lanes("web-closed", false).await;
    assert_eq!(listed.items.len(), 1);
    let standing = listed.items[0].current_close.clone().expect("a close");
    assert_eq!(standing.kind, LaneCloseRecordKind::HumanClosed);
    let after = world.web_close_preview("web-after", &lane.lane_id).await;
    assert_eq!(after.current_close, Some(standing.clone()));
    // Nothing else moved: the lane's run and the lead keep their phases.
    assert_eq!(phase(&world), phases);

    // The same request sent again by another client is answered with the record that
    // stands; no record and no notice is added.
    let ReplyPayload::Command(reply) = request(
        &world.backend,
        "resend",
        RequestBody::Command(Command::Lanes(LanesCommand::CloseLane(CloseLane {
            lane_id: workbench_protocol::lane_attention::LaneId::new(&lane.lane_id).unwrap(),
            kind: HumanLaneCloseKind::Complete,
            ready_seq: None,
            expected_management_revision: seen.management_revision,
            expected_request_revision: seen.request_revision,
            note: "checked by hand".into(),
        }))),
    ) else {
        panic!("a command reply was expected");
    };
    let Some(CommandResult::Lanes(LanesResult::Closed(again))) = reply.result.map(|r| *r) else {
        panic!("a close result was expected");
    };
    assert!(again.already_closed);
    assert_eq!(again.close_id, standing.close_id);
    assert_eq!(
        ledger(&world, &lane.lane_id),
        (vec!["human_closed".to_owned()], 1)
    );

    // A window that connects later shows the badge from the ledger.
    let mut restarted = world.restarted_desktop().await;
    restarted.select(LEAD);
    restarted
        .until(|d| d.shows("Lane close: closed (unverified)"))
        .await;
    for run in [lane.root_run_id, world.lead] {
        let _ = world.boot.runtime.cancel(run);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lane_discarded_from_the_screen_is_recorded_as_discarded_and_its_run_keeps_going() {
    let world = start().await;
    let lane = world
        .boot
        .runtime
        .open_lane(world.lead, "e2e-discard", brief())
        .unwrap();
    waiting(&world.boot.runtime, lane.root_run_id).await;
    let mut desktop = world.desktop();
    desktop.select(LEAD);
    desktop.until(|d| d.shows(OBJECTIVE)).await;
    let before = world
        .boot
        .runtime
        .inspect_agent(lane.root_run_id)
        .unwrap()
        .phase;

    desktop.click("Discard…").await;
    desktop.until_close_dialog_read("Discard lane").await;
    // Discarding claims no completion, so the missing Ready is not warned about.
    assert!(!desktop.shows_part("without a result to read"));
    desktop.type_reason("not needed any more").await;
    desktop.click_button("Discard lane").await;
    desktop
        .until(|d| d.shows_part("Lane ended: ") && d.shows("Lane close: discarded"))
        .await;
    assert_eq!(
        ledger(&world, &lane.lane_id),
        (vec!["discarded".to_owned()], 1)
    );
    let after = world.web_close_preview("web-discard", &lane.lane_id).await;
    assert_eq!(
        after.current_close.map(|close| close.kind),
        Some(workbench_protocol::lane_management::LaneCloseRecordKind::Discarded)
    );
    assert_eq!(
        world
            .boot
            .runtime
            .inspect_agent(lane.root_run_id)
            .unwrap()
            .phase,
        before
    );
    assert_eq!(
        world
            .web_lanes("web-still-managed", false)
            .await
            .items
            .len(),
        1
    );
    for run in [lane.root_run_id, world.lead] {
        let _ = world.boot.runtime.cancel(run);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn another_clients_close_puts_a_badge_on_an_open_window_and_a_new_report_takes_it_off() {
    use workbench_protocol::commands::CommandResult;
    use workbench_protocol::lane_management::{CloseLane, HumanLaneCloseKind};
    use workbench_protocol::namespaces::LanesCommand;

    let world = start().await;
    let lane = world
        .boot
        .runtime
        .open_lane(world.lead, "e2e-badge", brief())
        .unwrap();
    waiting(&world.boot.runtime, lane.root_run_id).await;
    // A window of its own (its own connection and subscription), left alone from here on:
    // nothing below reloads it, so only the core's events can change what it shows.
    let mut window = world.restarted_desktop().await;
    window.select(LEAD);
    window.until(|d| d.shows(OBJECTIVE)).await;
    assert!(!window.shows_part("Lane close: "));

    let seen = world.web_close_preview("web-badge", &lane.lane_id).await;
    let ReplyPayload::Command(reply) = request(
        &world.backend,
        "badge-close",
        RequestBody::Command(Command::Lanes(LanesCommand::CloseLane(CloseLane {
            lane_id: workbench_protocol::lane_attention::LaneId::new(&lane.lane_id).unwrap(),
            kind: HumanLaneCloseKind::Discard,
            ready_seq: None,
            expected_management_revision: seen.management_revision,
            expected_request_revision: seen.request_revision,
            note: "from another client".into(),
        }))),
    ) else {
        panic!("a command reply was expected");
    };
    assert!(matches!(
        reply.result.map(|r| *r),
        Some(CommandResult::Lanes(_))
    ));
    window.until(|d| d.shows("Lane close: discarded")).await;

    // The lane reports again, as its run does: the ledger lifts the close and the core
    // announces the report.
    let side = world.boot.storage_writer().clone();
    let appended = side
        .clone()
        .append_lane_report_with_delivery(
            lane.lane_id.clone(),
            seen.management_revision.0,
            event_bus::LaneReportInput {
                status: event_bus::LaneStatus::Working,
                summary: "going again".into(),
                artifact_revision: None,
                failure_episode: None,
                artifact_refs: vec![],
                needs_human: false,
                question_refs: vec![],
                reason_code: None,
                producer: event_bus::lane::LaneReportProducer::LaneAgent,
            },
            1,
        )
        .unwrap();
    world.boot.runtime.event_bus().emit(event_bus::Event::new(
        appended
            .lane
            .report_appended_event(&appended.report, Some(lane.root_run_id.to_string())),
    ));
    window.until(|d| !d.shows_part("Lane close: ")).await;
    assert!(
        world
            .web_close_preview("web-lifted", &lane.lane_id)
            .await
            .current_close
            .is_none()
    );
    for run in [lane.root_run_id, world.lead] {
        let _ = world.boot.runtime.cancel(run);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_click_on_a_team_task_opens_the_detail_the_core_and_the_web_client_report() {
    let world = start().await;
    // The team board is the lead thread's persisted one, written through the real board (a
    // claim included) the way a coordinator saves it; the core reads it as a restarted one would.
    let side = world.boot.storage_writer().clone();
    side.clone()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: LEAD.to_owned(),
            project_id: Some(world.project.as_str().to_owned()),
            project_root: Some(
                world
                    ._dir
                    .path()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        }])
        .unwrap();
    let board = runtime::team::TeamBoard::durable(
        side.clone(),
        format!("{}:{LEAD}", world.project.as_str()),
    );
    for (id, path) in [
        ("task-ready", "src/ready.rs"),
        ("task-claimed", "docs/claimed.md"),
    ] {
        board
            .enqueue(runtime::team::TaskSpec {
                id: id.to_owned(),
                paths: vec![std::path::PathBuf::from(path)],
            })
            .unwrap();
    }
    board
        .claim("task-claimed", &world.lead.to_string(), 1)
        .unwrap();

    // What the core and the Web client report for the claimed task.
    let QueryReply::Teams(TeamsReply::Task(task)) = world
        .web(
            "web-task",
            Query::Teams(TeamsQuery::GetTeamTask(TeamTaskRef {
                thread: ThreadId::new(LEAD).unwrap(),
                task_id: "task-claimed".to_owned(),
            })),
        )
        .await
    else {
        panic!("a team task expected");
    };
    assert_eq!(task.paths, ["docs/claimed.md"]);
    assert_eq!(task.owner.as_deref(), Some(world.lead.to_string().as_str()));
    assert_eq!(
        task.owner_run.as_ref().map(|run| run.as_str()),
        Some(world.lead.to_string().as_str())
    );
    let QueryReply::Teams(TeamsReply::Team(team)) = world
        .web(
            "web-team",
            Query::Teams(TeamsQuery::GetTeam(ThreadTarget {
                thread: ThreadId::new(LEAD).unwrap(),
            })),
        )
        .await
    else {
        panic!("a team overview expected");
    };
    assert_eq!((team.ready, team.claimed, team.complete), (1, 1, 0));

    // The screen lists both tasks and the board's counts; a click on one opens its detail.
    let mut desktop = world.desktop();
    desktop.select(LEAD);
    desktop.until(|d| d.shows("task-claimed")).await;
    assert!(desktop.shows("task-ready"));
    assert!(desktop.shows_part(&format!(
        "{} ready · {} claimed · {} complete",
        team.ready, team.claimed, team.complete
    )));
    assert!(
        !desktop.shows("Task task-claimed"),
        "no detail before the click"
    );
    desktop.click("task-claimed").await;
    desktop.until(|d| d.shows("Task task-claimed")).await;
    assert!(desktop.shows_part(&format!("Generation {}", task.claim_generation.0)));
    assert!(desktop.shows(&format!("Owner: {}", task.owner.as_deref().unwrap())));
    assert!(desktop.shows(&format!(
        "Owner run: {}",
        task.owner_run.as_ref().unwrap().as_str()
    )));
    assert!(desktop.shows(&task.paths[0]));
    assert!(
        !desktop.shows("Task task-ready"),
        "only the clicked task is opened"
    );

    world.boot.runtime.cancel(world.lead).unwrap();
    let _ = world.boot.runtime.wait(world.lead).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_detached_lane_past_the_first_page_is_shown_and_reattached_from_the_screen() {
    let world = start().await;
    // One more than a default page of detached lanes: the last one is behind the cursor.
    let count = workbench_protocol::limits::DEFAULT_PAGE_SIZE as usize + 1;
    let mut lanes = Vec::new();
    for n in 0..count {
        let lane = world
            .boot
            .runtime
            .open_lane(
                world.lead,
                &format!("bulk-{n}"),
                brief_of(&format!("bulk lane {n}")),
            )
            .unwrap();
        waiting(&world.boot.runtime, lane.root_run_id).await;
        lanes.push(lane);
    }
    let side = world.boot.storage_writer().clone();
    let project_root = world._dir.path().canonicalize().unwrap();
    side.clone()
        .upsert_threads_host(
            &std::iter::once(LEAD.to_owned())
                .chain(lanes.iter().map(|lane| lane.thread_id.clone()))
                .map(|thread| storage::threads::ThreadUpsert {
                    thread_id: thread,
                    project_id: Some(world.project.as_str().to_owned()),
                    project_root: Some(project_root.to_string_lossy().into_owned()),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
    for (n, lane) in lanes.iter().enumerate() {
        let ReplyPayload::Command(_) = request(
            &world.backend,
            &format!("detach-{n}"),
            RequestBody::Command(Command::Lanes(
                workbench_protocol::namespaces::LanesCommand::ManageLane(
                    workbench_protocol::lane_management::ManageLane {
                        request: workbench_protocol::lane_management::LaneManagementRequest {
                            operation_id:
                                workbench_protocol::lane_management::LaneManagementOperationId::new(
                                    format!("e2e-detach-{n}"),
                                )
                                .unwrap(),
                            lane_id: workbench_protocol::lane_attention::LaneId::new(&lane.lane_id)
                                .unwrap(),
                            expected_management_revision: workbench_protocol::wire::Revision(1),
                            action:
                                workbench_protocol::lane_management::LaneManagementAction::Detach,
                        },
                        expected_family_revision: None,
                    },
                ),
            )),
        ) else {
            panic!("expected a command reply");
        };
    }
    // The Web client's second page names the lane behind the cursor.
    let first = world.web_lanes("web-page-1", true).await;
    assert_eq!(first.items.len(), count - 1);
    let QueryReply::Lanes(LanesReply::Lanes(second)) = world
        .web(
            "web-page-2",
            Query::Lanes(LanesQuery::ListLanes(ListLanes {
                lead_thread: ThreadId::new(LEAD).unwrap(),
                policy: None,
                detached: true,
                page: PageRequest {
                    cursor: first.next_cursor.clone(),
                    ..Default::default()
                },
            })),
        )
        .await
    else {
        panic!("lanes expected");
    };
    assert_eq!(second.items.len(), 1);
    let far = second.items[0].clone();

    // The screen shows the first page only; the load button brings the far lane, and its own
    // reattach button attaches exactly that lane to the lead again.
    let mut desktop = world.desktop();
    desktop.select(LEAD);
    let far_label = format!("Detached: {}", far.objective_preview);
    desktop
        .until(|d| d.shows_part("Load more detached lanes"))
        .await;
    assert!(
        !desktop.shows(&far_label),
        "the far lane is behind the cursor"
    );
    // The window is 1024x768, the screen lists 50 rows and its buttons are used as a person
    // does: the load button below the list is clicked with the pointer, the far lane's row is
    // scrolled into view and its reattach button is clicked likewise.
    desktop.click("Load more detached lanes").await;
    desktop.until(|d| d.shows(&far_label)).await;
    assert_eq!(
        desktop
            .harness
            .query_all_by_label_contains("Detached: ")
            .count(),
        count
    );
    // The far lane is drawn last, so its reattach button is the last one. The screen is
    // scrolled the way a person does: the window to the list, then the list to the row.
    let first_row = format!("Detached: {}", first.items[0].objective_preview);
    desktop
        .scroll_window_to(&first_row, &["Load more detached lanes", "lane"])
        .await;
    let list = desktop.list_of(&first_row);
    desktop
        .wheel_until(
            "Reattach",
            |_| list.min + egui::vec2(10.0, 10.0),
            |d| {
                let button = d
                    .harness
                    .get_all_by_label("Reattach")
                    .last()
                    .expect("a reattach button")
                    .rect()
                    .center();
                list.contains(button) && Desktop::window().contains(button)
            },
        )
        .await;
    desktop
        .harness
        .get_all_by_label("Reattach")
        .last()
        .expect("a reattach button")
        .click();
    desktop.flush().await;
    // The lane is a managed one again: the screen says so only after the core's event.
    desktop.until(|d| d.shows(&far.objective_preview)).await;
    let attached = world.web_lanes("web-attached", false).await;
    assert_eq!(
        attached
            .items
            .iter()
            .map(|lane| lane.lane_id.as_str())
            .collect::<Vec<_>>(),
        [far.lane_id.as_str()]
    );
    for lane in &lanes {
        let _ = world.boot.runtime.cancel(lane.root_run_id);
    }
    let _ = world.boot.runtime.cancel(world.lead);
}

/// The pages of `parent`'s child runs as the Web client lists them.
async fn web_children(
    world: &World,
    tag: &str,
    parent: runtime::RunId,
    cursor: Option<PageCursor>,
) -> workbench_protocol::views::Page<workbench_protocol::namespaces::execution::RunDetail> {
    let QueryReply::Execution(ExecutionReply::RunDetails(page)) = world
        .web(
            tag,
            Query::Execution(ExecutionQuery::ListRunDetails(RunDetailsQuery {
                thread: ThreadId::new(LEAD).unwrap(),
                scope: RunScope::Children {
                    parent: RunId::new(parent.to_string()).unwrap(),
                },
                page: PageRequest {
                    cursor,
                    ..Default::default()
                },
            })),
        )
        .await
    else {
        panic!("run details expected");
    };
    page
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_history_past_the_first_page_is_paged_and_drilled_into_with_the_pointer_alone() {
    let world = start().await;
    let count = workbench_protocol::limits::DEFAULT_PAGE_SIZE as usize + 1;
    // The children's turns are held, so all of them stay live runs of the lead for the test.
    world.model.1.send_replace(false);
    let asked_before = world.model.2.load(std::sync::atomic::Ordering::SeqCst);
    let mut kids = std::collections::BTreeMap::new();
    for n in 0..count {
        let kid = world
            .boot
            .runtime
            .delegate_background_as_child(
                world.lead,
                runtime::Role::Worker,
                format!("kid {n}"),
                runtime::RunConfig {
                    project_root: Some(world._dir.path().canonicalize().unwrap()),
                    ..Default::default()
                },
            )
            .unwrap();
        kids.insert(kid.to_string(), kid);
    }
    for kid in kids.values() {
        running(&world.boot.runtime, *kid).await;
    }
    turns_reached(&world.model, asked_before + count).await;
    // The run behind the cursor is the one with a run of its own below it.
    let first = web_children(&world, "web-kids-1", world.lead, None).await;
    assert_eq!(first.items.len(), count - 1);
    let second = web_children(&world, "web-kids-2", world.lead, first.next_cursor.clone()).await;
    assert_eq!(second.items.len(), 1);
    let far = second.items[0].clone();
    let grandchild = world
        .boot
        .runtime
        .delegate_background_as_child(
            kids[far.summary.id.as_str()],
            runtime::Role::Worker,
            "grandkid",
            runtime::RunConfig {
                project_root: Some(world._dir.path().canonicalize().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
    running(&world.boot.runtime, grandchild).await;
    turns_reached(&world.model, asked_before + count + 1).await;
    let grandchild_name =
        web_children(&world, "web-grandkids", kids[far.summary.id.as_str()], None)
            .await
            .items[0]
            .name
            .clone();

    // The window is 1024x768 and the screen is used as a person does: every control is
    // scrolled to and clicked with the pointer.
    // The lead's subagents are the tab's first level.
    let mut desktop = world.desktop();
    desktop.select(LEAD);
    desktop.until(|d| d.shows("Load more runs")).await;
    // One Open pane per card, drawn for the rows scrolled out of the list too.
    let rows = |d: &Desktop| d.harness.query_all_by_label("Open pane").count();
    let before = rows(&desktop);
    desktop
        .scroll_window_to("Load more runs", &["Subagents", "Teams"])
        .await;
    desktop.click("Load more runs").await;
    desktop.until(|d| rows(d) > before).await;

    // The far run's own drill-down button is the only one: bring it into the list's view.
    let first_row = first.items[0].name.clone();
    desktop.scroll_cards_to(&first_row, "Subagents (1)").await;
    desktop.click("Subagents (1)").await;
    desktop.until(|d| d.shows(&grandchild_name)).await;
    assert!(
        desktop.shows(far.summary.id.as_str()),
        "the path shows the run that was entered"
    );
    // The path leads back to the lead's subagents.
    desktop.click("Root").await;
    desktop.until(|d| d.shows(&first_row)).await;
    assert!(!desktop.shows(far.summary.id.as_str()));

    for run in kids.values().copied().chain([grandchild, world.lead]) {
        let _ = world.boot.runtime.cancel(run);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_past_the_first_page_is_paged_and_opened_with_its_detail_in_view() {
    let world = start().await;
    let count = workbench_protocol::limits::DEFAULT_PAGE_SIZE as usize + 1;
    let side = world.boot.storage_writer().clone();
    side.clone()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: LEAD.to_owned(),
            project_id: Some(world.project.as_str().to_owned()),
            project_root: Some(
                world
                    ._dir
                    .path()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        }])
        .unwrap();
    let board = runtime::team::TeamBoard::durable(
        side.clone(),
        format!("{}:{LEAD}", world.project.as_str()),
    );
    let id = |n: usize| format!("task-{n:03}");
    for n in 0..count {
        board
            .enqueue(runtime::team::TaskSpec {
                id: id(n),
                paths: vec![std::path::PathBuf::from(format!("src/file-{n}.rs"))],
            })
            .unwrap();
    }

    let mut desktop = world.desktop();
    desktop.select(LEAD);
    desktop.until(|d| d.shows(&id(0))).await;
    assert!(
        !desktop.shows(&id(count - 1)),
        "the last task is behind the cursor"
    );
    desktop
        .scroll_window_to("Load more tasks", &["Teams", " ready · "])
        .await;
    desktop.click("Load more tasks").await;
    desktop.until(|d| d.shows(&id(count - 1))).await;

    // The last task is drawn last: the list is scrolled to it, and the click opens its detail,
    // which must then be reachable in the window.
    desktop.scroll_list_to(&id(0), &id(count - 1)).await;
    desktop.click(&id(count - 1)).await;
    let title = format!("Task {}", id(count - 1));
    desktop.until(|d| d.shows(&title)).await;
    let path = format!("src/file-{}.rs", count - 1);
    desktop
        .scroll_window_to(&path, &["Teams", " ready · ", &title])
        .await;
    assert!(desktop.shows(&path));

    world.boot.runtime.cancel(world.lead).unwrap();
    let _ = world.boot.runtime.wait(world.lead).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_conversation_controls_stay_in_view_beside_a_screenful_of_features() {
    let world = start().await;
    let count = workbench_protocol::limits::DEFAULT_PAGE_SIZE as usize + 1;
    // The ledger is written first: the runs below keep the database busy.
    let side = world.boot.storage_writer().clone();
    side.clone()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: LEAD.to_owned(),
            project_id: Some(world.project.as_str().to_owned()),
            project_root: Some(
                world
                    ._dir
                    .path()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        }])
        .unwrap();
    let board = runtime::team::TeamBoard::durable(
        side.clone(),
        format!("{}:{LEAD}", world.project.as_str()),
    );
    for n in 0..count {
        board
            .enqueue(runtime::team::TaskSpec {
                id: format!("task-{n:03}"),
                paths: vec![std::path::PathBuf::from(format!("src/file-{n}.rs"))],
            })
            .unwrap();
    }

    world.model.1.send_replace(false);
    let mut kids = Vec::new();
    for n in 0..count {
        kids.push(
            world
                .boot
                .runtime
                .delegate_background_as_child(
                    world.lead,
                    runtime::Role::Worker,
                    format!("kid {n}"),
                    runtime::RunConfig {
                        project_root: Some(world._dir.path().canonicalize().unwrap()),
                        ..Default::default()
                    },
                )
                .unwrap(),
        );
    }
    world
        .boot
        .runtime
        .create_thread_goal(
            LEAD,
            world.lead,
            "ship it".into(),
            (0..8).map(|n| format!("criterion {n}")).collect(),
        )
        .unwrap();
    // The window opens on every kid running: each gets a subagent pane below the Subagents
    // tab, and the conversation beside them keeps its controls in view.
    cached(&world.shared, &kids).await;
    // The window the client draws: the areas sit in tabs beside the conversation, so a long
    // list in one of them never pushes the conversation's controls out of view.
    let mut desktop = Desktop::page(
        world.shared.clone(),
        world.actions.clone(),
        world.project.clone(),
        LEAD,
    );
    // The goal's criteria are in the conversation's band now: its details list them.
    desktop.until(|d| d.shows("Show goal details")).await;
    desktop.click("Show goal details").await;
    desktop.until(|d| d.shows_part("criterion 7")).await;
    // Folded again, the details leave the conversation's controls where they were.
    desktop.click("Hide goal details").await;
    // Teams waits behind Subagents; its tab brings the long task list forward. The subagents'
    // count widens the Subagents tab's title, which pushes Teams to the tab bar's edge.
    desktop.click_tab("Teams tab").await;
    desktop.until(|d| d.shows("task-000")).await;
    let in_window = |d: &Desktop, control: &str| {
        d.harness
            .query_all_by_label(control)
            .next()
            .is_some_and(|node| Desktop::window().contains(node.rect().center()))
    };
    desktop.until(|d| in_window(d, "Stop")).await;
    assert!(in_window(&desktop, "Message"));
    // The header and the composer both offer it; either sends the same command.
    desktop
        .harness
        .get_all_by_label("Stop")
        .next()
        .expect("a stop control")
        .click();
    desktop.flush().await;
    desktop.harness.get_by_label("Message").focus();
    desktop
        .harness
        .get_by_label("Message")
        .type_text("one more thing");
    desktop.flush().await;
    assert!(in_window(&desktop, "Queue"));
    desktop.click("Queue").await;
    assert_eq!(desktop.commands, ["stop", "send"]);

    for run in kids.into_iter().chain([world.lead]) {
        let _ = world.boot.runtime.cancel(run);
    }
}

/// A command's name, such as `ResumeLaneDeliveries`, for a wait's report.
fn command_name(command: &Command) -> String {
    let debug = format!("{command:?}");
    debug
        .split(['(', ' ', '{'])
        .nth(1)
        .unwrap_or(&debug)
        .to_owned()
}
