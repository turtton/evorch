//! SSE behaviour, driven through the router. The response body is only polled
//! when a test reads it, so queue pressure is deterministic without sockets.

mod common;

use axum::Router;
use axum::body::Body;
use common::*;
use http::{Request, Response, StatusCode, header};
use std::sync::Arc;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::events::{CloseReason, ResyncReason, SubscriptionScope, UiEvent};
use workbench_protocol::handshake::WireLimits;
use workbench_web::{WebApp, WebServerConfig};

fn router_with(config: WebServerConfig, backend: &Arc<FakeBackend>) -> (Router, WebApp) {
    let app = WebApp::new(&config, Arc::clone(backend)).unwrap();
    (app.router(), app)
}

fn router(backend: &Arc<FakeBackend>) -> Router {
    router_with(WebServerConfig::default(), backend).0
}

async fn open(router: &Router, uri: &str) -> Response<Body> {
    send(router, get(uri).body(Body::empty()).unwrap()).await
}

#[tokio::test]
async fn snapshot_then_events_then_resync_ends_the_stream() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, events) = subscription(cursor(7, 10));
    backend.script_subscription(Ok(subscription));

    let response = open(&router(&backend), "/api/v1/events?scope=all").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let mut sse = SseReader::new(response);

    let first = sse.next_event().await.unwrap();
    assert_eq!(first.event.as_deref(), Some("subscription_started"));
    assert_eq!(first.id.as_deref(), Some("7:10"));
    assert_eq!(first.json()["high_water"]["sequence"], "10");

    // Events at or before the snapshot boundary are duplicates and are skipped.
    events.send(status_event(cursor(7, 9))).await.unwrap();
    events.send(status_event(cursor(7, 10))).await.unwrap();
    events.send(status_event(cursor(7, 11))).await.unwrap();
    events.send(status_event(cursor(7, 11))).await.unwrap();
    events.send(status_event(cursor(7, 12))).await.unwrap();
    events
        .send(event(
            cursor(7, 13),
            UiEvent::ResyncRequired {
                reason: ResyncReason::CursorTooOld,
            },
        ))
        .await
        .unwrap();
    // Anything after the terminal event is never delivered.
    let _ = events.send(status_event(cursor(7, 14))).await;

    for expected in ["7:11", "7:12", "7:13"] {
        let message = sse.next_event().await.unwrap();
        assert_eq!(message.event.as_deref(), Some("ui_event"));
        assert_eq!(message.id.as_deref(), Some(expected));
        assert_eq!(message.json()["cursor"]["sequence"], &expected[2..]);
    }
    assert!(
        sse.next_event().await.is_none(),
        "stream must end after resync"
    );
}

#[tokio::test]
async fn a_closed_subscription_ends_the_stream() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(subscription));
    let mut sse = SseReader::new(open(&router(&backend), "/api/v1/events").await);
    sse.next_event().await.unwrap();

    events
        .send(event(
            cursor(1, 1),
            UiEvent::SubscriptionClosed {
                reason: CloseReason::CoreShuttingDown,
            },
        ))
        .await
        .unwrap();
    let last = sse.next_event().await.unwrap();
    assert_eq!(last.json()["event"]["type"], "subscription_closed");
    assert!(sse.next_event().await.is_none());
}

#[tokio::test]
async fn a_backend_that_drops_the_channel_ends_the_stream_without_a_made_up_event() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(subscription));
    let mut sse = SseReader::new(open(&router(&backend), "/api/v1/events").await);
    sse.next_event().await.unwrap();
    events.send(status_event(cursor(1, 1))).await.unwrap();
    drop(events);
    assert_eq!(sse.next_event().await.unwrap().id.as_deref(), Some("1:1"));
    assert!(sse.next_event().await.is_none());
}

#[tokio::test]
async fn an_epoch_change_mid_stream_asks_for_a_resync() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, events) = subscription(cursor(7, 10));
    backend.script_subscription(Ok(subscription));
    let mut sse = SseReader::new(open(&router(&backend), "/api/v1/events").await);
    sse.next_event().await.unwrap();

    events.send(status_event(cursor(8, 1))).await.unwrap();
    let message = sse.next_event().await.unwrap();
    assert_eq!(message.json()["event"]["type"], "resync_required");
    assert_eq!(
        message.json()["event"]["payload"]["reason"],
        "epoch_changed"
    );
    // The synthesized message has no id, so Last-Event-ID stays at 7:10.
    assert_eq!(message.id, None);
    assert!(sse.next_event().await.is_none());
}

#[tokio::test]
async fn resume_cursor_comes_from_last_event_id_or_after_with_the_header_winning() {
    let cases = [
        ("/api/v1/events", None, None::<(u64, u64)>),
        ("/api/v1/events?after=7:5", None, Some((7, 5))),
        ("/api/v1/events", Some("7:42"), Some((7, 42))),
        ("/api/v1/events?after=7:5", Some("7:42"), Some((7, 42))),
    ];
    for (uri, last_event_id, expected) in cases {
        let backend = Arc::new(FakeBackend::new());
        let (subscription, _events) = subscription(cursor(7, expected.map_or(0, |c| c.1)));
        backend.script_subscription(Ok(subscription));
        let mut request = get(uri);
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id);
        }
        let response = send(&router(&backend), request.body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri} {last_event_id:?}");
        let subscribes = backend.subscribes();
        assert_eq!(subscribes.len(), 1);
        let after = subscribes[0]
            .1
            .after
            .map(|c| (c.core_epoch.get(), c.sequence.get()));
        assert_eq!(after, expected, "{uri} {last_event_id:?}");
        // The resumed stream starts with an acknowledgement carrying that cursor.
        let mut sse = SseReader::new(response);
        let first = sse.next_event().await.unwrap();
        assert_eq!(first.event.as_deref(), Some("subscription_started"));
    }
}

#[tokio::test]
async fn bad_cursors_and_scopes_are_refused_before_subscribing() {
    let backend = Arc::new(FakeBackend::new());
    let router = router(&backend);
    for uri in [
        "/api/v1/events?after=nope",
        "/api/v1/events?after=7:01",
        "/api/v1/events?scope=galaxy",
        "/api/v1/events?scope=thread:",
    ] {
        assert_eq!(
            open(&router, uri).await.status(),
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
    let bad_header = get("/api/v1/events")
        .header("last-event-id", "garbage")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&router, bad_header).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(backend.subscribes().is_empty());
}

#[tokio::test]
async fn scope_is_passed_to_the_backend() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, _events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(subscription));
    let response = open(&router(&backend), "/api/v1/events?scope=thread:thr-1").await;
    assert_eq!(response.status(), StatusCode::OK);
    let subscribes = backend.subscribes();
    assert_eq!(
        subscribes[0].1.scope,
        SubscriptionScope::Thread { thread: thread() }
    );
    assert!(subscribes[0].0.as_str().starts_with("web-sse-"));
}

#[tokio::test]
async fn subscribe_errors_use_the_error_mapping() {
    let backend = Arc::new(FakeBackend::new());
    backend.script_subscription(Err(WorkbenchError::new(
        ErrorCode::Draining,
        "core is draining",
        RetryAdvice::AfterReconnect,
    )));
    let response = open(&router(&backend), "/api/v1/events").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(body["payload"]["code"], "draining");
}

#[tokio::test]
async fn a_slow_client_gets_resync_required_and_is_cut_off() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        wire_limits: WireLimits {
            subscription_queue_events: 3,
            ..WireLimits::default()
        },
        ..WebServerConfig::default()
    };
    let (router, _app) = router_with(config, &backend);
    let (subscription, events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(subscription));

    // Nothing reads the body yet, exactly like a client that stopped reading.
    let response = open(&router, "/api/v1/events").await;
    for sequence in 1..=20 {
        // Sends fail once the adapter stopped listening; that is the point.
        if events
            .send(status_event(cursor(1, sequence)))
            .await
            .is_err()
        {
            break;
        }
    }
    // The adapter stops reading from the backend instead of buffering without
    // bound, which the backend sees as a closed channel.
    events.closed().await;

    let mut sse = SseReader::new(response);
    let mut ids = Vec::new();
    let mut last = None;
    while let Some(message) = sse.next_event().await {
        ids.push(message.id.clone());
        last = Some(message);
    }
    let last = last.unwrap();
    assert_eq!(last.json()["event"]["type"], "resync_required");
    assert_eq!(last.json()["event"]["payload"]["reason"], "slow_subscriber");
    assert_eq!(last.id, None);
    // started + at most 3 queued events + the resync; far fewer than 20.
    assert!(ids.len() <= 5, "{ids:?}");
    assert!(ids.len() >= 3, "{ids:?}");
}

#[tokio::test]
async fn a_byte_budget_overflow_is_also_slow() {
    let backend = Arc::new(FakeBackend::new());
    let defaults = WireLimits::default();
    let config = WebServerConfig {
        wire_limits: WireLimits {
            subscription_queue_bytes: defaults.max_frame_bytes,
            ..defaults
        },
        ..WebServerConfig::default()
    };
    let (router, _app) = router_with(config, &backend);
    let (subscription, events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(subscription));
    let response = open(&router, "/api/v1/events").await;

    // Each event carries ~300 KiB of title; the fourth no longer fits 1 MiB.
    let big = |sequence| {
        common::event(
            cursor(1, sequence),
            UiEvent::ThreadChanged {
                thread: workbench_protocol::views::ThreadSummary {
                    id: thread(),
                    project: workbench_protocol::ids::ProjectId::new("p").unwrap(),
                    title: "t".repeat(300 * 1024),
                    state: workbench_protocol::views::ThreadState::Idle,
                    lead_thread: None,
                    revision: 1.into(),
                    updated_at: 1.into(),
                },
            },
        )
    };
    for sequence in 1..=6 {
        if events.send(big(sequence)).await.is_err() {
            break;
        }
    }
    events.closed().await;

    let mut sse = SseReader::new(response);
    let mut count = 0;
    let mut last = None;
    while let Some(message) = sse.next_event().await {
        count += 1;
        last = Some(message);
    }
    assert!(count <= 5, "{count}");
    assert_eq!(
        last.unwrap().json()["event"]["payload"]["reason"],
        "slow_subscriber"
    );
}

#[tokio::test(start_paused = true)]
async fn idle_streams_send_keepalive_comments_without_ids() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, events) = subscription(cursor(3, 4));
    backend.script_subscription(Ok(subscription));
    let mut sse = SseReader::new(open(&router(&backend), "/api/v1/events").await);
    sse.next_event().await.unwrap();

    // Virtual time: the runtime advances to the keepalive timer by itself.
    let keepalive = sse.next().await.unwrap();
    assert_eq!(keepalive.comment.as_deref(), Some("keepalive"));
    assert_eq!(keepalive.id, None);
    assert_eq!(keepalive.event, None);

    // Keepalives did not consume a sequence: the next real event is 3:5.
    events.send(status_event(cursor(3, 5))).await.unwrap();
    let message = sse.next_event().await.unwrap();
    assert_eq!(message.id.as_deref(), Some("3:5"));
}

#[tokio::test]
async fn sse_streams_are_limited_and_slots_are_returned() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        max_sse_streams: 1,
        ..WebServerConfig::default()
    };
    let (router, _app) = router_with(config, &backend);

    let (first_sub, _first_events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(first_sub));
    let first = open(&router, "/api/v1/events").await;
    assert_eq!(first.status(), StatusCode::OK);

    let second = open(&router, "/api/v1/events").await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = body_json(second).await;
    assert_eq!(body["payload"]["code"], "busy");
    // The refused request never reached the backend.
    assert_eq!(backend.subscribes().len(), 1);

    drop(first);
    let (third_sub, _third_events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(third_sub));
    assert_eq!(
        open(&router, "/api/v1/events").await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn dropping_the_response_releases_the_backend_subscription() {
    let backend = Arc::new(FakeBackend::new());
    let (subscription, events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(subscription));
    let response = open(&router(&backend), "/api/v1/events").await;
    drop(response);
    events.closed().await;
}

#[tokio::test]
async fn shutdown_ends_open_streams_and_refuses_new_ones() {
    let backend = Arc::new(FakeBackend::new());
    let (router, app) = router_with(WebServerConfig::default(), &backend);
    let (subscription, events) = subscription(cursor(1, 0));
    backend.script_subscription(Ok(subscription));
    let mut sse = SseReader::new(open(&router, "/api/v1/events").await);
    sse.next_event().await.unwrap();

    app.shutdown_signal().trigger();
    assert!(sse.next_event().await.is_none());
    events.closed().await;

    let refused = open(&router, "/api/v1/events").await;
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(backend.subscribes().len(), 1);
}

#[tokio::test]
async fn event_stream_needs_the_same_browser_checks() {
    let backend = Arc::new(FakeBackend::new());
    let router = router(&backend);
    let request = |header: (&str, &str)| {
        Request::builder()
            .uri("/api/v1/events")
            .header("host", HOST)
            .header(header.0, header.1)
            .body(Body::empty())
            .unwrap()
    };
    for header in [
        ("origin", "http://evil.example"),
        ("origin", "null"),
        ("sec-fetch-site", "cross-site"),
    ] {
        assert_eq!(
            send(&router, request(header)).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    let wrong_host = Request::builder()
        .uri("/api/v1/events")
        .header("host", "evil.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&router, wrong_host).await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(backend.subscribes().is_empty());
}
