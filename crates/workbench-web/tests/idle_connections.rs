//! Real SSE admission, connection limits, disconnect and server stop use a census.
mod common;
use common::*;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use workbench_web::{WebServer, WebServerConfig};

#[tokio::test]
async fn accepted_work_retains_after_disconnect_until_its_completion_notification() {
    let (backend, mut entered, mut done) = FakeBackend::gated();
    let backend = Arc::new(backend);
    let server = WebServer::start(
        WebServerConfig {
            enabled: true,
            bind: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        },
        backend.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let clients = server.connected_clients();
    let activity = server.accepted_activity();
    let address = server.local_addr();
    let mut client = TcpStream::connect(address).await.unwrap();
    let body = command_request("held-after-disconnect").to_string();
    client
        .write_all(
            format!(
                "POST /api/v1/request HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    entered.recv().await.unwrap();
    drop(client);
    wait_count(&clients, 0).await;
    let state = activity.snapshot();
    assert_eq!((state.0, state.1), (1, false));
    assert!(!server.try_begin_idle_shutdown());
    backend.release(1);
    done.recv().await.unwrap();
    wait_count(&activity, 0).await;
    assert!(!activity.snapshot().1);
    assert_eq!(backend.seen_count(), 1);
    assert!(!server.try_begin_idle_shutdown_if(|| false));
    assert!(server.try_begin_idle_shutdown());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn sse_retains_but_a_refused_connection_does_not_increment_the_census() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(subscription));
    let server = WebServer::start(
        WebServerConfig {
            enabled: true,
            bind: "127.0.0.1:0".parse().unwrap(),
            max_connections: 1,
            ..Default::default()
        },
        backend,
    )
    .await
    .unwrap()
    .unwrap();
    let census = server.connected_clients();
    assert_eq!(census.snapshot().0, 0);
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /api/v1/events HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\n\r\n",
                server.local_addr(),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    let mut bytes = [0; 1024];
    while !response.contains("event: subscription_started") {
        let count = stream.read(&mut bytes).await.unwrap();
        assert_ne!(count, 0);
        response.push_str(&String::from_utf8_lossy(&bytes[..count]));
    }
    wait_count(&census, 1).await;
    assert!(
        !server.try_begin_idle_shutdown(),
        "an SSE response is a live client"
    );
    let mut refused = TcpStream::connect(server.local_addr()).await.unwrap();
    assert_eq!(refused.read(&mut bytes).await.unwrap(), 0);
    assert_eq!(census.snapshot().0, 1);
    drop(stream);
    wait_count(&census, 0).await;
    events.closed().await;
    let replacement = TcpStream::connect(server.local_addr()).await.unwrap();
    wait_count(&census, 1).await;
    server.shutdown().await.unwrap();
    wait_count(&census, 0).await;
    drop(replacement);
}

async fn wait_count(activity: &workbench_web::ActivityObserver, expected: usize) {
    loop {
        let (count, _, generation) = activity.snapshot();
        if count == expected {
            return;
        }
        activity.changed(generation).await;
    }
}
