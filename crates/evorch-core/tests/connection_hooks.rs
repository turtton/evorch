//! A client that leaves the core's Unix socket is reported to the areas that
//! asked to hear of it (ADR 0045 §10), and only that client is, once its last
//! connection ends.
use std::path::PathBuf;
use std::sync::Arc;

use evorch_core::{backend::HostBackend, unix};
use tokio::sync::{mpsc, watch};
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};
use workbench_host::connections::ConnectionHooks;
use workbench_protocol::handshake::{BuildInfo, ProtocolVersion};
use workbench_protocol::ids::ClientId;

fn config(name: &str) -> ClientConfig {
    ClientConfig::new(
        ClientId::new(name).unwrap(),
        BuildInfo {
            name: "test".into(),
            version: "1".into(),
            revision: None,
        },
    )
}

fn id(name: &str) -> ClientId {
    ClientId::new(name).unwrap()
}

/// A core serving its socket, with one hook that reports every departure.
struct Core {
    _dir: test_socket_dir::SplitDir,
    socket: PathBuf,
    connections: Arc<ConnectionHooks>,
    departures: mpsc::UnboundedReceiver<ClientId>,
    /// Each connection task reports whether it returned `Ok` once it ends.
    ended: mpsc::UnboundedReceiver<bool>,
    stop: watch::Sender<bool>,
    server: tokio::task::JoinHandle<()>,
}

impl Core {
    async fn start() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root,
            ownership_root: dir.sockets().to_path_buf(),
        })
        .await
        .unwrap();
        let connections = boot.host.lock().unwrap().connections();
        let (left, departures) = mpsc::unbounded_channel();
        connections.register(Arc::new(move |client, _| {
            left.send(client.clone()).unwrap();
        }));
        let backend = Arc::new(HostBackend::new(boot.host.clone(), "hooks".into()));
        let socket = dir.sockets().join("hooks.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (stop, stopped) = watch::channel(false);
        let (done, ended) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (backend, stopped, done) = (backend.clone(), stopped.clone(), done.clone());
                tokio::spawn(async move {
                    let result = unix::connection(stream, backend, stopped).await;
                    let _ = done.send(result.is_ok());
                });
            }
        });
        Self {
            _dir: dir,
            socket,
            connections,
            departures,
            ended,
            stop,
            server,
        }
    }

    async fn connect(&self, config: ClientConfig) -> Result<WorkbenchClient, impl std::fmt::Debug> {
        WorkbenchClient::connect(UnixTransport::connect(&self.socket).await.unwrap(), config).await
    }

    fn open(&self, name: &str) -> usize {
        self.connections.connections_of(&id(name))
    }

    fn finish(self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn a_client_that_disconnects_is_reported_once_and_the_others_are_not() {
    // Given: a core with two connected clients.
    let mut core = Core::start().await;
    let leaving = core.connect(config("cli-leaving")).await.unwrap();
    let staying = core.connect(config("cli-staying")).await.unwrap();
    assert_eq!(core.open("cli-leaving"), 1);

    // When: one of them disconnects.
    drop(leaving);

    // Then: the hook is told of that client, and the other stays connected.
    assert_eq!(
        core.departures.recv().await.unwrap().as_str(),
        "cli-leaving"
    );
    assert_eq!(core.open("cli-leaving"), 0);
    assert_eq!(core.open("cli-staying"), 1);
    assert!(core.departures.try_recv().is_err());

    // When: the other disconnects too.
    drop(staying);

    // Then: it is reported as well.
    assert_eq!(
        core.departures.recv().await.unwrap().as_str(),
        "cli-staying"
    );
    assert!(core.departures.try_recv().is_err());
    core.finish();
}

#[tokio::test]
async fn a_client_with_several_connections_is_reported_when_the_last_one_ends() {
    // Given: one client instance connected twice.
    let mut core = Core::start().await;
    let first = core.connect(config("cli-twice")).await.unwrap();
    let second = core.connect(config("cli-twice")).await.unwrap();
    assert_eq!(core.open("cli-twice"), 2);

    // When: the first connection ends and its task has finished.
    drop(first);
    assert!(core.ended.recv().await.unwrap());

    // Then: the client still has a connection and nothing was reported.
    assert_eq!(core.open("cli-twice"), 1);
    assert!(core.departures.try_recv().is_err());

    // When: the last one ends.
    drop(second);

    // Then: it is reported exactly once.
    assert_eq!(core.departures.recv().await.unwrap().as_str(), "cli-twice");
    assert_eq!(core.open("cli-twice"), 0);
    assert!(core.ended.recv().await.unwrap());
    assert!(core.departures.try_recv().is_err());
    core.finish();
}

#[tokio::test]
async fn a_rejected_handshake_is_never_a_connection_of_the_client() {
    // Given: a core, and a client that offers a protocol version it does not speak.
    let mut core = Core::start().await;
    let mut rejected = config("cli-rejected");
    rejected.supported_versions = vec![ProtocolVersion(u32::MAX)];

    // When: it connects.
    assert!(core.connect(rejected).await.is_err());
    assert!(core.ended.recv().await.unwrap());

    // Then: it was never registered, and nothing is reported for it.
    assert_eq!(core.open("cli-rejected"), 0);
    assert!(core.departures.try_recv().is_err());

    // And: a client that is accepted afterwards is still tracked and reported.
    let accepted = core.connect(config("cli-accepted")).await.unwrap();
    assert_eq!(core.open("cli-accepted"), 1);
    drop(accepted);
    assert_eq!(
        core.departures.recv().await.unwrap().as_str(),
        "cli-accepted"
    );
    assert!(core.departures.try_recv().is_err());
    core.finish();
}

#[tokio::test]
async fn a_shutdown_while_connected_reports_each_client_once() {
    // Given: two connected clients.
    let mut core = Core::start().await;
    let first = core.connect(config("cli-one")).await.unwrap();
    let second = core.connect(config("cli-two")).await.unwrap();
    assert_eq!((core.open("cli-one"), core.open("cli-two")), (1, 1));

    // When: the core shuts down with both still connected.
    core.stop.send(true).unwrap();

    // Then: both connections end and each client is reported once.
    assert!(core.ended.recv().await.unwrap());
    assert!(core.ended.recv().await.unwrap());
    let mut left = vec![
        core.departures.recv().await.unwrap(),
        core.departures.recv().await.unwrap(),
    ];
    left.sort();
    assert_eq!(left, [id("cli-one"), id("cli-two")]);
    assert_eq!((core.open("cli-one"), core.open("cli-two")), (0, 0));
    assert!(core.departures.try_recv().is_err());
    drop((first, second));
    core.finish();
}
