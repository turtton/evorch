//! Actual HTTP ingress: browser protections and claimed identity are not authentication.
mod common;
use common::FakeBackend;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use workbench_web::{WebServer, WebServerConfig};
fn graph() -> Value {
    json!({"binding":{"graph_id":"g1","restart_tx_id":"tx1","capture_id":"cap1","expected_revision":"1","expected_core_epoch":"2","owner_identity":null,"owner_generation":null,"operation_id":"op1"},"confirmed":true})
}
fn operations() -> Vec<(&'static str, Value)> {
    vec![
        (
            "query",
            json!({"type":"restart","payload":{"type":"targets"}}),
        ),
        (
            "command",
            json!({"type":"restart","payload":{"type":"request","payload":{"binding":{"target_id":"current","expected_core_epoch":"2","target_revision":"1","operation_id":"op1"},"confirmed":true}}}),
        ),
        (
            "command",
            json!({"type":"restart","payload":{"type":"cancel","payload":{"restart_id":"r1","expected_core_epoch":"2","expected_revision":"1","operation_id":"op2"}}}),
        ),
        (
            "query",
            json!({"type":"restart","payload":{"type":"status"}}),
        ),
        (
            "command",
            json!({"type":"restart","payload":{"type":"interrupt","payload":{"binding":{"restart_id":"r1","core_epoch":"2","connection_epoch":"3","run_incarnation":"inc1","blocker_identity":"opaque1","revision":"1","phase":"waiting_safe_point"},"operation_id":"op3","confirmed":true}}}),
        ),
        (
            "query",
            json!({"type":"quarantine","payload":{"type":"list"}}),
        ),
        (
            "command",
            json!({"type":"quarantine","payload":{"type":"discard","payload":graph()}}),
        ),
        (
            "command",
            json!({"type":"owned_graphs","payload":{"type":"release","payload":graph()}}),
        ),
        (
            "command",
            json!({"type":"owned_graphs","payload":{"type":"resume","payload":graph()}}),
        ),
    ]
}
async fn exchange(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
    let mut socket = TcpStream::connect(addr).await.unwrap();
    let text = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nConnection: close\r\nContent-Type: application/json\r\nTailscale-User-Login: admin@example.test\r\nX-Forwarded-User: root\r\nX-Forwarded-For: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(text.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).await.unwrap();
    String::from_utf8(bytes).unwrap()
}
#[tokio::test]
async fn anonymous_http_denies_all_nine_before_backend_even_with_identity_headers() {
    let backend = Arc::new(FakeBackend::new());
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
    assert_eq!(operations().len(), 9);
    for (i, (kind, operation)) in operations().into_iter().enumerate() {
        let body = json!({"request_id":format!("req-{i}"),"session_id":format!("claimed-admin-{i}"),"kind":kind,"payload":{"type":"lifecycle","payload":operation}});
        let response = exchange(
            server.local_addr(),
            "POST",
            "/api/v1/request",
            &body.to_string(),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "{i}: {response}");
        assert!(response.contains("permission_denied"));
    }
    let operation_status = json!({"request_id":"op-status","session_id":"claimed-admin","kind":"query","payload":{"type":"lifecycle","payload":{"type":"operation_status","payload":{"operation_id":"known-or-unknown"}}}});
    let response = exchange(
        server.local_addr(),
        "POST",
        "/api/v1/request",
        &operation_status.to_string(),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    // No authenticated snapshot, replay or live stream can be created. The
    // backend would panic if called: absence is proved by an immediate denial,
    // not by waiting for no event or by a timeout.
    for path in [
        "/api/v1/events?scope=management",
        "/api/v1/events?scope=management&after=42:1",
        "/api/v1/events?scope=management&after=42:2",
    ] {
        let response = exchange(server.local_addr(), "GET", path, "").await;
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(!response.contains("subscription_started"));
    }
    assert_eq!(backend.seen_count(), 0);
    assert!(backend.subscribes().is_empty());
    server.shutdown().await.unwrap();
}
