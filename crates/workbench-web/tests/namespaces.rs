//! The HTTP adapter and the typed namespaces: a request the protocol does not
//! name never reaches the backend, and one it does name does.

mod common;

use axum::Router;
use common::*;
use http::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;
use workbench_web::{WebApp, WebServerConfig};

fn app(backend: &Arc<FakeBackend>) -> Router {
    WebApp::new(&WebServerConfig::default(), Arc::clone(backend))
        .unwrap()
        .router()
}

fn request(kind: &str, payload: Value) -> Value {
    json!({"request_id": "req-ns", "session_id": "sess-1", "kind": kind, "payload": payload})
}

async fn post(backend: &Arc<FakeBackend>, body: Value) -> (StatusCode, Value) {
    let response = send(&app(backend), post_json(&body)).await;
    let status = response.status();
    (status, body_json(response).await)
}

#[tokio::test]
async fn reserved_namespaces_are_unsupported_and_never_reach_the_backend() {
    let backend = Arc::new(FakeBackend::new());
    let secret = format!("sk-proj-{}", "W".repeat(48));
    for namespace in [
        "settings",
        "auth",
        "mcp",
        "teams",
        "diagnostics",
        "memory",
        "evaluation",
        "improvement",
        "pull_requests",
        "browser",
        "artifacts",
        "maintenance",
    ] {
        for kind in ["command", "query"] {
            let payload = json!({
                "type": namespace,
                "payload": {"type": "run", "payload": {"token": secret}},
            });
            let (status, body) = post(&backend, request(kind, payload)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{kind}/{namespace}");
            assert_eq!(body["status"], "error");
            assert_eq!(body["payload"]["code"], "unsupported_capability");
            assert_eq!(body["request_id"], "req-ns");
            assert!(!body.to_string().contains("sk-proj-"), "{body}");
        }
    }
    assert_eq!(backend.seen_count(), 0);
}

#[tokio::test]
async fn the_flat_version_1_shape_is_refused_not_translated() {
    let backend = Arc::new(FakeBackend::new());
    for (kind, payload) in [
        ("query", json!({"type": "get_core_status"})),
        (
            "command",
            json!({"type": "stop_chat", "payload": {"thread": "thr-1"}}),
        ),
    ] {
        let (status, body) = post(&backend, request(kind, payload)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["payload"]["code"], "unsupported_capability");
    }
    assert_eq!(backend.seen_count(), 0);
}

#[tokio::test]
async fn known_operations_in_their_namespace_reach_the_backend_once_each() {
    let backend = Arc::new(FakeBackend::new());
    for (kind, payload) in [
        (
            "command",
            json!({"type": "execution", "payload": {"type": "stop_chat", "payload": {"thread": "thr-1"}}}),
        ),
        (
            "query",
            json!({"type": "lifecycle", "payload": {"type": "get_core_status"}}),
        ),
    ] {
        let (status, body) = post(&backend, request(kind, payload)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "success");
    }
    assert_eq!(backend.seen_count(), 2);
}

#[tokio::test]
async fn defects_inside_a_known_operation_are_invalid_requests() {
    let backend = Arc::new(FakeBackend::new());
    for payload in [
        json!({"type": "execution", "payload": {"type": "stop_run", "payload": {"thread": "thr-1"}}}),
        json!({"type": "execution", "payload": {"type": "stop_chat", "payload": {"thread": 3}}}),
        json!({"type": "execution", "payload": {"type": "stop_chat", "payload": {"thread": "thr-1", "x": 1}}}),
    ] {
        let (status, body) = post(&backend, request("command", payload)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["payload"]["code"], "invalid_request", "{body}");
    }
    assert_eq!(backend.seen_count(), 0);
}
