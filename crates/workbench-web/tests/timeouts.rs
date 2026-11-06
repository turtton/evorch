//! Progress deadlines and shutdown draining, on virtual time: the runtime
//! advances the clock by itself once everything is waiting on a timer.

mod common;

use axum::body::Body;
use common::*;
use http::{Request, StatusCode, header};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use workbench_web::{WebApp, WebServer, WebServerConfig};

/// A request body the test feeds by hand. `started` fires on the first poll,
/// which proves the handler is reading and therefore holds its request slot.
struct ControlledBody {
    chunks: mpsc::Receiver<Vec<u8>>,
    started: Option<mpsc::UnboundedSender<()>>,
}

impl futures_core::Stream for ControlledBody {
    type Item = Result<Vec<u8>, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        self.chunks.poll_recv(cx).map(|chunk| chunk.map(Ok))
    }
}

fn controlled_post() -> (
    Request<Body>,
    mpsc::Sender<Vec<u8>>,
    mpsc::UnboundedReceiver<()>,
) {
    let (chunk_tx, chunk_rx) = mpsc::channel(8);
    let (started_tx, started_rx) = mpsc::unbounded_channel();
    let body = Body::from_stream(ControlledBody {
        chunks: chunk_rx,
        started: Some(started_tx),
    });
    (json_post().body(body).unwrap(), chunk_tx, started_rx)
}

#[tokio::test(start_paused = true)]
async fn an_unfinished_body_times_out_and_gives_its_slot_back() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        max_in_flight_requests: 1,
        body_read_timeout: Duration::from_secs(30),
        ..WebServerConfig::default()
    };
    let router = WebApp::new(&config, Arc::clone(&backend)).unwrap().router();

    let (request, chunks, mut started) = controlled_post();
    chunks
        .send(br#"{"request_id":"req-1","#.to_vec())
        .await
        .unwrap();
    let stalled = tokio::spawn({
        let router = router.clone();
        async move { send(&router, request).await }
    });
    started.recv().await.unwrap();

    // The stalled upload holds the only slot.
    let busy = send(&router, post_json(&query_request("req-2"))).await;
    assert_eq!(busy.status(), StatusCode::TOO_MANY_REQUESTS);

    // No more bytes ever arrive; the deadline passes on virtual time.
    let timed_out = stalled.await.unwrap();
    assert_eq!(timed_out.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(timed_out.headers()[header::CONNECTION], "close");
    let body = body_json(timed_out).await;
    assert_eq!(body["payload"]["code"], "invalid_request");
    drop(chunks);

    let after = send(&router, post_json(&query_request("req-3"))).await;
    assert_eq!(after.status(), StatusCode::OK);
    assert_eq!(backend.seen_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_body_that_keeps_arriving_in_time_is_accepted() {
    let backend = Arc::new(FakeBackend::new());
    let router = WebApp::new(&WebServerConfig::default(), Arc::clone(&backend))
        .unwrap()
        .router();
    let whole = serde_json::to_vec(&query_request("req-4")).unwrap();
    let (request, chunks, _started) = controlled_post();
    let (first, second) = whole.split_at(whole.len() / 2);
    chunks.send(first.to_vec()).await.unwrap();
    chunks.send(second.to_vec()).await.unwrap();
    drop(chunks);
    assert_eq!(send(&router, request).await.status(), StatusCode::OK);
}

async fn read_to_end(stream: &mut TcpStream) -> String {
    let mut received = Vec::new();
    // A reset counts as closed too.
    let _ = stream.read_to_end(&mut received).await;
    String::from_utf8_lossy(&received).into_owned()
}

fn enabled(config: WebServerConfig) -> WebServerConfig {
    WebServerConfig {
        enabled: true,
        bind: "127.0.0.1:0".parse().unwrap(),
        ..config
    }
}

async fn health_request(stream: &mut TcpStream, host: &str) {
    stream
        .write_all(format!("GET /api/v1/health HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
        .await
        .unwrap();
}

async fn read_until(stream: &mut TcpStream, needle: &str) -> String {
    let mut received = String::new();
    let mut chunk = [0u8; 2048];
    while !received.contains(needle) {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "closed before `{needle}`: {received}");
        received.push_str(&String::from_utf8_lossy(&chunk[..read]));
    }
    received
}

#[tokio::test(start_paused = true)]
async fn an_incomplete_request_head_is_dropped_and_its_connection_slot_returns() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        max_connections: 1,
        header_read_timeout: Duration::from_secs(10),
        ..WebServerConfig::default()
    };
    let server = WebServer::start(enabled(config), backend)
        .await
        .unwrap()
        .unwrap();
    let addr = server.local_addr();
    let host = addr.to_string();

    // A client that starts a request and never finishes it.
    let mut stalled = TcpStream::connect(addr).await.unwrap();
    stalled
        .write_all(b"GET /api/v1/health HTTP/1.1\r\nHost: 127.0.")
        .await
        .unwrap();
    // The server gives up on the head; whatever it says, it hangs up.
    let reply = read_to_end(&mut stalled).await;
    assert!(!reply.contains("200 OK"), "{reply}");

    // The only connection slot is free again.
    let mut next = TcpStream::connect(addr).await.unwrap();
    health_request(&mut next, &host).await;
    read_until(&mut next, "200 OK").await;

    server.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_idle_keep_alive_connection_is_closed_after_the_header_timeout() {
    let backend = Arc::new(FakeBackend::new());
    let config = WebServerConfig {
        max_connections: 1,
        ..WebServerConfig::default()
    };
    let server = WebServer::start(enabled(config), backend)
        .await
        .unwrap()
        .unwrap();
    let addr = server.local_addr();
    let host = addr.to_string();

    let mut idle = TcpStream::connect(addr).await.unwrap();
    health_request(&mut idle, &host).await;
    read_until(&mut idle, "200 OK").await;
    // Nothing more is sent. The server closes the idle connection by itself.
    read_to_end(&mut idle).await;

    let mut next = TcpStream::connect(addr).await.unwrap();
    health_request(&mut next, &host).await;
    read_until(&mut next, "200 OK").await;
    server.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_waiting_for_a_stuck_backend_but_not_for_its_command() {
    let (backend, mut entered, mut done) = FakeBackend::gated();
    let backend = Arc::new(backend);
    let config = WebServerConfig {
        shutdown_grace: Duration::from_secs(5),
        ..WebServerConfig::default()
    };
    let server = WebServer::start(enabled(config), Arc::clone(&backend))
        .await
        .unwrap()
        .unwrap();
    let addr = server.local_addr();

    let mut client = TcpStream::connect(addr).await.unwrap();
    let body = serde_json::to_string(&command_request("req-9")).unwrap();
    client
        .write_all(
            format!(
                "POST /api/v1/request HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    entered.recv().await.unwrap();

    // The backend never answers, yet shutdown returns once the grace is over.
    server.shutdown().await.unwrap();
    assert!(!read_to_end(&mut client).await.contains("200 OK"));

    // The command that was already accepted was not cancelled.
    backend.release(1);
    done.recv().await.unwrap();
    assert_eq!(backend.seen_count(), 1);
}
