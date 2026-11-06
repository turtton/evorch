//! Lifecycle and limits with real loopback sockets. Every wait is on data or
//! on EOF from the peer; nothing sleeps or uses a deadline.

mod common;

use common::*;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use workbench_web::{ConfigError, WebServer, WebServerConfig, WebServerError};

fn enabled() -> WebServerConfig {
    WebServerConfig {
        enabled: true,
        bind: "127.0.0.1:0".parse().unwrap(),
        ..WebServerConfig::default()
    }
}

async fn start(config: WebServerConfig, backend: &Arc<FakeBackend>) -> WebServer {
    WebServer::start(config, Arc::clone(backend))
        .await
        .unwrap()
        .expect("server is enabled")
}

fn request_text(method: &str, path: &str, host: &str, extra: &str, body: &str) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Read one response that carries a Content-Length, leaving the connection open.
async fn read_response(stream: &mut TcpStream) -> (u16, String, String) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let (head_end, length) = loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "connection closed before a full response head");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..position]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .map_or(0, |value| value.trim().parse::<usize>().unwrap());
            break (position + 4, length);
        }
    };
    while buffer.len() < head_end + length {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "connection closed before the full body");
        buffer.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = String::from_utf8_lossy(&buffer[head_end..head_end + length]).into_owned();
    (status, head, body)
}

async fn connect(addr: SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.unwrap()
}

#[tokio::test]
async fn disabled_config_starts_nothing() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        enabled: false,
        ..enabled()
    };
    let started = WebServer::start(config, backend).await.unwrap();
    assert!(started.is_none());
}

#[tokio::test]
async fn only_loopback_addresses_can_be_bound() {
    let backend = Arc::new(FakeBackend::new());
    for bind in ["0.0.0.0:0", "[::]:0", "192.168.1.10:0", "10.0.0.1:0"] {
        let config = WebServerConfig {
            bind: bind.parse().unwrap(),
            ..enabled()
        };
        let error = WebServer::start(config.clone(), Arc::clone(&backend))
            .await
            .err()
            .unwrap_or_else(|| panic!("{bind} must be refused"));
        assert!(
            matches!(error, WebServerError::Config(ConfigError::NonLoopbackBind(addr)) if addr == config.bind),
            "{bind}: {error}"
        );
    }
}

#[tokio::test]
async fn invalid_configuration_is_reported_not_repaired() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        allowed_hosts: vec!["*".to_owned()],
        ..enabled()
    };
    let error = WebServer::start(config, backend).await.err().unwrap();
    assert!(matches!(
        error,
        WebServerError::Config(ConfigError::InvalidHost(_))
    ));
}

#[tokio::test]
async fn serves_requests_over_a_loopback_socket() {
    let backend = Arc::new(FakeBackend::new());
    let server = start(enabled(), &backend).await;
    let addr = server.local_addr();
    assert!(addr.ip().is_loopback());
    let host = addr.to_string();

    let mut stream = connect(addr).await;
    let body = serde_json::to_string(&query_request("req-1")).unwrap();
    let extra = "Content-Type: application/json\r\n";
    stream
        .write_all(request_text("POST", "/api/v1/request", &host, extra, &body).as_bytes())
        .await
        .unwrap();
    let (status, head, reply) = read_response(&mut stream).await;
    assert_eq!(status, 200);
    assert!(head.to_ascii_lowercase().contains("x-frame-options: deny"));
    assert!(reply.contains("\"request_id\":\"req-1\""), "{reply}");

    // Same connection, now a rebound Host: refused.
    stream
        .write_all(request_text("GET", "/api/v1/health", "evil.example", "", "").as_bytes())
        .await
        .unwrap();
    assert_eq!(read_response(&mut stream).await.0, 403);

    // Same-origin browser POST with the real port in Origin.
    let origin = format!("Origin: http://{host}\r\nContent-Type: application/json\r\n");
    stream
        .write_all(request_text("POST", "/api/v1/request", &host, &origin, &body).as_bytes())
        .await
        .unwrap();
    assert_eq!(read_response(&mut stream).await.0, 200);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_closes_event_streams_and_stops_accepting() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(subscription));
    let server = start(enabled(), &backend).await;
    let addr = server.local_addr();

    let mut stream = connect(addr).await;
    stream
        .write_all(request_text("GET", "/api/v1/events", &addr.to_string(), "", "").as_bytes())
        .await
        .unwrap();
    let mut received = String::new();
    let mut chunk = [0u8; 1024];
    while !received.contains("event: subscription_started") {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "stream closed early: {received}");
        received.push_str(&String::from_utf8_lossy(&chunk[..read]));
    }
    assert!(received.contains("text/event-stream"));
    events.send(status_event(cursor(1, 1))).await.unwrap();

    // Graceful shutdown must finish although an SSE stream is open.
    server.shutdown().await.unwrap();

    // The stream was closed by the server: reading ends with EOF.
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap();
    // Nothing is listening any more.
    assert!(TcpStream::connect(addr).await.is_err());
    // The backend learned that the subscriber is gone.
    events.closed().await;
}

#[tokio::test]
async fn connections_beyond_the_limit_are_closed_without_service() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        max_connections: 2,
        ..enabled()
    };
    let server = start(config, &backend).await;
    let addr = server.local_addr();
    let host = addr.to_string();

    // Two keep-alive connections that have been served hold both slots.
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut stream = connect(addr).await;
        stream
            .write_all(request_text("GET", "/api/v1/health", &host, "", "").as_bytes())
            .await
            .unwrap();
        assert_eq!(read_response(&mut stream).await.0, 200);
        held.push(stream);
    }

    let mut third = connect(addr).await;
    let _ = third
        .write_all(request_text("GET", "/api/v1/health", &host, "", "").as_bytes())
        .await;
    let mut received = Vec::new();
    // EOF or a reset: either way no HTTP response comes back.
    let _ = third.read_to_end(&mut received).await;
    assert!(
        received.is_empty(),
        "{}",
        String::from_utf8_lossy(&received)
    );

    // The held connections are unaffected.
    held[0]
        .write_all(request_text("GET", "/api/v1/health", &host, "", "").as_bytes())
        .await
        .unwrap();
    assert_eq!(read_response(&mut held[0]).await.0, 200);

    drop(held);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_the_handle_starts_shutdown() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(subscription));
    let server = start(enabled(), &backend).await;
    let addr = server.local_addr();
    let mut stream = connect(addr).await;
    stream
        .write_all(request_text("GET", "/api/v1/events", &addr.to_string(), "", "").as_bytes())
        .await
        .unwrap();
    let mut chunk = [0u8; 1024];
    let mut received = String::new();
    while !received.contains("subscription_started") {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0);
        received.push_str(&String::from_utf8_lossy(&chunk[..read]));
    }
    drop(server);
    // The open stream ends once the detached server task has wound down.
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap();
    events.closed().await;
}
