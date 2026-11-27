//! A connection that ends says why in the core's log (`tracing`), with the task
//! that ended first and its I/O error. Every wait is an event: the connection
//! task reports through a channel once it has returned.
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use evorch_core::{backend::HostBackend, unix};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};
use workbench_protocol::handshake::BuildInfo;
use workbench_protocol::ids::ClientId;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
    fn line_with(&self, parts: &[&str]) -> Option<String> {
        self.lines()
            .into_iter()
            .find(|line| parts.iter().all(|part| line.contains(part)))
    }
}

/// Route this test's `tracing` output into a buffer, for the test's thread only.
fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    (captured, tracing::subscriber::set_default(subscriber))
}

struct Core {
    _dir: test_socket_dir::SplitDir,
    socket: std::path::PathBuf,
    ended: mpsc::UnboundedReceiver<io::Result<()>>,
    handlers: mpsc::UnboundedReceiver<tokio::task::JoinHandle<()>>,
    _stop: watch::Sender<bool>,
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
        let backend = Arc::new(HostBackend::new(boot.host.clone(), "diagnostics".into()));
        let socket = dir.sockets().join("diagnostics.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (stop, stopped) = watch::channel(false);
        let (done, ended) = mpsc::unbounded_channel();
        let (spawned, handlers) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (backend, stopped, done) = (backend.clone(), stopped.clone(), done.clone());
                let handler = tokio::spawn(async move {
                    let _ = done.send(unix::connection(stream, backend, stopped).await);
                });
                let _ = spawned.send(handler);
            }
        });
        Self {
            _dir: dir,
            socket,
            ended,
            handlers,
            _stop: stop,
            server,
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.server.abort();
    }
}

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

#[tokio::test]
async fn a_peer_that_leaves_is_recorded_with_the_task_that_noticed_it() {
    // Given: a connected client.
    let (log, _guard) = capture();
    let mut core = Core::start().await;
    let client = WorkbenchClient::connect(
        UnixTransport::connect(&core.socket).await.unwrap(),
        config("cli-leaving"),
    )
    .await
    .unwrap();

    // When: the client goes away and the core's connection has finished.
    drop(client);
    assert!(core.ended.recv().await.unwrap().is_ok());

    // Then: the log names the client, the reader that saw the EOF, and the end.
    assert!(
        log.line_with(&["handshake accepted", "cli-leaving"])
            .is_some()
    );
    assert!(
        log.line_with(&["the peer closed the connection", "role=", "reader"])
            .is_some(),
        "{:?}",
        log.lines()
    );
    assert!(
        log.line_with(&["connection closed", "elapsed_ms", "core_stopping=false"])
            .is_some()
    );
}

#[tokio::test]
async fn a_protocol_violation_is_recorded_with_its_io_error() {
    // Given: a socket that is not yet a client.
    let (log, _guard) = capture();
    let mut core = Core::start().await;
    let mut raw = tokio::net::UnixStream::connect(&core.socket).await.unwrap();

    // When: it sends a frame length no frame may have, and the core has finished with it.
    raw.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
    let result = core.ended.recv().await.unwrap();

    // Then: the connection ends with InvalidData and the log carries the kind and text.
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert!(
        log.line_with(&[
            "connection closed with an error",
            "InvalidData",
            "invalid frame length"
        ])
        .is_some(),
        "{:?}",
        log.lines()
    );
}

fn conn_of(line: &str) -> &str {
    let rest = &line[line.find("conn=").expect("a line of a connection") + 5..];
    rest.split_whitespace().next().unwrap()
}

#[tokio::test]
async fn a_handler_that_is_cancelled_is_recorded_under_its_connection_number() {
    // Given: a connected client whose connection the core has numbered.
    let (log, _guard) = capture();
    let mut core = Core::start().await;
    let _client = WorkbenchClient::connect(
        UnixTransport::connect(&core.socket).await.unwrap(),
        config("cli-cancelled"),
    )
    .await
    .unwrap();
    let handler = core.handlers.recv().await.unwrap();
    let accepted = log
        .line_with(&["connection accepted"])
        .expect("the connection was numbered when it was accepted");

    // When: the handler task is cancelled (or panics) instead of returning.
    handler.abort();
    assert!(handler.await.unwrap_err().is_cancelled());

    // Then: the record that it ended without a result carries the same number.
    let lost = log
        .line_with(&["connection ended without a result"])
        .expect("the lost handler was recorded");
    assert_eq!(conn_of(&lost), conn_of(&accepted));
}
