//! A client that leaves the core's Unix socket is reported to the areas that
//! asked to hear of it (ADR 0045 §10), and only that client is.
use std::sync::Arc;

use evorch_core::{backend::HostBackend, unix};
use tokio::sync::{mpsc, watch};
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};
use workbench_protocol::handshake::BuildInfo;
use workbench_protocol::ids::ClientId;

async fn connect(socket: &std::path::Path, name: &str) -> WorkbenchClient {
    WorkbenchClient::connect(
        UnixTransport::connect(socket).await.unwrap(),
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
    .unwrap()
}

#[tokio::test]
async fn a_client_that_disconnects_is_reported_once_and_the_others_are_not() {
    // Given: a core serving its socket, and a hook registered by an area.
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    let boot = BootstrappedHost::open(BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: dir.sockets().to_path_buf(),
    })
    .await
    .unwrap();
    let connections = boot.host.lock().unwrap().connections();
    let (left, mut departures) = mpsc::unbounded_channel();
    connections.register(Arc::new(move |client| {
        left.send(client.clone()).unwrap();
    }));
    let backend = Arc::new(HostBackend::new(boot.host.clone(), "hooks".into()));
    let socket = dir.sockets().join("hooks.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (stop, stopped) = watch::channel(false);
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            connections.spawn(unix::connection(stream, backend.clone(), stopped.clone()));
        }
    });
    let leaving = connect(&socket, "cli-leaving").await;
    let staying = connect(&socket, "cli-staying").await;
    assert_eq!(
        connections.connections_of(&ClientId::new("cli-leaving").unwrap()),
        1
    );

    // When: one of them disconnects.
    drop(leaving);

    // Then: the hook is told of that client, and the other stays connected.
    assert_eq!(departures.recv().await.unwrap().as_str(), "cli-leaving");
    assert_eq!(
        connections.connections_of(&ClientId::new("cli-staying").unwrap()),
        1
    );
    assert!(departures.try_recv().is_err());

    // When: the other disconnects too.
    drop(staying);

    // Then: it is reported as well.
    assert_eq!(departures.recv().await.unwrap().as_str(), "cli-staying");
    stop.send(true).unwrap();
    server.abort();
}
