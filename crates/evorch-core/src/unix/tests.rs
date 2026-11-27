//! One-worker socket fixtures with isolated stores and event-driven gates.
use super::*;
use crate::blocking::testing::Gate;
use std::{future::Future, path::Path, pin::Pin, sync::Mutex, task::Poll};
use tokio::sync::oneshot;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};
use workbench_protocol::{
    commands::Command,
    events::{Subscribe, SubscriptionScope},
    ids::*,
    namespaces::{ConversationCommand, LifecycleQuery},
    queries::Query,
    wire::Nonce,
};

struct Fixture {
    dir: test_socket_dir::SplitDir,
    host: BootstrappedHost,
    backend: Arc<HostBackend>,
}
impl Fixture {
    async fn new() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let host = BootstrappedHost::open(BootstrapOptions {
            config_dir: dir.path().into(),
            project_root: dir.path().into(),
            ownership_root: dir.sockets().into(),
        })
        .await
        .unwrap();
        let backend = Arc::new(HostBackend::new(host.host.clone(), "offload-test".into()));
        Self { dir, host, backend }
    }
    fn hold(&self, job: Job, panic: bool) -> Gate {
        self.backend.blocking_hooks.lock().unwrap().hold(job, panic)
    }
    async fn connect(&self, name: &str) -> Running {
        let socket = self.dir.sockets().join(name);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let stream = UnixStream::connect(&socket).await.unwrap();
        let (server_stream, _) = listener.accept().await.unwrap();
        let (stop, stopped) = watch::channel(false);
        let server = tokio::spawn(connection(server_stream, self.backend.clone(), stopped));
        let peer = Peer::hello(stream, name).await;
        Running { peer, stop, server }
    }
}
struct Running {
    peer: Peer,
    stop: watch::Sender<bool>,
    server: tokio::task::JoinHandle<io::Result<()>>,
}
struct Peer(UnixStream);
impl Peer {
    async fn connect(path: &Path, name: &str) -> Self {
        Self::hello(UnixStream::connect(path).await.unwrap(), name).await
    }
    async fn hello(stream: UnixStream, name: &str) -> Self {
        let mut peer = Self(stream);
        peer.send(&ClientHello {
            supported_versions: vec![ProtocolVersion::CURRENT],
            required_capabilities: vec![],
            optional_capabilities: Capability::IMPLEMENTED
                .iter()
                .copied()
                .map(Into::into)
                .collect(),
            client_build: build(),
            client_instance_id: ClientId::new(name).unwrap(),
            desired_core: None,
            local_surface: None,
        })
        .await;
        let hello: HandshakeReply =
            serde_json::from_slice(&payload(&mut peer.0, 1024 * 1024).await.unwrap()).unwrap();
        assert!(matches!(hello, HandshakeReply::Accepted(_)));
        peer
    }
    async fn send(&mut self, value: &impl serde::Serialize) {
        self.0
            .write_all(&encode_frame(value, 1024 * 1024).unwrap())
            .await
            .unwrap();
    }
    async fn request(&mut self, id: &str, body: RequestBody) {
        self.send(&RequestEnvelope {
            request_id: RequestId::new(id).unwrap(),
            session_id: SessionId::new("offload-session").unwrap(),
            body,
        })
        .await;
    }
    async fn frame(&mut self) -> ServerFrame {
        serde_json::from_slice(&payload(&mut self.0, 1024 * 1024).await.unwrap()).unwrap()
    }
    async fn reply(&mut self, id: &str) -> ReplyResult {
        loop {
            if let ServerFrame::Reply(reply) = self.frame().await {
                assert_eq!(reply.request_id.as_str(), id);
                return reply.result;
            }
        }
    }
    async fn subscribe(&mut self, id: &str) -> SubscriptionId {
        self.request(id, subscribe()).await;
        let ReplyResult::Success(ReplyPayload::SubscriptionStarted(started)) = self.reply(id).await
        else {
            panic!("subscription rejected");
        };
        started.subscription_id
    }
    async fn eof(&mut self) {
        // Queued frames are allowed; explicit socket EOF proves connection end.
        loop {
            match payload(&mut self.0, 1024 * 1024).await {
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                other => panic!("unexpected socket result: {other:?}"),
            }
        }
    }
}
fn build() -> BuildInfo {
    BuildInfo {
        name: "test".into(),
        version: "1".into(),
        revision: None,
    }
}
fn subscribe() -> RequestBody {
    RequestBody::Control(Control::Subscribe(Subscribe {
        scope: SubscriptionScope::All,
        after: None,
    }))
}
fn query() -> RequestBody {
    RequestBody::Query(Query::Lifecycle(LifecycleQuery::GetCoreStatus))
}
async fn pending<T>(future: Pin<&mut impl Future<Output = T>>) {
    let mut future = future;
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx)))
            .await
            .is_pending()
    );
}

// A + B: deliberately do not run the unchanged lifecycle host polling.
#[tokio::test]
async fn locked_host_does_not_block_other_socket_ping_or_cleanup() {
    let mut f = Fixture::new().await;
    let mut blocked = Vec::new();
    for i in 0..3 {
        blocked.push(f.connect(&format!("blocked-{i}")).await);
    }
    let mut cleanup = f.connect("cleanup").await;
    let first = cleanup.peer.subscribe("first").await;
    let second = cleanup.peer.subscribe("second").await;
    let (locked, held) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let host = f.backend.host.clone();
    let holder = std::thread::spawn(move || {
        let _guard = host.lock().unwrap();
        locked.send(()).unwrap();
        let _ = released.recv();
    });
    held.await.unwrap();
    for (i, conn) in blocked.iter_mut().enumerate() {
        let gate = f.hold(Job::Request, false);
        gate.release.send(()).unwrap();
        conn.peer.request(&format!("query-{i}"), query()).await;
        gate.started.await.unwrap();
    }
    // Pong traverses reader/connection/writer while ordinary jobs wait on host.
    cleanup
        .peer
        .request(
            "ping",
            RequestBody::Control(Control::Ping { nonce: Nonce(42) }),
        )
        .await;
    assert!(matches!(
        cleanup.peer.reply("ping").await,
        ReplyResult::Success(ReplyPayload::Pong { .. })
    ));
    cleanup
        .peer
        .request(
            "unsubscribe",
            RequestBody::Control(Control::Unsubscribe {
                subscription_id: first,
            }),
        )
        .await;
    assert!(matches!(
        cleanup.peer.reply("unsubscribe").await,
        ReplyResult::Success(ReplyPayload::Unsubscribed { .. })
    ));
    cleanup.stop.send(true).unwrap();
    let mut goodbye = false;
    loop {
        let bytes = match payload(&mut cleanup.peer.0, 1024 * 1024).await {
            Ok(bytes) => bytes,
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
                break;
            }
        };
        let frame: ServerFrame = serde_json::from_slice(&bytes).unwrap();
        if let ServerFrame::Event(event) = frame {
            assert!(!goodbye, "no old event may follow goodbye");
            if event.event.ends_subscription() {
                assert_eq!(event.subscription_id, second);
                goodbye = true;
            }
        }
    }
    assert!(goodbye);
    cleanup.server.await.unwrap().unwrap();
    release.send(()).unwrap();
    holder.join().unwrap();
    for (i, mut conn) in blocked.into_iter().enumerate() {
        assert!(matches!(
            conn.peer.reply(&format!("query-{i}")).await,
            ReplyResult::Success(_)
        ));
        conn.stop.send(true).unwrap();
        conn.server.await.unwrap().unwrap();
    }
    f.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn ping_keeps_session_and_draining_checks() {
    let mut f = Fixture::new().await;
    let mut conn = f.connect("ping-contract").await;
    let ping = || RequestBody::Control(Control::Ping { nonce: Nonce(1) });
    conn.peer.request("initial", ping()).await;
    assert!(matches!(
        conn.peer.reply("initial").await,
        ReplyResult::Success(ReplyPayload::Pong { .. })
    ));
    conn.peer
        .send(&RequestEnvelope {
            request_id: RequestId::new("wrong-session").unwrap(),
            session_id: SessionId::new("other-session").unwrap(),
            body: ping(),
        })
        .await;
    assert!(
        matches!(conn.peer.reply("wrong-session").await, ReplyResult::Error(e) if e.code == ErrorCode::PermissionDenied)
    );
    f.backend.close();
    conn.peer.request("draining", ping()).await;
    assert!(
        matches!(conn.peer.reply("draining").await, ReplyResult::Error(e) if e.code == ErrorCode::Busy)
    );
    conn.stop.send(true).unwrap();
    conn.server.await.unwrap().unwrap();
    f.host.shutdown().await.unwrap();
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    (captured, tracing::subscriber::set_default(subscriber))
}

#[tokio::test]
async fn blocking_panics_close_the_connection_and_record_job_and_connection() {
    let (logs, _guard) = capture();
    let mut f = Fixture::new().await;
    for job in [Job::Subscribe, Job::Unsubscribe, Job::Goodbye] {
        let mut conn = f.connect(job.name()).await;
        let gate = f.hold(job, true);
        match job {
            Job::Subscribe => conn.peer.request("panic", subscribe()).await,
            Job::Unsubscribe => {
                conn.peer
                    .request(
                        "panic",
                        RequestBody::Control(Control::Unsubscribe {
                            subscription_id: SubscriptionId::new("missing").unwrap(),
                        }),
                    )
                    .await
            }
            Job::Goodbye => conn.stop.send(true).unwrap(),
            _ => unreachable!(),
        }
        gate.started.await.unwrap();
        gate.release.send(()).unwrap();
        assert!(conn.server.await.unwrap_err().is_panic());
        // No Internal reply or panic payload reaches the wire.
        assert!(payload(&mut conn.peer.0, 1024 * 1024).await.is_err());
    }
    // Exercise OutcomeUnknown using the real client and a sent command.
    let socket = f.dir.sockets().join("command-panic");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let transport = workbench_client::UnixTransport::connect(&socket)
        .await
        .unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    let (_stop, stopped) = watch::channel(false);
    let server = tokio::spawn(connection(stream, f.backend.clone(), stopped));
    let mut config =
        workbench_client::ClientConfig::new(ClientId::new("panic-client").unwrap(), build());
    config.optional_capabilities.push(Capability::Conversation);
    let client = workbench_client::WorkbenchClient::connect(transport, config)
        .await
        .unwrap();
    let gate = f.hold(Job::Request, true);
    let command = client.command(Command::Conversation(ConversationCommand::ResolveProject {
        root: f.dir.path().to_string_lossy().into_owned(),
    }));
    tokio::pin!(command);
    pending(command.as_mut()).await;
    gate.started.await.unwrap();
    gate.release.send(()).unwrap();
    assert!(matches!(
        command.await,
        Err(workbench_client::ClientError::Transport(
            workbench_client::TransportError::OutcomeUnknown { .. }
        ))
    ));
    assert!(server.await.unwrap_err().is_panic());
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    for job in [Job::Request, Job::Subscribe, Job::Unsubscribe, Job::Goodbye] {
        let line = text
            .lines()
            .find(|line| {
                line.contains("a connection blocking job failed")
                    && line.contains(&format!("job=\"{}\"", job.name()))
            })
            .unwrap_or_else(|| panic!("missing {} failure: {text}", job.name()));
        assert!(
            line.contains("conn=")
                && line.contains("panicked=true")
                && line.contains("cancelled=false"),
            "{line}"
        );
    }
    f.host.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn goodbye_deadline_does_not_detach_notice_and_pumps_end_first() {
    let mut f = Fixture::new().await;
    let (output, mut frames) = mpsc::channel(64);
    let (finish, _finishing) = oneshot::channel();
    let (_flush, flushed) = oneshot::channel();
    let (_stop, stopped) = watch::channel(false);
    let log = ConnectionLog::new(&stopped);
    let mut pumps = JoinSet::new();
    let (pump_ready, ready) = oneshot::channel();
    let (pump_ended, ended) = oneshot::channel();
    struct OnDrop(Option<oneshot::Sender<()>>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }
    pumps.spawn(async move {
        let _done = OnDrop(Some(pump_ended));
        pump_ready.send(()).unwrap();
        std::future::pending::<()>().await;
        Ok(false)
    });
    ready.await.unwrap();
    let id = SubscriptionId::new("deadline-sub").unwrap();
    let gate = f.hold(Job::Goodbye, false);
    let goodbye = say_goodbye(
        &f.backend,
        &output,
        finish,
        flushed,
        vec![id],
        &mut pumps,
        &log,
    );
    tokio::pin!(goodbye);
    tokio::select! {
        result = gate.started => result.unwrap(),
        _ = goodbye.as_mut() => panic!("goodbye ended before notice release"),
    }
    ended.await.unwrap();
    // Blocking jobs prevent automatic virtual clock advancement.
    tokio::time::advance(GOODBYE_BOUND + std::time::Duration::from_millis(1)).await;
    pending(goodbye.as_mut()).await;
    gate.release.send(()).unwrap();
    goodbye.await;
    gate.completed.await.unwrap();
    assert!(
        frames.try_recv().is_err(),
        "expired delivery must not enqueue a late notice"
    );
    f.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_disconnected_client_does_not_detach_an_inflight_job() {
    let mut f = Fixture::new().await;
    for job in [Job::Request, Job::Subscribe, Job::Unsubscribe] {
        let mut conn = f.connect(job.name()).await;
        let gate = f.hold(job, false);
        let body = match job {
            Job::Request => query(),
            Job::Subscribe => subscribe(),
            Job::Unsubscribe => RequestBody::Control(Control::Unsubscribe {
                subscription_id: SubscriptionId::new("missing").unwrap(),
            }),
            _ => unreachable!(),
        };
        conn.peer.request("disconnect", body).await;
        gate.started.await.unwrap();
        drop(conn.peer);
        conn.stop.send(true).unwrap();
        pending(Pin::new(&mut conn.server)).await;
        gate.release.send(()).unwrap();
        conn.server.await.unwrap().unwrap();
        gate.completed.await.unwrap();
    }
    f.host.shutdown().await.unwrap();
}

#[cfg(target_os = "linux")]
mod drain;

#[tokio::test]
async fn ping_does_not_overtake_an_earlier_job_on_the_same_connection() {
    let mut f = Fixture::new().await;
    let mut conn = f.connect("serial").await;
    let gate = f.hold(Job::Request, false);
    conn.peer.request("first", query()).await;
    gate.started.await.unwrap();
    conn.peer
        .request(
            "second",
            RequestBody::Control(Control::Ping { nonce: Nonce(2) }),
        )
        .await;
    // Even after the reader can enqueue both, no request is dispatched while
    // the first job owns the connection.
    {
        let response = conn.peer.reply("first");
        tokio::pin!(response);
        pending(response.as_mut()).await;
        gate.release.send(()).unwrap();
        assert!(matches!(response.await, ReplyResult::Success(_)));
    }
    assert!(matches!(
        conn.peer.reply("second").await,
        ReplyResult::Success(ReplyPayload::Pong { nonce: Nonce(2) })
    ));
    conn.stop.send(true).unwrap();
    conn.server.await.unwrap().unwrap();
    f.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_job_is_logged_and_remains_a_connection_task_failure() {
    let (logs, _guard) = capture();
    let (_stop, stopped) = watch::channel(false);
    let log = ConnectionLog::new(&stopped);
    let job = tokio::spawn(std::future::pending::<()>());
    job.abort();
    let error = job.await.unwrap_err();
    assert!(error.is_cancelled());
    let connection = tokio::spawn(async move {
        log.blocking_result::<()>(Job::Goodbye, Err(error));
    });
    assert!(connection.await.unwrap_err().is_panic());
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let line = text
        .lines()
        .find(|line| line.contains("a connection blocking job failed"))
        .unwrap();
    assert!(
        line.contains("conn=")
            && line.contains("job=\"goodbye\"")
            && line.contains("panicked=false")
            && line.contains("cancelled=true"),
        "{line}"
    );
}
