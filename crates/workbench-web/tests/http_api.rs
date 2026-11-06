//! Endpoint behaviour and browser-facing protections, driven through the
//! router with `tower::ServiceExt::oneshot` (no socket, no timers).

mod common;

use axum::Router;
use axum::body::Body;
use common::*;
use http::{Request, StatusCode, header};
use serde_json::json;
use std::sync::Arc;
use workbench_protocol::envelope::{Actor, ReplyEnvelope, ReplyResult, Surface};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_web::{StaticAsset, WebApp, WebServerConfig};

fn app_with(config: WebServerConfig, backend: &Arc<FakeBackend>) -> Router {
    WebApp::new(&config, Arc::clone(backend)).unwrap().router()
}

fn app(backend: &Arc<FakeBackend>) -> Router {
    app_with(WebServerConfig::default(), backend)
}

fn error_of(body: &serde_json::Value) -> &serde_json::Value {
    assert_eq!(body["status"], "error", "{body}");
    &body["payload"]
}

#[tokio::test]
async fn query_round_trips_a_reply_envelope() {
    let backend = Arc::new(FakeBackend::new());
    let response = send(&app(&backend), post_json(&query_request("req-1"))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let reply: ReplyEnvelope = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(reply.request_id.as_str(), "req-1");
    assert!(matches!(reply.result, ReplyResult::Success(_)));
    assert_eq!(backend.seen_count(), 1);
}

#[tokio::test]
async fn commands_reach_the_backend_with_web_provenance() {
    let backend = Arc::new(FakeBackend::new());
    let mut request = post_json(&command_request("req-2"));
    // Headers a proxy or a hostile client might add must change nothing.
    for (name, value) in [
        ("x-forwarded-for", "203.0.113.9"),
        ("x-forwarded-host", "evil.example"),
        ("forwarded", "for=203.0.113.9"),
        ("tailscale-user-login", "someone@example.com"),
        ("x-evorch-actor", "agent"),
    ] {
        request.headers_mut().insert(name, value.parse().unwrap());
    }
    let response = send(&app(&backend), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    backend.with_seen(|seen| {
        assert_eq!(seen.len(), 1);
        let provenance = &seen[0].provenance;
        assert_eq!(provenance.actor, Actor::Human);
        assert_eq!(provenance.surface, Surface::Web);
        assert_eq!(provenance.agent_run_id, None);
        assert_eq!(
            provenance.client_id.as_ref().unwrap().as_str(),
            "web-sess-1"
        );
        assert_eq!(seen[0].session.session_id, session_id());
        assert_eq!(seen[0].session.client_id.as_str(), "web-sess-1");
    });
}

#[tokio::test]
async fn client_supplied_provenance_is_refused_not_ignored() {
    let backend = Arc::new(FakeBackend::new());
    let provenance = json!({"actor": "agent", "surface": "none", "agent_run_id": "run-1"});

    let mut top_level = command_request("req-3");
    top_level["provenance"] = provenance.clone();
    let mut in_payload = command_request("req-4");
    in_payload["payload"]["payload"]["provenance"] = provenance;

    for body in [top_level, in_payload] {
        let response = send(&app(&backend), post_json(&body)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(error_of(&body)["code"], "invalid_request");
    }
    assert_eq!(backend.seen_count(), 0);
}

#[tokio::test]
async fn overlong_session_id_cannot_become_a_client_id() {
    let backend = Arc::new(FakeBackend::new());
    let mut body = query_request("req-5");
    body["session_id"] = json!("s".repeat(126));
    let response = send(&app(&backend), post_json(&body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(backend.seen_count(), 0);
}

#[tokio::test]
async fn backend_errors_map_to_http_statuses_and_hide_internals() {
    let table = [
        (ErrorCode::InvalidRequest, 400),
        (ErrorCode::UnsupportedVersion, 400),
        (ErrorCode::UnsupportedCapability, 400),
        (ErrorCode::PermissionDenied, 403),
        (ErrorCode::NotFound, 404),
        (ErrorCode::Conflict, 409),
        (ErrorCode::AlreadyResolved, 409),
        (ErrorCode::StaleRevision, 409),
        (ErrorCode::StaleEpoch, 409),
        (ErrorCode::ResyncRequired, 409),
        (ErrorCode::RestoreBlocked, 409),
        (ErrorCode::LimitExceeded, 413),
        (ErrorCode::Busy, 429),
        (ErrorCode::Draining, 503),
        (ErrorCode::StorageUnavailable, 503),
        (ErrorCode::Internal, 500),
    ];
    for (code, status) in table {
        let backend = Arc::new(FakeBackend::new());
        backend.fail_with(WorkbenchError::new(
            code,
            "database /var/lib/evorch/db.sqlite is locked",
            RetryAdvice::SameRequest,
        ));
        let response = send(&app(&backend), post_json(&command_request("req-6"))).await;
        assert_eq!(response.status().as_u16(), status, "{code:?}");
        let body = body_json(response).await;
        assert_eq!(body["request_id"], "req-6");
        let error = error_of(&body);
        assert_eq!(
            serde_json::to_value(code).unwrap(),
            error["code"],
            "{code:?}"
        );
        assert_eq!(error["retry"], "same_request");
        let leaks = error["message"].as_str().unwrap().contains("/var/lib");
        let hidden = matches!(code, ErrorCode::Internal | ErrorCode::StorageUnavailable);
        assert_eq!(leaks, !hidden, "{code:?}: {body}");
    }
}

#[tokio::test]
async fn a_reply_for_another_request_becomes_an_internal_error() {
    let backend = Arc::new(FakeBackend::new());
    backend.reply_with_wrong_request_id();
    let response = send(&app(&backend), post_json(&query_request("req-7"))).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(response).await;
    assert_eq!(body["request_id"], "req-7");
    assert_eq!(error_of(&body)["code"], "internal");
}

#[tokio::test]
async fn malformed_and_unknown_requests_are_rejected_without_the_backend() {
    let backend = Arc::new(FakeBackend::new());
    let router = app(&backend);

    let bad_json = json_post().body(Body::from("{not json")).unwrap();
    assert_eq!(
        send(&router, bad_json).await.status(),
        StatusCode::BAD_REQUEST
    );

    let mut unknown = query_request("req-8");
    unknown["payload"] = json!({"type": "from_the_future"});
    let response = send(&router, post_json(&unknown)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["request_id"], "req-8");
    assert_eq!(error_of(&body)["code"], "unsupported_capability");

    let mut bad_field = command_request("req-9");
    bad_field["payload"]["payload"]["thread"] = json!("");
    assert_eq!(
        send(&router, post_json(&bad_field)).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(backend.seen_count(), 0);
}

#[tokio::test]
async fn subscription_controls_are_not_served_over_post() {
    let backend = Arc::new(FakeBackend::new());
    let router = app(&backend);

    let subscribe = json!({
        "request_id": "req-10", "session_id": "sess-1", "kind": "control",
        "payload": {"type": "subscribe", "payload": {"scope": {"type": "all"}}}
    });
    let response = send(&router, post_json(&subscribe)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let ping = json!({
        "request_id": "req-11", "session_id": "sess-1", "kind": "control",
        "payload": {"type": "ping", "payload": {"nonce": "5"}}
    });
    let response = send(&router, post_json(&ping)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["payload"]["type"], "pong");
    assert_eq!(body["payload"]["payload"]["nonce"], "5");
    assert_eq!(backend.seen_count(), 0);
}

#[tokio::test]
async fn oversized_bodies_are_refused_before_decoding() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        max_request_bytes: 256,
        ..WebServerConfig::default()
    };
    let router = app_with(config, &backend);

    let mut big = command_request("req-12");
    big["payload"]["payload"]["padding"] = json!("x".repeat(1024));
    let response = send(&router, post_json(&big)).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = body_json(response).await;
    assert_eq!(error_of(&body)["code"], "limit_exceeded");

    // The limit also holds when the body size is not declared up front.
    let request = json_post()
        .body(Body::from_stream(futures_chunks(vec![
            vec![b'x'; 200],
            vec![b'x'; 200],
        ])))
        .unwrap();
    assert_eq!(
        send(&router, request).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(backend.seen_count(), 0);
}

fn futures_chunks(
    chunks: Vec<Vec<u8>>,
) -> impl futures_core::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    struct Chunks(std::vec::IntoIter<Vec<u8>>);
    impl futures_core::Stream for Chunks {
        type Item = Result<Vec<u8>, std::io::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Ready(self.0.next().map(Ok))
        }
    }
    Chunks(chunks.into_iter())
}

#[tokio::test]
async fn post_requires_a_json_content_type() {
    let backend = Arc::new(FakeBackend::new());
    let router = app(&backend);
    let body = serde_json::to_vec(&query_request("req-13")).unwrap();

    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
        Some("application/jsonx"),
    ] {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/v1/request")
            .header("host", HOST);
        if let Some(value) = content_type {
            builder = builder.header("content-type", value);
        }
        let response = send(&router, builder.body(Body::from(body.clone())).unwrap()).await;
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type:?}"
        );
    }
    assert_eq!(backend.seen_count(), 0);

    let accepted = Request::builder()
        .method("POST")
        .uri("/api/v1/request")
        .header("host", HOST)
        .header("content-type", "Application/JSON; charset=utf-8")
        .body(Body::from(body))
        .unwrap();
    assert_eq!(send(&router, accepted).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn host_header_must_be_allowlisted() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        allowed_hosts: vec!["Evorch.tail1234.ts.net".to_owned()],
        ..WebServerConfig::default()
    };
    let router = app_with(config, &backend);

    for host in [
        "localhost",
        "localhost:3000",
        "127.0.0.1:7878",
        "[::1]:7878",
        "evorch.tail1234.ts.net",
        "EVORCH.tail1234.ts.net:443",
    ] {
        let request = get("/api/v1/health").header("x", "y");
        let mut request = request.body(Body::empty()).unwrap();
        request.headers_mut().insert("host", host.parse().unwrap());
        assert_eq!(
            send(&router, request).await.status(),
            StatusCode::OK,
            "{host}"
        );
    }

    for host in [
        "evil.example",
        "evil.example:7878",
        "127.0.0.1.evil.example",
        "localhost.evil.example",
        "user@localhost",
        "evorch.tail1234.ts.net.evil.example",
        "0.0.0.0:7878",
    ] {
        let mut request = get("/api/v1/health").body(Body::empty()).unwrap();
        request.headers_mut().insert("host", host.parse().unwrap());
        let response = send(&router, request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host}");
    }

    // No Host, and an allowed name smuggled in a forwarding header.
    let mut missing = get("/api/v1/health").body(Body::empty()).unwrap();
    missing.headers_mut().remove("host");
    assert_eq!(send(&router, missing).await.status(), StatusCode::FORBIDDEN);

    let mut forwarded = get("/api/v1/health")
        .header("x-forwarded-host", "localhost")
        .header("forwarded", "host=localhost")
        .body(Body::empty())
        .unwrap();
    forwarded
        .headers_mut()
        .insert("host", "evil.example".parse().unwrap());
    assert_eq!(
        send(&router, forwarded).await.status(),
        StatusCode::FORBIDDEN
    );

    // Two Host headers are ambiguous.
    let mut twice = get("/api/v1/health").body(Body::empty()).unwrap();
    twice
        .headers_mut()
        .append("host", "evil.example".parse().unwrap());
    assert_eq!(send(&router, twice).await.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn origin_must_be_same_origin_or_allowlisted() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        allowed_origins: vec!["http://localhost:5173".to_owned()],
        allowed_hosts: vec!["evorch.tail1234.ts.net".to_owned()],
        ..WebServerConfig::default()
    };
    let router = app_with(config, &backend);
    let body = query_request("req-14");

    let with_origin = |host: &str, origin: Option<&str>| {
        let mut request = post_json(&body);
        request.headers_mut().insert("host", host.parse().unwrap());
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert(header::ORIGIN, origin.parse().unwrap());
        }
        request
    };

    // Allowed: no Origin (non-browser), same origin, explicit allowlist, and
    // the Tailscale Serve name over https.
    for (host, origin) in [
        (HOST, None),
        (HOST, Some("http://127.0.0.1:7878")),
        (HOST, Some("http://localhost:5173")),
        (
            "evorch.tail1234.ts.net",
            Some("https://evorch.tail1234.ts.net"),
        ),
    ] {
        let status = send(&router, with_origin(host, origin)).await.status();
        assert_eq!(status, StatusCode::OK, "{host} {origin:?}");
    }

    // Refused: null, other sites, other ports of the same host, other schemes,
    // and anything that is not an origin.
    for origin in [
        "null",
        "http://evil.example",
        "https://127.0.0.1:7878.evil.example",
        "http://127.0.0.1:9999",
        "http://localhost:7878",
        "ftp://127.0.0.1:7878",
        "http://127.0.0.1:7878/path",
        "*",
        "",
    ] {
        let response = send(&router, with_origin(HOST, Some(origin))).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin:?}");
        let body = body_json(response).await;
        assert_eq!(error_of(&body)["code"], "permission_denied");
    }
    // The refused origin is never echoed.
    assert_eq!(backend.seen_count(), 4);

    // Origin is checked on GET too.
    let get_request = get("/api/v1/health")
        .header(header::ORIGIN, "null")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&router, get_request).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn cross_site_fetch_metadata_is_refused() {
    let backend = Arc::new(FakeBackend::new());
    let router = app(&backend);
    for site in ["cross-site", "bogus"] {
        let mut post = post_json(&query_request("req-15"));
        post.headers_mut()
            .insert("sec-fetch-site", site.parse().unwrap());
        assert_eq!(send(&router, post).await.status(), StatusCode::FORBIDDEN);

        let get_request = get("/")
            .header("sec-fetch-site", site)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            send(&router, get_request).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    for site in ["same-origin", "same-site", "none"] {
        let get_request = get("/api/v1/health")
            .header("sec-fetch-site", site)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            send(&router, get_request).await.status(),
            StatusCode::OK,
            "{site}"
        );
    }
    assert_eq!(backend.seen_count(), 0);
}

#[tokio::test]
async fn no_cors_headers_and_preflight_is_not_approved() {
    let backend = Arc::new(FakeBackend::new());
    let router = app(&backend);

    let preflight = Request::builder()
        .method("OPTIONS")
        .uri("/api/v1/request")
        .header("host", HOST)
        .header(header::ORIGIN, "http://127.0.0.1:7878")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .unwrap();
    let response = send(&router, preflight).await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

    let same_origin = post_json(&query_request("req-16"));
    let mut same_origin = same_origin;
    same_origin
        .headers_mut()
        .insert(header::ORIGIN, "http://127.0.0.1:7878".parse().unwrap());
    for response in [
        response,
        send(&router, same_origin).await,
        send(&router, get("/").body(Body::empty()).unwrap()).await,
    ] {
        let names: Vec<_> = response
            .headers()
            .keys()
            .map(|n| n.as_str().to_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n.starts_with("access-control-")),
            "{names:?}"
        );
    }
}

#[tokio::test]
async fn security_headers_cover_every_kind_of_answer() {
    let backend = Arc::new(FakeBackend::new());
    let router = app(&backend);

    let answers = vec![
        (
            "api ok",
            send(&router, post_json(&query_request("req-17"))).await,
            true,
        ),
        (
            "api error",
            send(&router, get("/api/v1/request").body(Body::empty()).unwrap()).await,
            true,
        ),
        (
            "static",
            send(&router, get("/").body(Body::empty()).unwrap()).await,
            false,
        ),
        (
            "static 404",
            send(&router, get("/nope").body(Body::empty()).unwrap()).await,
            false,
        ),
        (
            "rejected host",
            send(
                &router,
                get("/api/v1/health")
                    .header("x", "y")
                    .body(Body::empty())
                    .map(|mut r| {
                        r.headers_mut()
                            .insert("host", "evil.example".parse().unwrap());
                        r
                    })
                    .unwrap(),
            )
            .await,
            true,
        ),
    ];
    for (name, response, api) in answers {
        let headers = response.headers();
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.contains("frame-ancestors 'none'"), "{name}");
        assert!(csp.contains("default-src 'none'"), "{name}");
        assert_eq!(headers["x-frame-options"], "DENY", "{name}");
        assert_eq!(headers["x-content-type-options"], "nosniff", "{name}");
        assert_eq!(headers["referrer-policy"], "no-referrer", "{name}");
        let cache = headers[header::CACHE_CONTROL].to_str().unwrap();
        assert_eq!(cache == "no-store", api, "{name}: {cache}");
    }
}

#[tokio::test]
async fn get_requests_have_no_side_effects() {
    let backend = Arc::new(FakeBackend::new());
    let router = app(&backend);
    for uri in [
        "/",
        "/index.html",
        "/api/v1/health",
        "/api/v1/request",
        "/api/v1/unknown",
        "/missing",
    ] {
        let _ = send(&router, get(uri).body(Body::empty()).unwrap()).await;
    }
    // GET with a command body in the query or body must not reach the backend.
    let tricky = get("/api/v1/request?kind=command")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&command_request("req-18")).unwrap(),
        ))
        .unwrap();
    assert_eq!(
        send(&router, tricky).await.status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    let head = Request::builder()
        .method("HEAD")
        .uri("/")
        .header("host", HOST)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&router, head).await.status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(backend.seen_count(), 0);
    assert!(backend.subscribes().is_empty());
}

#[tokio::test]
async fn health_reports_core_status_or_the_mapped_error() {
    let backend = Arc::new(FakeBackend::new());
    let response = send(
        &app(&backend),
        get("/api/v1/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["adapter"]["name"], "workbench-web");
    let hello = &body["hello"];
    assert_eq!(hello["protocol_version"], 1);
    assert_eq!(hello["server_build"]["version"], "9.9.9");
    assert_eq!(hello["instance_epoch"], "7");
    assert_eq!(hello["core"], "core-test");
    assert_eq!(hello["status"]["state"], "ready");
    let capabilities = hello["capabilities"].as_array().unwrap();
    assert!(capabilities.iter().any(|c| c == "commands.v1"));
    assert!(capabilities.iter().any(|c| c == "snapshot_events.v1"));

    backend.fail_status(WorkbenchError::new(
        ErrorCode::Draining,
        "stopping",
        RetryAdvice::AfterReconnect,
    ));
    let response = send(
        &app(&backend),
        get("/api/v1/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(error_of(&body)["code"], "draining");
}

#[tokio::test]
async fn static_assets_come_from_the_embedded_table() {
    static ASSETS: &[StaticAsset] = &[
        StaticAsset {
            path: "index.html",
            content_type: "text/html; charset=utf-8",
            body: b"<p>app</p>",
        },
        StaticAsset {
            path: "app.js",
            content_type: "text/javascript",
            body: b"1",
        },
    ];
    let backend = Arc::new(FakeBackend::new());
    let router = WebApp::with_assets(&WebServerConfig::default(), Arc::clone(&backend), ASSETS)
        .unwrap()
        .router();

    let index = send(&router, get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(index.status(), StatusCode::OK);
    assert_eq!(
        index.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    assert_eq!(body_bytes(index).await, b"<p>app</p>");

    let script = send(&router, get("/app.js").body(Body::empty()).unwrap()).await;
    assert_eq!(script.headers()[header::CONTENT_TYPE], "text/javascript");

    for uri in ["/missing", "/app.js/", "/../Cargo.toml", "/api/v1/other"] {
        let response = send(&router, get(uri).body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }

    // Without a table of its own the server serves the web workbench.
    let default_index = send(&app(&backend), get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(default_index.status(), StatusCode::OK);
    assert_eq!(
        body_bytes(default_index).await,
        workbench_web::UI_ASSETS[0].body
    );
}

#[tokio::test]
async fn requests_beyond_the_in_flight_limit_get_429() {
    let (backend, mut entered, _done) = FakeBackend::gated();
    let backend = Arc::new(backend);
    let config = WebServerConfig {
        max_in_flight_requests: 1,
        ..WebServerConfig::default()
    };
    let router = app_with(config, &backend);

    let first = tokio::spawn({
        let router = router.clone();
        async move {
            send(&router, post_json(&query_request("req-19")))
                .await
                .status()
        }
    });
    // The first request is now inside the backend and holds the only slot.
    entered.recv().await.unwrap();

    let second = send(&router, post_json(&query_request("req-20"))).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = body_json(second).await;
    assert_eq!(error_of(&body)["code"], "busy");

    backend.release(1);
    assert_eq!(first.await.unwrap(), StatusCode::OK);

    // The slot is free again.
    backend.release(1);
    let third = send(&router, post_json(&query_request("req-21"))).await;
    assert_eq!(third.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_command_finishes_even_if_the_browser_gives_up() {
    let (backend, mut entered, mut done) = FakeBackend::gated();
    let backend = Arc::new(backend);
    let router = app(&backend);

    let pending = tokio::spawn({
        let router = router.clone();
        async move { send(&router, post_json(&command_request("req-22"))).await }
    });
    entered.recv().await.unwrap();
    // The browser disconnects: the handler future is dropped.
    pending.abort();
    let _ = pending.await;

    // The backend call was spawned apart from the handler, so it still runs
    // to completion once released.
    backend.release(1);
    done.recv().await.unwrap();
    assert_eq!(
        backend.with_seen(|seen| seen[0].request.request_id.as_str().to_owned()),
        "req-22"
    );
}

#[tokio::test]
async fn api_404_and_405_use_the_common_error_format() {
    let backend = Arc::new(FakeBackend::new());
    let router = app(&backend);

    for (method, uri, status, allow) in [
        ("GET", "/api/v1/nope", 404, None),
        ("POST", "/api/v1/nope", 404, None),
        ("GET", "/api/", 404, None),
        ("GET", "/api/v1/request", 405, Some("POST")),
        ("POST", "/api/v1/health", 405, Some("GET")),
        ("POST", "/api/v1/events", 405, Some("GET")),
        ("PUT", "/api/v1/request", 405, Some("POST")),
        ("DELETE", "/api/v1/health", 405, Some("GET")),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", HOST)
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        let response = send(&router, request).await;
        assert_eq!(response.status().as_u16(), status, "{method} {uri}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json",
            "{method} {uri}"
        );
        // The router adds `HEAD` next to `GET`; only the named method matters.
        let advertised = response
            .headers()
            .get(header::ALLOW)
            .map(|v| v.to_str().unwrap().to_owned());
        match allow {
            Some(expected) => assert!(
                advertised
                    .as_deref()
                    .is_some_and(|a| a.split(',').any(|m| m.trim() == expected)),
                "{method} {uri}: {advertised:?}"
            ),
            None => assert_eq!(advertised, None, "{method} {uri}"),
        }
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = body_json(response).await;
        assert_eq!(body["status"], "error", "{method} {uri}: {body}");
        let code = if status == 404 {
            "not_found"
        } else {
            "invalid_request"
        };
        assert_eq!(body["payload"]["code"], code, "{method} {uri}");
    }
    assert_eq!(backend.seen_count(), 0);

    // Static paths keep a plain 404, which is not part of the API contract.
    let page = send(&router, get("/missing").body(Body::empty()).unwrap()).await;
    assert_eq!(page.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        page.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
}

#[tokio::test]
async fn rejections_made_before_reading_the_body_close_the_connection() {
    let backend = Arc::new(FakeBackend::new());
    let router = app(&backend);
    let wrong_type = Request::builder()
        .method("POST")
        .uri("/api/v1/request")
        .header("host", HOST)
        .header("content-type", "text/plain")
        .body(Body::from("x"))
        .unwrap();
    let mut wrong_host = post_json(&query_request("req-30"));
    wrong_host
        .headers_mut()
        .insert("host", "evil.example".parse().unwrap());
    for request in [wrong_type, wrong_host] {
        let response = send(&router, request).await;
        assert_eq!(response.headers()[header::CONNECTION], "close");
    }
    // An accepted request does not force a close.
    let ok = send(&router, post_json(&query_request("req-31"))).await;
    assert!(ok.headers().get(header::CONNECTION).is_none());
}

#[tokio::test]
async fn disconnecting_does_not_free_the_backend_slot_of_a_running_command() {
    let (backend, mut entered, mut done) = FakeBackend::gated();
    let backend = Arc::new(backend);
    let config = WebServerConfig {
        max_in_flight_requests: 1,
        ..WebServerConfig::default()
    };
    let router = app_with(config, &backend);

    let first = tokio::spawn({
        let router = router.clone();
        async move { send(&router, post_json(&command_request("req-40"))).await }
    });
    entered.recv().await.unwrap();
    // The browser gives up. The HTTP slot is gone but the command still runs.
    first.abort();
    let _ = first.await;

    // Starting over must not start a second backend call beside the first.
    let retry = send(&router, post_json(&command_request("req-41"))).await;
    assert_eq!(retry.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = body_json(retry).await;
    assert_eq!(body["payload"]["code"], "busy");
    assert_eq!(body["request_id"], "req-41");
    assert_eq!(backend.with_seen(|seen| seen.len()), 0);

    // Once the first call finishes (it releases its slot in the same task
    // step as it returns, before this receiver wakes) the slot is free.
    backend.release(1);
    done.recv().await.unwrap();
    backend.release(1);
    let again = send(&router, post_json(&command_request("req-41"))).await;
    assert_eq!(again.status(), StatusCode::OK);
    assert_eq!(backend.seen_count(), 2);
}
