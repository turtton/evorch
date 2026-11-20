//! Management SSE: which events and cursors a stream may carry.
use crate::common::{
    FakeBackend, SseReader, body_json, cursor, event, get, send, started, status_event,
    subscription,
};
use crate::support::Facade;
use axum::Router;
use axum::body::Body;
use http::{Response, StatusCode};
use serde_json::json;
use std::sync::Arc;
use tokio::time::Instant;
use workbench_authority::VerifiedWebPrincipal;
use workbench_authority::test_support::{self, Lease};
use workbench_protocol::events::UiEvent;
use workbench_protocol::namespaces::{LifecycleEvent, WorkspaceEvent};
use workbench_protocol::views::{ThreadState, ThreadSummary};
use workbench_protocol::wire::Revision;
use workbench_web::{BackendSubscription, WebApp, WebServerConfig, WorkbenchBackend};

fn router<B: WorkbenchBackend>(backend: &Arc<B>) -> Router {
    WebApp::new(&WebServerConfig::default(), Arc::clone(backend))
        .unwrap()
        .router()
}

fn principal() -> (VerifiedWebPrincipal, Lease) {
    test_support::web(
        "contract-admin",
        Instant::now() + std::time::Duration::from_secs(60),
    )
}

async fn open(
    router: &Router,
    uri: &str,
    principal: Option<&VerifiedWebPrincipal>,
) -> Response<Body> {
    let mut request = get(uri).body(Body::empty()).unwrap();
    if let Some(principal) = principal {
        request.extensions_mut().insert(principal.clone());
    }
    send(router, request).await
}

fn admin_changed(sequence: u64) -> workbench_protocol::events::EventEnvelope {
    event(
        cursor(7, sequence),
        UiEvent::Lifecycle(LifecycleEvent::AdminChanged {
            revision: Revision(sequence),
            restarting: false,
        }),
    )
}

fn thread_summary() -> ThreadSummary {
    ThreadSummary {
        id: crate::common::thread(),
        project: workbench_protocol::ids::ProjectId::new("p").unwrap(),
        title: "t".to_owned(),
        state: ThreadState::Idle,
        lead_thread: None,
        revision: 1.into(),
        updated_at: 1.into(),
    }
}

fn thread_event(sequence: u64) -> workbench_protocol::events::EventEnvelope {
    event(
        cursor(7, sequence),
        UiEvent::Workspace(WorkspaceEvent::ThreadChanged {
            thread: thread_summary(),
        }),
    )
}

#[tokio::test]
async fn management_notices_never_appear_on_an_unauthenticated_all_stream() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, events) = subscription(cursor(7, 10));
    backend.script_subscription(Ok(subscription));
    let response = open(&router(&backend), "/api/v1/events?scope=all", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut sse = SseReader::new(response);
    assert_eq!(
        sse.next_event().await.unwrap().event.as_deref(),
        Some("subscription_started")
    );
    events.send(admin_changed(11)).await.unwrap();
    events.send(status_event(cursor(7, 12))).await.unwrap();
    let delivered = sse.next_event().await.unwrap();
    assert_eq!(delivered.id.as_deref(), Some("7:12"), "{delivered:?}");
    assert_eq!(
        delivered.json()["event"]["payload"]["type"],
        "core_status_changed"
    );
}

#[tokio::test]
async fn a_management_stream_carries_only_lifecycle_notices() {
    let facade = Arc::new(Facade::new());
    let (subscription, events) = subscription(cursor(7, 10));
    facade.script_subscription(Ok(subscription));
    let (principal, _lease) = principal();
    let response = open(
        &router(&facade),
        "/api/v1/events?scope=management",
        Some(&principal),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut sse = SseReader::new(response);
    sse.next_event().await.unwrap();
    events.send(thread_event(11)).await.unwrap();
    events.send(admin_changed(12)).await.unwrap();
    let delivered = sse.next_event().await.unwrap();
    assert_eq!(delivered.id.as_deref(), Some("7:12"), "{delivered:?}");
    assert_eq!(
        delivered.json()["event"]["payload"]["type"],
        "admin_changed"
    );
}

#[tokio::test]
async fn a_stale_management_cursor_is_dropped_before_the_backend_sees_it() {
    let facade = Arc::new(Facade::new());
    let router = router(&facade);
    let (principal, _lease) = principal();

    let (stale, _tx) = subscription(cursor(7, 1));
    facade.script_subscription(Ok(stale));
    let response = open(
        &router,
        "/api/v1/events?scope=management&after=3:5",
        Some(&principal),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let (current, _tx2) = subscription(cursor(7, 5));
    facade.script_subscription(Ok(current));
    let response = open(
        &router,
        "/api/v1/events?scope=management&after=7:5",
        Some(&principal),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let seen = facade.subscribes();
    assert_eq!(seen[0].after, None, "the epoch-3 cursor is not forwarded");
    assert_eq!(
        seen[1].after,
        Some(cursor(7, 5)),
        "a current cursor is kept"
    );
}

#[tokio::test]
async fn a_replay_that_does_not_continue_from_the_cursor_is_not_delivered() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, _tx) = subscription(cursor(7, 9));
    backend.script_subscription(Ok(subscription));
    let response = open(
        &router(&backend),
        "/api/v1/events?scope=all&after=7:5",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let reply = body_json(response).await;
    assert_eq!(reply["payload"]["code"], "internal");
}

#[tokio::test]
async fn a_management_snapshot_with_anything_but_core_status_is_refused() {
    let facade = Arc::new(Facade::new());
    let mut start = started(cursor(7, 1));
    start.snapshot_id = Some(workbench_protocol::ids::SnapshotId::new("snap-1").unwrap());
    start.initial_page = Some(
        serde_json::from_value(json!({
            "items": [{"type": "thread", "payload": serde_json::to_value(thread_summary()).unwrap()}],
            "snapshot_revision": "1"
        }))
        .unwrap(),
    );
    let (_events_tx, events) = tokio::sync::mpsc::channel(1);
    facade.script_subscription(Ok(BackendSubscription {
        started: start,
        events,
    }));
    let (principal, _lease) = principal();
    let response = open(
        &router(&facade),
        "/api/v1/events?scope=management",
        Some(&principal),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn a_revoked_principal_ends_the_management_stream_and_cannot_reconnect() {
    let facade = Arc::new(Facade::new());
    let (subscription, _events) = subscription(cursor(7, 10));
    facade.script_subscription(Ok(subscription));
    let router = router(&facade);
    let (principal, lease) = principal();
    let response = open(&router, "/api/v1/events?scope=management", Some(&principal)).await;
    let mut sse = SseReader::new(response);
    assert_eq!(
        sse.next_event().await.unwrap().event.as_deref(),
        Some("subscription_started")
    );
    lease.revoke();
    assert!(sse.next_event().await.is_none(), "the stream ends at once");
    let again = open(
        &router,
        "/api/v1/events?scope=management&after=7:10",
        Some(&principal),
    )
    .await;
    assert_eq!(again.status(), StatusCode::FORBIDDEN);
    assert_eq!(facade.subscribes().len(), 1);
}
