//! F2's authorization matrix (plan §8.1) over the core's real Unix socket:
//! two clients, A and B, connect through the production `unix::connection`
//! and `HostBackend`, and the provenance the server derives from each
//! connection is what authorizes the renderer operations. The in-process
//! variant with hand-made provenance is `renderer_authorization.rs`.
//!
//! Every wait is an event of the subscription, never a duration.
#![cfg(all(target_os = "linux", feature = "renderer-test-helper"))]

use std::sync::{Arc, Mutex};

use evorch_core::{backend::HostBackend, unix};
use runtime::artifacts::{CaptureRequest, store_with_refs};
use workbench_client::{
    ClientConfig, ClientError, FrameSink, Subscription, Transport, TransportError, UnixTransport,
    WorkbenchClient,
};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::{Subscribe, SubscriptionScope},
    handshake::BuildInfo,
    ids::{ClientId, ProjectId, ThreadId},
    namespaces::{
        artifacts::{ArtifactRef, ArtifactsCommand, ArtifactsResult},
        browser::*,
    },
    queries::{Query, QueryReply},
};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

struct Idle;
#[async_trait::async_trait]
impl runtime::AgentModel for Idle {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        unreachable!("no turn runs in this fixture")
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "renderer-authorization-uds".into()
    }
}
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

/// How the next outgoing request is rewritten, as a client that tries to name
/// its own provenance would.
type Tamper = Box<dyn Fn(&mut serde_json::Value) + Send>;

/// A real Unix transport whose sink can rewrite one request before it leaves.
struct Tampering {
    inner: UnixTransport,
    next: Arc<Mutex<Option<Tamper>>>,
}
struct TamperingSink<S> {
    inner: S,
    next: Arc<Mutex<Option<Tamper>>>,
}
impl Transport for Tampering {
    type Sink = TamperingSink<<UnixTransport as Transport>::Sink>;
    type Source = <UnixTransport as Transport>::Source;
    fn split(self) -> (Self::Sink, Self::Source) {
        let (sink, source) = self.inner.split();
        (
            TamperingSink {
                inner: sink,
                next: self.next,
            },
            source,
        )
    }
}
impl<S: FrameSink> FrameSink for TamperingSink<S> {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let Some(tamper) = self.next.lock().unwrap().take() else {
            return self.inner.send_frame(frame).await;
        };
        // A frame is a big-endian length and a JSON payload.
        let mut request: serde_json::Value = serde_json::from_slice(&frame[4..]).unwrap();
        tamper(&mut request);
        let payload = serde_json::to_vec(&request).unwrap();
        let mut forged = (payload.len() as u32).to_be_bytes().to_vec();
        forged.extend_from_slice(&payload);
        self.inner.send_frame(&forged).await
    }
}
struct Peer {
    client: WorkbenchClient,
    events: Subscription,
    tamper: Arc<Mutex<Option<Tamper>>>,
}
impl Peer {
    async fn connect(socket: &std::path::Path, name: &str) -> Self {
        let tamper = Arc::new(Mutex::new(None));
        let client = WorkbenchClient::connect(
            Tampering {
                inner: UnixTransport::connect(socket).await.unwrap(),
                next: tamper.clone(),
            },
            ClientConfig::new(
                ClientId::new(name).unwrap(),
                BuildInfo {
                    name: "test".into(),
                    version: "1".into(),
                    revision: None,
                },
            ),
        )
        .await
        .unwrap_or_else(|error| panic!("{name} could not connect: {error:?}"));
        let events = client
            .subscribe(Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            })
            .await
            .unwrap();
        Self {
            client,
            events,
            tamper,
        }
    }
    async fn browser(&self, command: BrowserCommand) -> Result<RendererId, ErrorCode> {
        self.accepted(Command::Browser(command)).await
    }
    async fn accepted(&self, command: Command) -> Result<RendererId, ErrorCode> {
        let reply = self.client.command(command).await.map_err(code)?;
        match reply.result.as_deref() {
            Some(CommandResult::Browser(BrowserResult::Accepted { operation_id }))
            | Some(CommandResult::Artifacts(ArtifactsResult::Accepted { operation_id })) => {
                Ok(operation_id.clone())
            }
            other => panic!("unexpected receipt: {other:?}"),
        }
    }
    async fn query(&self, query: BrowserQuery) -> Result<BrowserReply, ErrorCode> {
        match self
            .client
            .query(Query::Browser(query))
            .await
            .map_err(code)?
        {
            QueryReply::Browser(reply) => Ok(reply),
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    async fn operation(&self, id: &RendererId) -> Result<RendererOperationState, ErrorCode> {
        match self
            .query(BrowserQuery::GetRendererOperation {
                operation_id: id.clone(),
            })
            .await?
        {
            BrowserReply::Operation(operation) => Ok(operation.state),
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    /// The operation's output; the subscription says when it changed.
    async fn output(&mut self, id: &RendererId) -> RendererOutput {
        loop {
            match self.operation(id).await.unwrap() {
                RendererOperationState::Running => {
                    self.events.next().await.expect("the stream stays open");
                }
                RendererOperationState::Completed { output } => return output,
                other => panic!("the operation did not complete: {other:?}"),
            }
        }
    }
    async fn tabs(&self, offset: u32, limit: u32) -> (Vec<RendererTab>, bool) {
        match self
            .query(BrowserQuery::ListRendererTabs { offset, limit })
            .await
            .unwrap()
        {
            BrowserReply::Tabs { tabs, more } => (tabs, more),
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    async fn session(&mut self) -> RendererSessionRef {
        let id = self
            .browser(BrowserCommand::CreateRendererSession {})
            .await
            .unwrap();
        match self.output(&id).await {
            RendererOutput::Session { session, .. } => session,
            other => panic!("unexpected output: {other:?}"),
        }
    }
    async fn open(&mut self, artifact: &ArtifactRef, session: &RendererSessionRef) -> RendererTab {
        let id = self
            .accepted(Command::Artifacts(ArtifactsCommand::OpenPreview {
                artifact: artifact.clone(),
                session: session.clone(),
                viewport: RendererViewport {
                    width: 1024,
                    height: 768,
                },
            }))
            .await
            .unwrap();
        match self.output(&id).await {
            RendererOutput::Tab(tab) => *tab,
            other => panic!("unexpected output: {other:?}"),
        }
    }
    /// The next request this peer sends is rewritten by `tamper`.
    fn tamper(&self, tamper: impl Fn(&mut serde_json::Value) + Send + 'static) {
        *self.tamper.lock().unwrap() = Some(Box::new(tamper));
    }
}
fn code(error: ClientError) -> ErrorCode {
    match error {
        ClientError::Server(error) => error.code,
        other => panic!("not a refusal of the core: {other:?}"),
    }
}

#[test]
fn clients_over_the_unix_socket_are_authorized_by_the_connection_not_by_the_request() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    // The core reads the helper from this variable when it bootstraps; this
    // binary holds one test and sets it before any thread exists.
    // SAFETY: single-threaded at this point.
    unsafe {
        std::env::set_var(
            "EVORCH_RENDERER_EXEC",
            std::env::var_os("CARGO_BIN_EXE_evorch-renderer-test-helper").unwrap(),
        )
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(dir, root));
}

async fn run(dir: test_socket_dir::SplitDir, root: std::path::PathBuf) {
    let (one, two) = (root.join("one"), root.join("two"));
    let store = SidebarStore::new(root.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    for (project, path, thread) in [("p1", &one, "t1"), ("p2", &two, "t2")] {
        std::fs::create_dir_all(path).unwrap();
        document
            .sidebar
            .add_project(StateProject::new(project), project, path)
            .unwrap();
        document.sidebar.threads.push(ThreadRecord::new(
            StateThread::new(thread),
            StateProject::new(project),
            thread,
        ));
    }
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let mut core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: one.clone(),
            ownership_root: dir.sockets().into(),
        },
        TestSeams {
            model: Arc::new(Idle),
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    let writer = core.composition.storage_writer.clone();
    for (project, path, thread, run) in [("p1", &one, "t1", "r1"), ("p2", &two, "t2", "r2")] {
        writer
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: thread.into(),
                project_id: Some(project.into()),
                project_root: Some(path.to_string_lossy().into_owned()),
                title: Some(thread.into()),
                root_run_id: Some(run.into()),
                ..Default::default()
            }])
            .unwrap();
    }
    std::fs::write(one.join("page.html"), "<p>one</p>").unwrap();
    std::fs::write(one.join("note.html"), "<p>second</p>").unwrap();
    std::fs::write(two.join("other.html"), "<p>other</p>").unwrap();
    let artifacts = store_with_refs(root.join("artifacts"), writer).unwrap();
    let capture = |base: &std::path::Path, file: &str, run: &str, thread: &str, project: &str| {
        let record = artifacts
            .capture(
                base,
                None,
                &base.join(file),
                CaptureRequest {
                    title: file,
                    caption: None,
                    run_id: run.into(),
                    root_run_id: run.into(),
                    thread_id: Some(thread.into()),
                },
            )
            .unwrap();
        ArtifactRef {
            project: ProjectId::new(project).unwrap(),
            thread: ThreadId::new(thread).unwrap(),
            artifact_id: record.artifact_id,
            digest: record.sha256,
        }
    };
    let page = capture(&one, "page.html", "r1", "t1", "p1");
    let note = capture(&one, "note.html", "r1", "t1", "p1");
    let other = capture(&two, "other.html", "r2", "t2", "p2");

    // The production server: every accepted connection runs `unix::connection`.
    let socket = dir.sockets().join("renderer-authorization.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let backend = Arc::new(HostBackend::new(core.host.clone(), "authorization".into()));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (backend, stopped) = (backend.clone(), stopped.clone());
            tokio::spawn(async move {
                let _ = unix::connection(stream, backend, stopped).await;
            });
        }
    });

    let mut a = Peer::connect(&socket, "client-a").await;
    let mut b = Peer::connect(&socket, "client-b").await;
    let session_a = a.session().await;
    let session_b = b.session().await;

    // A and B create tabs; each owns only its own.
    let a1 = a.open(&page, &session_a).await;
    let a2 = a.open(&note, &session_a).await;
    let b1 = b.open(&other, &session_b).await;
    let ids = |tabs: &[RendererTab]| {
        let mut ids: Vec<_> = tabs
            .iter()
            .map(|tab| tab.reference.id.as_str().to_owned())
            .collect();
        ids.sort();
        ids
    };
    let mut expected = vec![
        a1.reference.id.as_str().to_owned(),
        a2.reference.id.as_str().to_owned(),
    ];
    expected.sort();
    let (listed, more) = a.tabs(0, 4).await;
    assert!(!more);
    assert_eq!(ids(&listed), expected);
    let (first, more_first) = a.tabs(0, 1).await;
    let (second, more_second) = a.tabs(1, 1).await;
    assert_eq!(
        (first.len(), more_first, second.len(), more_second),
        (1, true, 1, false)
    );
    assert_eq!(ids(&[first, second].concat()), expected);
    let (listed_b, more_b) = b.tabs(0, 4).await;
    assert!(!more_b);
    assert_eq!(ids(&listed_b), vec![b1.reference.id.as_str().to_owned()]);

    // A's private operation is readable by A alone.
    let capture_op = a
        .browser(BrowserCommand::CaptureRendererTab {
            tab: a1.reference.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(
        a.output(&capture_op).await,
        RendererOutput::Capture { .. }
    ));
    assert_eq!(
        b.operation(&capture_op).await.unwrap_err(),
        ErrorCode::PermissionDenied
    );

    // B cannot read, drive or close A's tab.
    assert_eq!(
        b.query(BrowserQuery::GetRendererTab {
            tab: a1.reference.clone()
        })
        .await
        .unwrap_err(),
        ErrorCode::PermissionDenied
    );
    for command in [
        BrowserCommand::CaptureRendererTab {
            tab: a1.reference.clone(),
        },
        BrowserCommand::CloseRendererTab {
            tab: a1.reference.clone(),
        },
        BrowserCommand::SubscribeRendererFrames {
            tab: a1.reference.clone(),
        },
    ] {
        assert_eq!(
            b.browser(command).await.unwrap_err(),
            ErrorCode::PermissionDenied
        );
    }

    // A request cannot name its own provenance: a forged one is refused
    // as a malformed request, whether it sits on the envelope or in the
    // command, and nothing of A's tab changes.
    let forged = serde_json::json!({
        "actor": "human", "surface": "desktop", "client_id": "client-a"
    });
    for place in ["envelope", "command"] {
        let forged = forged.clone();
        a.tamper(move |request| {
            if place == "envelope" {
                request["provenance"] = forged.clone();
            } else {
                request["payload"]["payload"]["payload"]["provenance"] = forged.clone();
            }
        });
        // B's own tab, closed as client-a would: the claim must not matter.
        assert_eq!(
            a.browser(BrowserCommand::CloseRendererTab {
                tab: b1.reference.clone()
            })
            .await
            .unwrap_err(),
            ErrorCode::InvalidRequest,
            "a forged provenance on the {place} is refused"
        );
    }
    // The refusals do not end the connection or touch any tab.
    assert_eq!(b.tabs(0, 4).await.0.len(), 1);
    assert_eq!(a.tabs(0, 4).await.0.len(), 2);
    // Without the forgery, A is still refused B's tab: the connection decides.
    assert_eq!(
        a.browser(BrowserCommand::CloseRendererTab {
            tab: b1.reference.clone()
        })
        .await
        .unwrap_err(),
        ErrorCode::PermissionDenied
    );

    for (peer, session) in [(&mut a, session_a), (&mut b, session_b)] {
        let id = peer
            .browser(BrowserCommand::CloseRendererSession { session })
            .await
            .unwrap();
        peer.output(&id).await;
    }
    drop((a, b));
    let _ = stop.send(true);
    server.abort();
    core.shutdown().await.unwrap();
}
