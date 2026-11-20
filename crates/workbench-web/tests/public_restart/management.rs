//! Management requests through the router, with and without a verified principal.
use crate::common::{body_json, post_json, send};
use crate::support::Facade;
use axum::Router;
use axum::body::Body;
use http::{Response, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::time::Instant;
use workbench_authority::VerifiedWebPrincipal;
use workbench_authority::test_support::{self, Lease};
use workbench_web::{WebApp, WebServerConfig};

fn app(facade: &Arc<Facade>) -> Router {
    WebApp::new(&WebServerConfig::default(), Arc::clone(facade))
        .unwrap()
        .router()
}

fn principal() -> (VerifiedWebPrincipal, Lease) {
    test_support::web(
        "contract-admin",
        Instant::now() + std::time::Duration::from_secs(60),
    )
}

fn envelope(id: &str, kind: &str, operation: Value) -> Value {
    json!({"request_id": id, "session_id": "sess-1", "kind": kind,
           "payload": {"type": "lifecycle", "payload": operation}})
}

fn targets() -> Value {
    envelope(
        "req-targets",
        "query",
        json!({"type":"restart","payload":{"type":"targets"}}),
    )
}

fn restart(operation: &str, epoch: &str, revision: &str) -> Value {
    envelope(
        "req-restart",
        "command",
        json!({"type":"restart","payload":{"type":"request","payload":{
            "binding":{"target_id":"current","expected_core_epoch":epoch,
                       "target_revision":revision,"operation_id":operation},
            "confirmed":true}}}),
    )
}

async fn as_principal(
    router: &Router,
    principal: &VerifiedWebPrincipal,
    body: &Value,
) -> Response<Body> {
    let mut request = post_json(body);
    request.extensions_mut().insert(principal.clone());
    send(router, request).await
}

#[tokio::test]
async fn a_verified_principal_reads_targets_and_requests_with_the_confirmed_binding() {
    let facade = Arc::new(Facade::new());
    let router = app(&facade);
    let (principal, _lease) = principal();

    let response = as_principal(&router, &principal, &targets()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let reply = body_json(response).await;
    let listed = &reply["payload"]["payload"]["payload"]["payload"]["payload"];
    assert_eq!(listed["core_epoch"], "2");
    assert_eq!(listed["targets"][0]["id"], "current");

    let response = as_principal(&router, &principal, &restart("op-1", "2", "1")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let reply = body_json(response).await;
    assert_eq!(reply["status"], "success");
    assert_eq!(facade.restarts(), ["op-1"]);
}

#[tokio::test]
async fn a_changed_target_or_epoch_is_a_conflict_and_changes_nothing() {
    let facade = Arc::new(Facade::new());
    let router = app(&facade);
    let (principal, _lease) = principal();

    facade.set_target_revision(2);
    let response = as_principal(&router, &principal, &restart("op-1", "2", "1")).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(response).await["payload"]["code"], "conflict");

    facade.set_target_revision(1);
    facade.set_management_epoch(3);
    let response = as_principal(&router, &principal, &restart("op-2", "2", "1")).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(facade.restarts().is_empty());
}

#[tokio::test]
async fn an_unverified_request_never_reaches_the_facade() {
    let facade = Arc::new(Facade::new());
    let router = app(&facade);
    for body in [targets(), restart("op-1", "2", "1")] {
        let response = send(&router, post_json(&body)).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    assert!(facade.calls().is_empty());
}

#[tokio::test]
async fn a_revoked_principal_is_refused_on_the_next_request() {
    let facade = Arc::new(Facade::new());
    let router = app(&facade);
    let (principal, lease) = principal();
    let response = as_principal(&router, &principal, &targets()).await;
    assert_eq!(response.status(), StatusCode::OK);
    lease.revoke();
    let response = as_principal(&router, &principal, &restart("op-1", "2", "1")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(facade.calls().len(), 1);
}

#[tokio::test]
async fn a_target_is_an_id_only_never_a_path_argv_or_environment() {
    let facade = Arc::new(Facade::new());
    let router = app(&facade);
    let (principal, _lease) = principal();
    let binding = |target: &str| {
        json!({"target_id": target, "expected_core_epoch": "2",
               "target_revision": "1", "operation_id": "op-1"})
    };
    let mut bodies: Vec<Value> = ["/usr/bin/evil", "../evil", "a b", "C:\\evil.exe", ""]
        .iter()
        .map(|target| {
            envelope(
                "req-path",
                "command",
                json!({"type":"restart","payload":{"type":"request",
                    "payload":{"binding":binding(target),"confirmed":true}}}),
            )
        })
        .collect();
    for extra in ["path", "argv", "env", "exe"] {
        let mut value = binding("current");
        value[extra] = json!("x");
        bodies.push(envelope(
            "req-extra",
            "command",
            json!({"type":"restart","payload":{"type":"request",
                "payload":{"binding":value,"confirmed":true}}}),
        ));
    }
    for body in &bodies {
        let response = as_principal(&router, &principal, body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    assert!(facade.calls().is_empty());
}

#[tokio::test]
async fn a_path_in_a_management_answer_is_withheld() {
    let facade = Arc::new(Facade::new());
    let router = app(&facade);
    let (principal, _lease) = principal();
    let secret_path = "/home/user/.config/evorch/targets.toml";
    facade.fail_management_with_text(secret_path);
    let response = as_principal(&router, &principal, &targets()).await;
    let text = String::from_utf8(crate::common::body_bytes(response).await).unwrap();
    assert!(!text.contains(secret_path), "{text}");
    assert!(!text.contains("/home"), "{text}");
    assert!(text.contains(r#""code":"internal""#), "{text}");
}
