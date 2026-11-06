use async_trait::async_trait;
use config::mcp::SourceRevision;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tools::{
    DnsResolver, NetworkGuard, NetworkGuardError,
    mcp::{
        McpClient, McpClientInfo, McpErrorKind,
        approval::ApprovalLedger,
        transport::{api::*, http::HttpTransport},
        types::*,
    },
};
#[allow(dead_code)]
#[path = "../shared-test-support/mcp/mod.rs"]
mod golden;
struct Resolver(Mutex<Vec<IpAddr>>);
#[async_trait]
impl DnsResolver for Resolver {
    async fn resolve(&self, _: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        Ok(self.0.lock().unwrap().clone())
    }
}
struct Request {
    body: Value,
    headers: BTreeMap<String, String>,
    stream: TcpStream,
}
struct Peer {
    url: String,
    requests: mpsc::Receiver<Request>,
    task: JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn read_request(mut stream: TcpStream) -> Request {
    let mut raw = vec![];
    let mut buffer = [0; 1024];
    let (end, length) = loop {
        let n = stream.read(&mut buffer).await.unwrap();
        assert_ne!(n, 0);
        raw.extend_from_slice(&buffer[..n]);
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&raw[..end]).unwrap();
            let length = head
                .lines()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                        .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (end + 4, length);
        }
    };
    while raw.len() < end + length {
        let n = stream.read(&mut buffer).await.unwrap();
        assert_ne!(n, 0);
        raw.extend_from_slice(&buffer[..n]);
    }
    let headers = std::str::from_utf8(&raw[..end])
        .unwrap()
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Request {
        body: serde_json::from_slice(&raw[end..end + length]).unwrap(),
        headers,
        stream,
    }
}
async fn peer() -> Peer {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, requests) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let request = read_request(stream).await;
                let _ = tx.send(request).await;
            });
        }
    });
    Peer {
        url: format!("http://127.0.0.1:{}/mcp", addr.port()),
        requests,
        task,
    }
}
async fn reply(request: &mut Request, status: u16, extra: &[(&str, &str)], body: &Value) {
    let bytes = if body.is_null() {
        vec![]
    } else {
        serde_json::to_vec(body).unwrap()
    };
    let mut header = format!(
        "HTTP/1.1 {status} test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        bytes.len()
    );
    for (k, v) in extra {
        header.push_str(&format!("{k}: {v}\r\n"));
    }
    header.push_str("\r\n");
    request.stream.write_all(header.as_bytes()).await.unwrap();
    request.stream.write_all(&bytes).await.unwrap();
    request.stream.flush().await.unwrap();
}
fn permit(ips: Vec<IpAddr>) -> (tempfile::TempDir, ApprovalPermit) {
    let dir = tempfile::tempdir().unwrap();
    let store = ApprovalLedger::at_user_config_dir(dir.path().join("user")).unwrap();
    let inspection = McpApprovalInspection {
        scope: ApprovalScope {
            canonical_root: dir.path().into(),
            server_id: "fixture".into(),
        },
        source_revision: SourceRevision::from_sources(None, None),
        fingerprint: ApprovalFingerprint::from_projection(b"fixture"),
        summary: McpApprovalSummary {
            executable: None,
            canonical_cwd: None,
            destination: None,
            environment_keys: vec![],
            secret_references: vec![],
        },
        warnings: vec![ApprovalWarning::PrivateDestination],
        approved_ips: ips,
    };
    let preview = store.preview(&inspection).unwrap();
    store
        .approve(
            &ConfirmMcpApproval {
                preview_id: preview.id,
                scope: preview.scope,
                expected_revision: preview.source_revision,
                expected_fingerprint: preview.fingerprint,
            },
            &inspection,
        )
        .unwrap();
    (dir, store.check(&inspection).unwrap())
}
async fn transport(
    url: &str,
    guard: Arc<NetworkGuard>,
    permit: ApprovalPermit,
    cancel: CancelContext,
) -> Arc<HttpTransport> {
    transport_with_timeout(url, guard, permit, cancel, Duration::from_secs(60)).await
}
async fn transport_with_timeout(
    url: &str,
    guard: Arc<NetworkGuard>,
    permit: ApprovalPermit,
    cancel: CancelContext,
    request_timeout: Duration,
) -> Arc<HttpTransport> {
    let ips = guard.inspect_mcp_destination(url).await.unwrap().ips;
    let destination = guard
        .approve_mcp_destination(url, &permit, &ips)
        .await
        .unwrap();
    let server = ResolvedMcpServer {
        scope: permit.scope().clone(),
        fingerprint: permit.fingerprint(),
        transport: ResolvedMcpTransport::Http {
            url: url.to_owned().into(),
            headers: BTreeMap::new(),
            token: None,
            destination,
        },
        startup_timeout: Duration::from_secs(10),
        request_timeout,
    };
    HttpTransport::connect(guard, server, permit, cancel).unwrap()
}
fn guard() -> Arc<NetworkGuard> {
    Arc::new(NetworkGuard::with_resolver(Arc::new(Resolver(Mutex::new(
        vec!["127.0.0.1".parse().unwrap()],
    )))))
}
fn info() -> McpClientInfo {
    McpClientInfo {
        name: "evorch".into(),
        version: "0.1.0".into(),
    }
}
fn probe() -> RpcRequest {
    RpcRequest {
        jsonrpc: "2.0".into(),
        id: RpcId::Number(1),
        method: "server/discover".into(),
        params: json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"evorch","version":"0.1.0"}}}),
    }
}

async fn golden_http(name: &str) {
    let mut peer = peer().await;
    let (_dir, permit) = permit(vec!["127.0.0.1".parse().unwrap()]);
    let transport = transport(&peer.url, guard(), permit, CancelContext::default()).await;
    let path = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/mcp")
        .join(name);
    let steps = golden::load(&path);
    let (complete, mut wait_complete) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut current: Option<Request> = None;
        for step in steps {
            match step {
                golden::Step::Send { value } => {
                    let request = peer.requests.recv().await.unwrap();
                    assert_eq!(request.body, value["body"]);
                    for (name, value) in value["headers"].as_object().unwrap() {
                        assert_eq!(
                            request.headers.get(&name.to_ascii_lowercase()).unwrap(),
                            value.as_str().unwrap()
                        );
                    }
                    if value["body"]["method"] == "initialize" {
                        assert!(!request.headers.contains_key("mcp-protocol-version"));
                        assert!(!request.headers.contains_key("mcp-session-id"));
                    }
                    current = Some(request);
                }
                golden::Step::Receive { value } => {
                    let request = current.as_mut().unwrap();
                    if let Some(chunks) = value.get("chunks").and_then(Value::as_array) {
                        request.stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                        for chunk in chunks {
                            let chunk = chunk.as_str().unwrap().as_bytes();
                            request
                                .stream
                                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                                .await
                                .unwrap();
                            request.stream.write_all(chunk).await.unwrap();
                            request.stream.write_all(b"\r\n").await.unwrap();
                        }
                        request.stream.flush().await.unwrap();
                    } else {
                        let headers = value["headers"].as_object().unwrap();
                        let extra: Vec<_> = headers
                            .iter()
                            .filter(|(n, _)| !n.eq_ignore_ascii_case("content-type"))
                            .map(|(n, v)| (n.as_str(), v.as_str().unwrap()))
                            .collect();
                        reply(
                            request,
                            value["status"].as_u64().unwrap() as u16,
                            &extra,
                            &value["body"],
                        )
                        .await;
                    }
                }
                golden::Step::Gate { name } => {
                    assert_eq!(name, "client_completed_before_eof");
                    (&mut wait_complete).await.unwrap();
                }
                golden::Step::Signal { .. } => {}
                golden::Step::Close => break,
            }
        }
    });
    let mut client = McpClient::connect_transport(
        transport,
        "fixture".into(),
        TransportKind::Http,
        info(),
        Duration::from_secs(60),
        CancelContext::default(),
    )
    .await
    .unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools[0].name, "echo");
    let args = if name.starts_with("modern") {
        json!({"text":"hello","region":"west"})
    } else {
        json!({"text":"hello"})
    };
    assert_eq!(
        client.call_tool("echo", args).await.unwrap().text(),
        "hello"
    );
    let _ = complete.send(());
    server.await.unwrap();
}
#[tokio::test]
async fn sse_finishes_on_matching_result_without_eof() {
    golden_http("modern-http.json").await;
}
#[tokio::test]
async fn http_legacy_golden_headers_and_session() {
    golden_http("legacy-http.json").await;
}

async fn configured_request_deadline(timeout_seconds: u64, respond: bool) {
    // Real loopback I/O must not trigger Tokio's automatic jump to the next
    // deadline. This event-held blocking task leaves advance() as the only clock driver.
    let (release_clock, hold_clock) = oneshot::channel::<()>();
    let clock_holder = tokio::task::spawn_blocking(move || {
        let _ = hold_clock.blocking_recv();
    });
    let mut peer = peer().await;
    let (_dir, permit) = permit(vec!["127.0.0.1".parse().unwrap()]);
    let transport = transport_with_timeout(
        &peer.url,
        guard(),
        permit,
        CancelContext::default(),
        Duration::from_secs(timeout_seconds),
    )
    .await;
    let request = transport.exchange(probe(), CancelContext::default());
    tokio::pin!(request);
    let mut incoming = tokio::select! {
        result = &mut request => panic!("request completed before the fixture responded: {result:?}"),
        incoming = peer.requests.recv() => incoming.unwrap(),
    };
    tokio::time::advance(Duration::from_secs(if respond {
        70
    } else {
        timeout_seconds - 1
    }))
    .await;
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            request.as_mut(),
            cx
        )))
        .await
        .is_pending(),
        "the configured deadline must not inherit the guard's 60-second default"
    );
    if respond {
        reply(
            &mut incoming,
            200,
            &[],
            &json!({"jsonrpc":"2.0","id":1,"result":{}}),
        )
        .await;
        assert_eq!(request.await.unwrap().status, Some(200));
    } else {
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(request.await.unwrap_err().kind, McpErrorKind::Timeout);
    }
    let _ = release_clock.send(());
    clock_holder.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn configured_http_timeout_allows_a_response_after_seventy_seconds() {
    configured_request_deadline(120, true).await;
}

#[tokio::test(start_paused = true)]
async fn configured_http_timeout_rejects_a_response_past_its_deadline() {
    configured_request_deadline(120, false).await;
}

#[tokio::test(start_paused = true)]
async fn configured_http_timeout_can_be_shorter_than_the_guard_default() {
    configured_request_deadline(5, false).await;
}

#[tokio::test]
async fn local_exception_is_destination_bound() {
    let guard = guard();
    let (_dir, permit) = permit(vec!["127.0.0.1".parse().unwrap()]);
    let url = "http://127.0.0.1:3000/mcp?tenant=one";
    let ips = vec!["127.0.0.1".parse().unwrap()];
    let destination = guard
        .approve_mcp_destination(url, &permit, &ips)
        .await
        .unwrap();
    assert!(
        guard
            .check_mcp_destination(url, &destination, &permit)
            .await
            .is_ok()
    );
    for url in [
        "http://127.0.0.1:3000/other?tenant=one",
        "http://127.0.0.1:3001/mcp?tenant=one",
        "http://127.0.0.1:3000/mcp?tenant=two",
        "https://127.0.0.1:3000/mcp?tenant=one",
        "http://localhost:3000/mcp?tenant=one",
    ] {
        assert_eq!(
            guard
                .check_mcp_destination(url, &destination, &permit)
                .await
                .unwrap_err(),
            McpErrorKind::NeedsApproval
        );
    }
    let (_other, other) = self::permit(ips);
    assert_eq!(
        guard
            .check_mcp_destination("http://127.0.0.1:3000/mcp?tenant=one", &destination, &other)
            .await
            .unwrap_err(),
        McpErrorKind::NeedsApproval
    );
}
#[tokio::test]
async fn dns_rebinding_requires_approval() {
    let resolver = Arc::new(Resolver(Mutex::new(vec!["192.168.1.2".parse().unwrap()])));
    let guard = NetworkGuard::with_resolver(resolver.clone());
    let (_dir, permit) = permit(vec!["192.168.1.2".parse().unwrap()]);
    let destination = guard
        .approve_mcp_destination(
            "https://local.test/mcp",
            &permit,
            &["192.168.1.2".parse().unwrap()],
        )
        .await
        .unwrap();
    *resolver.0.lock().unwrap() = vec!["192.168.1.3".parse().unwrap()];
    assert_eq!(
        guard
            .check_mcp_destination("https://local.test/mcp", &destination, &permit)
            .await
            .unwrap_err(),
        McpErrorKind::NeedsApproval
    );
    *resolver.0.lock().unwrap() = vec!["169.254.169.254".parse().unwrap()];
    assert_eq!(
        guard
            .inspect_mcp_destination("https://local.test/mcp")
            .await
            .unwrap_err(),
        McpErrorKind::Transport
    );
}
#[tokio::test]
async fn mcp_exception_does_not_affect_web_tools() {
    let guard = guard();
    let (_dir, permit) = permit(vec!["127.0.0.1".parse().unwrap()]);
    let url = url::Url::parse("http://127.0.0.1:3000/mcp").unwrap();
    assert!(guard.check_destination(&url).await.is_err());
    let _destination = guard
        .approve_mcp_destination(url.as_str(), &permit, &["127.0.0.1".parse().unwrap()])
        .await
        .unwrap();
    assert!(guard.check_destination(&url).await.is_err());
    let resolver = Arc::new(Resolver(Mutex::new(vec![
        "169.254.169.254".parse().unwrap(),
    ])));
    let guard = NetworkGuard::with_resolver(resolver);
    assert!(guard.get("https://metadata.test/mcp").await.is_err());
    assert!(
        guard
            .post_json("https://metadata.test/mcp", Default::default(), &json!({}))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn modern_http_closes_without_cancel_notification() {
    let mut peer = peer().await;
    let (_dir, permit) = permit(vec!["127.0.0.1".parse().unwrap()]);
    let transport = transport(&peer.url, guard(), permit, CancelContext::default()).await;
    let cancel = CancelContext::default();
    let client = {
        let t = transport.clone();
        let c = cancel.clone();
        tokio::spawn(async move { t.exchange(probe(), c).await })
    };
    let mut request = peer.requests.recv().await.unwrap();
    request.stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
    request.stream.flush().await.unwrap();
    transport
        .cancel(CancelRequest {
            id: RpcId::Number(1),
            era: ProtocolEra::Modern,
            transport: TransportKind::Http,
            phase: RequestPhase::Request,
            issued: true,
            in_flight: true,
        })
        .await
        .unwrap();
    assert_eq!(
        client.await.unwrap().unwrap_err().kind,
        McpErrorKind::Cancelled
    );
    let mut b = [0; 1];
    assert_eq!(request.stream.read(&mut b).await.unwrap(), 0);
    transport
        .cancel(CancelRequest {
            id: RpcId::Number(1),
            era: ProtocolEra::Modern,
            transport: TransportKind::Http,
            phase: RequestPhase::Request,
            issued: true,
            in_flight: true,
        })
        .await
        .unwrap();
    transport.shutdown(ShutdownReason::Cancelled).await.unwrap();
}
#[tokio::test]
async fn rejects_redirects_without_following() {
    let mut peer = peer().await;
    let (_dir, permit) = permit(vec!["127.0.0.1".parse().unwrap()]);
    let transport = transport(&peer.url, guard(), permit, CancelContext::default()).await;
    let client =
        tokio::spawn(async move { transport.exchange(probe(), CancelContext::default()).await });
    let mut request = peer.requests.recv().await.unwrap();
    reply(
        &mut request,
        302,
        &[("Location", "http://169.254.169.254/secret")],
        &Value::Null,
    )
    .await;
    assert_eq!(
        client.await.unwrap().unwrap_err().kind,
        McpErrorKind::Transport
    );
}

#[tokio::test]
async fn legacy_http_cancels_same_session() {
    let mut peer = peer().await;
    let (_dir, permit) = permit(vec!["127.0.0.1".parse().unwrap()]);
    let transport = transport(&peer.url, guard(), permit, CancelContext::default()).await;
    let cancel = CancelContext::default();
    let client = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut client = McpClient::connect_transport(
                transport,
                "fixture".into(),
                TransportKind::Http,
                info(),
                Duration::from_secs(60),
                cancel,
            )
            .await
            .unwrap();
            client.call_tool("slow", json!({})).await
        })
    };
    let mut discover = peer.requests.recv().await.unwrap();
    reply(
        &mut discover,
        400,
        &[],
        &json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"legacy"}}),
    )
    .await;
    let mut initialize = peer.requests.recv().await.unwrap();
    reply(&mut initialize,200,&[("Mcp-Session-Id","same-session")],&json!({"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}})).await;
    let mut initialized = peer.requests.recv().await.unwrap();
    reply(&mut initialized, 202, &[], &Value::Null).await;
    let mut call = peer.requests.recv().await.unwrap();
    assert_eq!(call.body["method"], "tools/call");
    assert_eq!(call.headers["mcp-session-id"], "same-session");
    call.stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
    call.stream.flush().await.unwrap();
    cancel.cancel();
    let mut notification = peer.requests.recv().await.unwrap();
    assert_eq!(
        notification.body,
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3}})
    );
    assert_eq!(notification.headers["mcp-session-id"], "same-session");
    assert_eq!(notification.headers["mcp-protocol-version"], "2025-11-25");
    // The peer may wait for the cancelled stream to close before accepting the
    // side-channel notification. No timeout is used as an EOF detector.
    let mut b = [0; 1];
    assert_eq!(call.stream.read(&mut b).await.unwrap(), 0);
    reply(&mut notification, 202, &[], &Value::Null).await;
    assert_eq!(
        client.await.unwrap().unwrap_err().kind,
        McpErrorKind::Cancelled
    );
}
#[tokio::test]
async fn unsafe_destinations_are_never_approvable() {
    let guard = guard();
    let (_dir, permit) = permit(vec![]);
    for url in [
        "http://169.254.169.254/mcp",
        "http://0.0.0.0/mcp",
        "http://224.0.0.1/mcp",
        "http://[fe80::1]/mcp",
        "http://[::]/mcp",
        "http://[ff02::1]/mcp",
        "http://[::ffff:169.254.169.254]/mcp",
    ] {
        assert_eq!(
            guard
                .approve_mcp_destination(url, &permit, &[])
                .await
                .unwrap_err(),
            McpErrorKind::Transport
        );
    }
    assert_eq!(
        guard
            .approve_mcp_destination("http://127.0.0.1/mcp", &permit, &[])
            .await
            .unwrap_err(),
        McpErrorKind::NeedsApproval
    );
}

#[tokio::test]
async fn server_shutdown_does_not_cancel_the_shared_run() {
    let peer = peer().await;
    let (_dir, permit) = permit(vec!["127.0.0.1".parse().unwrap()]);
    let owner_cancel = CancelContext::default();
    let transport = transport(&peer.url, guard(), permit, owner_cancel.clone()).await;
    transport
        .shutdown(ShutdownReason::InitializationFailed)
        .await
        .unwrap();
    assert!(!owner_cancel.is_cancelled());
    assert_eq!(
        transport
            .exchange(probe(), CancelContext::default())
            .await
            .unwrap_err()
            .kind,
        McpErrorKind::Cancelled
    );
}

#[tokio::test]
async fn public_plain_http_is_rejected_even_if_approved() {
    let public = "93.184.216.34".parse().unwrap();
    let private = "127.0.0.1".parse().unwrap();
    for ips in [vec![public], vec![private, public]] {
        let guard = NetworkGuard::with_resolver(Arc::new(Resolver(Mutex::new(ips.clone()))));
        let (_dir, permit) = permit(ips.clone());
        for url in ["http://93.184.216.34/mcp", "http://mixed.test/mcp"] {
            assert_eq!(
                guard.inspect_mcp_destination(url).await.unwrap_err(),
                McpErrorKind::Transport
            );
            assert_eq!(
                guard
                    .approve_mcp_destination(url, &permit, &ips)
                    .await
                    .unwrap_err(),
                McpErrorKind::Transport
            );
        }
        // The same public answer remains admissible over certificate-checked HTTPS.
        assert!(
            guard
                .inspect_mcp_destination("https://93.184.216.34/mcp")
                .await
                .is_ok()
        );
    }
}
