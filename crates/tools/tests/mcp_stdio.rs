use sandbox::Secret;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc,
};
use tools::mcp::{
    McpErrorKind,
    approval::ApprovalLedger,
    transport::{
        stdio::{StdioShutdownClock, StdioShutdownPolicy, StdioTransport},
        *,
    },
    types::*,
};

#[derive(Debug)]
struct TestClock(bool);
#[async_trait::async_trait]
impl StdioShutdownClock for TestClock {
    async fn expired(&self, _: Duration) {
        if !self.0 {
            std::future::pending::<()>().await;
        }
    }
}
struct Limiter {
    live: Arc<AtomicUsize>,
    released: mpsc::UnboundedSender<()>,
    limit: usize,
}
struct Permit {
    live: Arc<AtomicUsize>,
    released: mpsc::UnboundedSender<()>,
}
impl McpProcessPermit for Permit {}
impl Drop for Permit {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
        let _ = self.released.send(());
    }
}
impl McpProcessLimiter for Limiter {
    fn try_acquire(&self, _: &Path) -> Result<Box<dyn McpProcessPermit>, McpErrorKind> {
        let mut live = self.live.load(Ordering::Acquire);
        loop {
            if live >= self.limit {
                return Err(McpErrorKind::ProcessLimitReached);
            }
            match self.live.compare_exchange_weak(
                live,
                live + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => live = current,
            }
        }
        Ok(Box::new(Permit {
            live: self.live.clone(),
            released: self.released.clone(),
        }))
    }
}
struct Harness {
    dir: TempDir,
    listener: UnixListener,
    limiter: Arc<Limiter>,
    released: mpsc::UnboundedReceiver<()>,
    ledger: ApprovalLedger,
    inspection: McpApprovalInspection,
}
impl Harness {
    fn new(steps: &[Value]) -> Self {
        // Keep the Unix socket path below the OS's sockaddr_un limit even when
        // the runner's TMPDIR is a long scratch path.
        let dir = tempfile::Builder::new()
            .prefix("mcp-d-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("peer.jsonl"),
            steps.iter().map(|s| format!("{s}\n")).collect::<String>(),
        )
        .unwrap();
        let listener = UnixListener::bind(root.join("control")).unwrap();
        let (released, rx) = mpsc::unbounded_channel();
        let ledger = ApprovalLedger::at_user_config_dir(root.join("consent")).unwrap();
        let inspection = McpApprovalInspection {
            scope: ApprovalScope {
                canonical_root: root.clone(),
                server_id: "fixture".into(),
            },
            source_revision: config::mcp::SourceRevision::from_sources(None, Some(b"fixture")),
            fingerprint: ApprovalFingerprint::from_projection(b"stdio-test-fixture"),
            summary: McpApprovalSummary {
                executable: Some(server_binary()),
                canonical_cwd: Some(root),
                destination: None,
                environment_keys: vec![],
                secret_references: vec![],
            },
            warnings: vec![ApprovalWarning::HostProcessOutsideSandbox],
            approved_ips: vec![],
        };
        Self {
            dir,
            listener,
            limiter: Arc::new(Limiter {
                live: Arc::new(AtomicUsize::new(0)),
                released,
                limit: 1,
            }),
            released: rx,
            ledger,
            inspection,
        }
    }
    fn server(&self) -> ResolvedMcpServer {
        let root = &self.inspection.scope.canonical_root;
        ResolvedMcpServer {
            scope: self.inspection.scope.clone(),
            fingerprint: self.inspection.fingerprint,
            transport: ResolvedMcpTransport::Stdio {
                executable: server_binary(),
                args: vec![
                    Secret::from(root.join("peer.jsonl").to_string_lossy().into_owned()),
                    Secret::from(root.join("control").to_string_lossy().into_owned()),
                ],
                cwd: root.clone(),
                env: BTreeMap::new(),
            },
            startup_timeout: Duration::from_secs(600),
            request_timeout: Duration::from_secs(600),
        }
    }
    fn approval(&self) -> ApprovalPermit {
        let preview = self.ledger.preview(&self.inspection).unwrap();
        self.ledger
            .approve(
                &ConfirmMcpApproval {
                    preview_id: preview.id,
                    scope: preview.scope,
                    expected_revision: preview.source_revision,
                    expected_fingerprint: preview.fingerprint,
                },
                &self.inspection,
            )
            .unwrap();
        self.ledger.check(&self.inspection).unwrap()
    }
    async fn connect(&self) -> Arc<StdioTransport> {
        self.connect_clock(false).await
    }
    async fn connect_clock(&self, expires: bool) -> Arc<StdioTransport> {
        self.connect_cancel(expires, CancelContext::default()).await
    }
    async fn connect_cancel(&self, expires: bool, cancel: CancelContext) -> Arc<StdioTransport> {
        StdioTransport::connect_with_policy(
            self.server(),
            self.approval(),
            self.limiter.clone(),
            cancel,
            StdioShutdownPolicy {
                stdin_grace: Duration::ZERO,
                term_grace: Duration::ZERO,
                clock: Arc::new(TestClock(expires)),
            },
        )
        .await
        .unwrap()
    }
    async fn peer(&self) -> BufReader<UnixStream> {
        let mut peer = BufReader::new(self.listener.accept().await.unwrap().0);
        event(&mut peer, "started").await;
        peer
    }
    async fn assert_released(&mut self) {
        self.released.recv().await.unwrap();
        assert_eq!(self.limiter.live.load(Ordering::Acquire), 0);
    }
}
fn server_binary() -> PathBuf {
    // Cargo exposes these variables at run time; do not content-key a checkout
    // path into the test binary with env!.
    std::env::var_os("CARGO_BIN_EXE_mcp-test-server")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // nextest may omit binary variables; its package's binaries still live
            // beside deps/, not in a shared CARGO_TARGET_DIR.
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("mcp-test-server")
        })
}
async fn event(peer: &mut BufReader<UnixStream>, name: &str) -> Value {
    loop {
        let mut line = String::new();
        assert_ne!(
            peer.read_line(&mut line).await.unwrap(),
            0,
            "peer closed before {name}"
        );
        let value: Value = serde_json::from_str(&line).unwrap();
        if value["event"] == name {
            return value;
        }
    }
}
fn request(id: i64, method: &str) -> RpcRequest {
    RpcRequest {
        jsonrpc: "2.0".into(),
        id: RpcId::Number(id),
        method: method.into(),
        params: json!({}),
    }
}
fn sent(value: impl serde::Serialize) -> Value {
    json!({"event":"send","value":value})
}
fn received(id: i64) -> Value {
    json!({"event":"receive","value":{"jsonrpc":"2.0","id":id,"result":{}}})
}
fn cancel_request(id: i64, era: ProtocolEra, phase: RequestPhase) -> CancelRequest {
    CancelRequest {
        id: RpcId::Number(id),
        era,
        phase,
        transport: TransportKind::Stdio,
        issued: true,
        in_flight: true,
    }
}
async fn golden(name: &str) {
    let path = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/mcp")
        .join(name);
    let steps: Vec<Value> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let mut h = Harness::new(&steps);
    let transport = h.connect().await;
    let mut peer = h.peer().await;
    let mut client = tools::mcp::McpClient::connect_transport(
        transport.clone(),
        "fixture".into(),
        TransportKind::Stdio,
        McpClientInfo {
            name: "evorch".into(),
            version: "0.1.0".into(),
        },
        Duration::from_secs(600),
        CancelContext::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        client.era(),
        if name.starts_with("modern") {
            ProtocolEra::Modern
        } else {
            ProtocolEra::Legacy
        }
    );
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let result = client
        .call_tool("echo", json!({"text":"hello"}))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.text(), "hello");
    event(&mut peer, "complete").await;
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    h.assert_released().await;
}
#[tokio::test]
async fn golden_modern_stdio() {
    golden("modern-stdio.jsonl").await;
}
#[tokio::test]
async fn golden_legacy_stdio() {
    golden("legacy-stdio.jsonl").await;
}

#[tokio::test]
async fn stderr_does_not_deadlock() {
    let req = request(1, "tools/list");
    let mut h = Harness::new(&[
        sent(&req),
        json!({"event":"stderr","chunks":512}),
        received(1),
        json!({"event":"close"}),
    ]);
    let transport = h.connect().await;
    let mut peer = h.peer().await;
    let response = transport
        .exchange(req, CancelContext::default())
        .await
        .unwrap();
    assert_eq!(response.frames().len(), 1);
    event(&mut peer, "stderr_drained").await;
    event(&mut peer, "complete").await;
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    h.assert_released().await;
}
async fn cancellation(legacy_initialize: bool, sticky: bool) {
    let req = request(
        1,
        if legacy_initialize {
            "initialize"
        } else {
            "tools/call"
        },
    );
    let mut steps = vec![sent(&req)];
    if !legacy_initialize {
        steps.push(sent(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}),
        ));
    }
    steps.extend([json!({"event":"stdin_closed"}), json!({"event":"close"})]);
    let mut h = Harness::new(&steps);
    let transport = h.connect().await;
    let mut peer = h.peer().await;
    let cancel = CancelContext::default();
    let task_transport = transport.clone();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move { task_transport.exchange(req, task_cancel).await });
    event(&mut peer, "read").await;
    if sticky {
        cancel.cancel();
    } else {
        transport
            .cancel(cancel_request(
                1,
                if legacy_initialize {
                    ProtocolEra::Legacy
                } else {
                    ProtocolEra::Modern
                },
                if legacy_initialize {
                    RequestPhase::Initialize
                } else {
                    RequestPhase::Request
                },
            ))
            .await
            .unwrap();
    }
    assert_eq!(
        task.await.unwrap().unwrap_err().kind,
        McpErrorKind::Cancelled
    );
    // Script rejects any initialize cancellation, and requires modern cancelled
    // to be read before EOF. Await an exact completion event, not a timeout.
    event(&mut peer, "complete").await;
    h.assert_released().await;
}
#[tokio::test]
async fn modern_stdio_sends_cancelled() {
    cancellation(false, false).await;
}
#[tokio::test]
async fn modern_stdio_context_sends_cancelled() {
    cancellation(false, true).await;
}
#[tokio::test]
async fn legacy_initialize_is_not_cancelled() {
    cancellation(true, false).await;
}

#[tokio::test]
async fn completed_ids_are_not_cancelled() {
    let req = request(1, "tools/list");
    let mut h = Harness::new(&[
        sent(&req),
        received(1),
        json!({"event":"stdin_closed"}),
        json!({"event":"close"}),
    ]);
    let transport = h.connect().await;
    let mut peer = h.peer().await;
    transport
        .exchange(req, CancelContext::default())
        .await
        .unwrap();
    transport
        .cancel(cancel_request(
            1,
            ProtocolEra::Modern,
            RequestPhase::Request,
        ))
        .await
        .unwrap();
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    event(&mut peer, "complete").await;
    h.assert_released().await;
}
#[tokio::test]
async fn unknown_call_outcome_is_not_replayed() {
    let req = request(1, "tools/call");
    let mut h = Harness::new(&[sent(&req), json!({"event":"close"})]);
    let transport = h.connect().await;
    let mut peer = h.peer().await;
    assert_eq!(
        transport
            .exchange(req, CancelContext::default())
            .await
            .unwrap_err()
            .kind,
        McpErrorKind::OutcomeUnknown
    );
    event(&mut peer, "read").await;
    event(&mut peer, "complete").await;
    h.assert_released().await;
    // An explicit replacement consumes the sole restart. It only launches: no
    // old tools/call may be sent into the replacement process.
    transport.restart(CancelContext::default()).await.unwrap();
    let mut peer = h.peer().await;
    assert_eq!(
        transport
            .restart(CancelContext::default())
            .await
            .unwrap_err()
            .kind,
        McpErrorKind::Unavailable
    );
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    let mut remaining = String::new();
    peer.read_to_string(&mut remaining).await.unwrap();
    assert!(!remaining.contains("\"event\":\"read\""));
    h.assert_released().await;
}
#[tokio::test]
async fn stdio_shutdown_reaps_descendants() {
    let mut h = Harness::new(&[
        json!({"event":"spawn_descendant"}),
        json!({"event":"gate","name":"remain_alive"}),
        json!({"event":"close"}),
    ]);
    let transport = h.connect_clock(true).await;
    let _peer = h.peer().await;
    let mut descendant = BufReader::new(h.listener.accept().await.unwrap().0);
    event(&mut descendant, "descendant_ready").await;
    assert_eq!(h.limiter.live.load(Ordering::Acquire), 1);
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    // Descendant deliberately holds the inherited stdio pipes and its own
    // control stream. EOF proves group cleanup, not merely leader reap.
    let mut byte = [0];
    assert_eq!(descendant.read(&mut byte).await.unwrap(), 0);
    h.assert_released().await;
}
#[tokio::test]
async fn owner_drop_reaps_descendants() {
    let mut h = Harness::new(&[
        json!({"event":"spawn_descendant"}),
        json!({"event":"gate","name":"remain_alive"}),
        json!({"event":"close"}),
    ]);
    let transport = h.connect_clock(true).await;
    let _peer = h.peer().await;
    let mut descendant = BufReader::new(h.listener.accept().await.unwrap().0);
    event(&mut descendant, "descendant_ready").await;
    drop(transport);
    let mut byte = [0];
    assert_eq!(descendant.read(&mut byte).await.unwrap(), 0);
    h.assert_released().await;
}
#[tokio::test]
async fn unapproved_stdio_never_spawns() {
    let mut h = Harness::new(&[json!({"event":"close"})]);
    let mut server = h.server();
    server.fingerprint = ApprovalFingerprint::from_projection(b"not-approved");
    let result = StdioTransport::connect(
        server,
        h.approval(),
        h.limiter.clone(),
        CancelContext::default(),
    )
    .await;
    assert!(matches!(result, Err(error) if error.kind == McpErrorKind::NeedsApproval));
    assert_eq!(h.limiter.live.load(Ordering::Acquire), 0);
    // No permit was even acquired, and a non-waiting admission denial must also
    // happen before spawn. Successful launch would connect to our socket.
    assert!(h.released.try_recv().is_err());
    let limited = Arc::new(Limiter {
        live: Arc::new(AtomicUsize::new(0)),
        released: h.limiter.released.clone(),
        limit: 0,
    });
    let result =
        StdioTransport::connect(h.server(), h.approval(), limited, CancelContext::default()).await;
    assert!(matches!(result, Err(error) if error.kind == McpErrorKind::ProcessLimitReached));
    assert!(h.released.try_recv().is_err());
}
#[tokio::test]
async fn legacy_server_request_is_rejected_without_call_mutex() {
    let req = request(1, "initialize");
    let mut h = Harness::new(&[
        sent(&req),
        json!({"event":"receive","value":{"jsonrpc":"2.0","id":99,"method":"sampling/createMessage","params":{}}}),
        sent(json!({"jsonrpc":"2.0","id":99,"error":{"code":-32601,"message":"Method not found"}})),
        received(1),
        json!({"event":"close"}),
    ]);
    let transport = h.connect().await;
    let mut peer = h.peer().await;
    transport
        .exchange(req, CancelContext::default())
        .await
        .unwrap();
    event(&mut peer, "complete").await;
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    h.assert_released().await;
}
#[tokio::test]
async fn malformed_and_oversized_stdout_fail_bounded() {
    for (raw, kind) in [
        (
            json!({"event":"raw","bytes":"not-json\n"}),
            McpErrorKind::Protocol,
        ),
        (
            json!({"event":"raw","repeat":MAX_FRAME_BYTES+1}),
            McpErrorKind::LimitExceeded,
        ),
    ] {
        let req = request(1, "tools/list");
        let mut h = Harness::new(&[
            sent(&req),
            raw,
            json!({"event":"stdin_closed"}),
            json!({"event":"close"}),
        ]);
        let transport = h.connect().await;
        let _peer = h.peer().await;
        assert_eq!(
            transport
                .exchange(req, CancelContext::default())
                .await
                .unwrap_err()
                .kind,
            kind
        );
        h.assert_released().await;
    }
}
#[tokio::test]
async fn unknown_ids_are_ignored_but_bounded() {
    let req = request(1, "tools/list");
    let mut h = Harness::new(&[
        sent(&req),
        received(999),
        received(1),
        json!({"event":"close"}),
    ]);
    let transport = h.connect().await;
    let mut peer = h.peer().await;
    let response = transport
        .exchange(req, CancelContext::default())
        .await
        .unwrap();
    assert_eq!(response.frames().len(), 1);
    assert!(matches!(
        &response.frames()[0],
        RpcFrame::Result {
            id: RpcId::Number(1),
            ..
        }
    ));
    event(&mut peer, "complete").await;
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    h.assert_released().await;
}
#[tokio::test]
async fn environment_is_allowlisted_and_explicit_env_wins() {
    let mut h = Harness::new(&[
        json!({"event":"environment"}),
        json!({"event":"stdin_closed"}),
        json!({"event":"close"}),
    ]);
    let mut server = h.server();
    if let ResolvedMcpTransport::Stdio { env, .. } = &mut server.transport {
        env.insert(
            "LANG".into(),
            Secret::from("explicit-test-locale".to_owned()),
        );
        env.insert(
            "MCP_EXPLICIT".into(),
            Secret::from("explicit-value".to_owned()),
        );
    }
    let transport = StdioTransport::connect_with_policy(
        server,
        h.approval(),
        h.limiter.clone(),
        CancelContext::default(),
        StdioShutdownPolicy {
            clock: Arc::new(TestClock(false)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut peer = h.peer().await;
    let state = event(&mut peer, "environment").await;
    assert_eq!(state["cwd"], h.dir.path().to_string_lossy().as_ref());
    assert_eq!(state["env"]["LANG"], "explicit-test-locale");
    assert_eq!(state["env"]["MCP_EXPLICIT"], "explicit-value");
    for key in state["env"].as_object().unwrap().keys() {
        assert!(
            tools::mcp::transport::stdio::INHERITED_ENVIRONMENT.contains(&key.as_str())
                || key == "MCP_EXPLICIT",
            "unexpected inherited key {key}"
        );
    }
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    event(&mut peer, "complete").await;
    h.assert_released().await;
}

#[tokio::test]
async fn request_future_drop_flushes_cancelled_and_reaps() {
    for keep_owner in [false, true] {
        let req = request(1, "tools/call");
        let mut h = Harness::new(&[
            sent(&req),
            sent(
                json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}),
            ),
            json!({"event":"stdin_closed"}),
            json!({"event":"close"}),
        ]);
        let transport = h.connect().await;
        let mut peer = h.peer().await;
        let task_transport = transport.clone();
        let task =
            tokio::spawn(
                async move { task_transport.exchange(req, CancelContext::default()).await },
            );
        event(&mut peer, "read").await;
        let _owner = if keep_owner {
            Some(transport)
        } else {
            drop(transport);
            None
        };
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        event(&mut peer, "complete").await;
        h.assert_released().await;
    }
}
#[test]
fn runtime_teardown_still_reaps_before_releasing_permit() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut h, transport, _peer, mut descendant) = runtime.block_on(async {
        let h = Harness::new(&[
            json!({"event":"spawn_descendant"}),
            json!({"event":"gate","name":"never_continue"}),
            json!({"event":"close"}),
        ]);
        let transport = h.connect().await;
        let peer = h.peer().await;
        let mut descendant = BufReader::new(h.listener.accept().await.unwrap().0);
        event(&mut descendant, "descendant_ready").await;
        let descendant = descendant.into_inner().into_std().unwrap();
        descendant.set_nonblocking(false).unwrap();
        (h, transport, peer, descendant)
    });
    // Aborts the supervisor rather than calling transport.shutdown/Drop first.
    // Its fallback must use OS wait without relying on that dead runtime.
    drop(runtime);
    h.released.blocking_recv().unwrap();
    assert_eq!(h.limiter.live.load(Ordering::Acquire), 0);
    let mut byte = [0];
    assert_eq!(std::io::Read::read(&mut descendant, &mut byte).unwrap(), 0);
    drop(transport);
}
#[tokio::test]
async fn frame_limit_is_inclusive_and_unknown_flood_is_bounded() {
    let req = request(1, "tools/list");
    let prefix = r#"{"jsonrpc":"2.0","id":1,"result":{"text":""#;
    let suffix = r#""}}"#;
    let raw = format!(
        "{prefix}{}{suffix}\n",
        "x".repeat(MAX_FRAME_BYTES - prefix.len() - suffix.len())
    );
    let mut h = Harness::new(&[
        sent(&req),
        json!({"event":"raw","bytes":raw}),
        json!({"event":"close"}),
    ]);
    let transport = h.connect().await;
    let mut peer = h.peer().await;
    let response = transport
        .exchange(req, CancelContext::default())
        .await
        .unwrap();
    assert_eq!(response.received_bytes, MAX_FRAME_BYTES);
    event(&mut peer, "complete").await;
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    h.assert_released().await;

    let req = request(2, "tools/list");
    let mut steps = vec![sent(&req)];
    steps.extend((0..=MAX_FRAMES_PER_EXCHANGE).map(|_| received(999)));
    steps.push(json!({"event":"close"}));
    let mut h = Harness::new(&steps);
    let transport = h.connect_clock(true).await;
    let mut peer = h.peer().await;
    // Drain the fixture's control events too; those do not belong to MCP and
    // their own backpressure must not obstruct this flood scenario.
    let drain = tokio::spawn(async move {
        let mut discard = Vec::new();
        peer.read_to_end(&mut discard).await.unwrap();
    });
    assert_eq!(
        transport
            .exchange(req, CancelContext::default())
            .await
            .unwrap_err()
            .kind,
        McpErrorKind::LimitExceeded
    );
    h.assert_released().await;
    drain.await.unwrap();
}

#[tokio::test]
async fn owner_cancellation_cleans_up_idle_and_active_sessions() {
    for (active, shared_cancel) in [(false, false), (true, false), (true, true)] {
        let req = request(1, "tools/call");
        let mut steps = Vec::new();
        if active {
            steps.push(sent(&req));
            steps.push(sent(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}})));
        }
        steps.extend([json!({"event":"stdin_closed"}), json!({"event":"close"})]);
        let mut h = Harness::new(&steps);
        let cancel = CancelContext::default();
        let transport = h.connect_cancel(false, cancel.clone()).await;
        let mut peer = h.peer().await;
        let task = if active {
            let transport = transport.clone();
            let request_cancel = if shared_cancel {
                cancel.clone()
            } else {
                CancelContext::default()
            };
            let task = tokio::spawn(async move { transport.exchange(req, request_cancel).await });
            event(&mut peer, "read").await;
            Some(task)
        } else {
            None
        };
        cancel.cancel();
        event(&mut peer, "complete").await;
        if let Some(task) = task {
            assert_eq!(
                task.await.unwrap().unwrap_err().kind,
                McpErrorKind::Cancelled
            );
        }
        h.assert_released().await;
    }
}

#[tokio::test]
async fn discover_deadline_drop_quarantines_late_stdio_response() {
    let probe = request(1, "server/discover");
    let initialize = request(2, "initialize");
    let mut h = Harness::new(&[
        sent(&probe),
        sent(&initialize),
        received(1),
        received(2),
        json!({"event":"close"}),
    ]);
    let transport = h.connect().await;
    let mut peer = h.peer().await;
    let task_transport = transport.clone();
    let task = tokio::spawn(async move {
        task_transport
            .exchange(probe, CancelContext::default())
            .await
    });
    event(&mut peer, "read").await;
    // A protocol-layer deadline drops this future in exactly the same way.
    // We test the drop boundary, not elapsed time in an external subprocess.
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let response = transport
        .exchange(initialize, CancelContext::default())
        .await
        .unwrap();
    assert_eq!(response.frames().len(), 1);
    assert!(matches!(
        &response.frames()[0],
        RpcFrame::Result {
            id: RpcId::Number(2),
            ..
        }
    ));
    event(&mut peer, "complete").await;
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    h.assert_released().await;
}

#[tokio::test]
async fn server_shutdown_does_not_cancel_the_shared_run() {
    let shared_run = CancelContext::default();
    let initialize = request(1, "initialize");
    let mut failed = Harness::new(&[
        sent(&initialize),
        json!({"event":"raw","bytes":"invalid\n"}),
        json!({"event":"close"}),
    ]);
    let failed_transport = failed.connect_cancel(false, shared_run.clone()).await;
    let _failed_peer = failed.peer().await;
    assert!(
        failed_transport
            .exchange(initialize, CancelContext::default())
            .await
            .is_err()
    );
    failed.assert_released().await;
    assert!(!shared_run.is_cancelled());
    failed_transport.restart(shared_run.clone()).await.unwrap();
    let _replacement_peer = failed.peer().await;
    assert!(!shared_run.is_cancelled());
    failed_transport
        .shutdown(ShutdownReason::InitializationFailed)
        .await
        .unwrap();
    failed.assert_released().await;
    assert!(!shared_run.is_cancelled());
    let req = request(1, "tools/list");
    let mut healthy = Harness::new(&[
        sent(&req),
        received(1),
        json!({"event":"stdin_closed"}),
        json!({"event":"close"}),
    ]);
    let transport = healthy.connect_cancel(false, shared_run.clone()).await;
    let mut peer = healthy.peer().await;
    transport.exchange(req, shared_run.clone()).await.unwrap();
    transport.shutdown(ShutdownReason::Completed).await.unwrap();
    event(&mut peer, "complete").await;
    healthy.assert_released().await;
    assert!(!shared_run.is_cancelled());
}

#[tokio::test]
async fn legacy_stdio_sends_cancelled_for_calls() {
    let initialize = request(1, "initialize");
    let call = request(2, "tools/call");
    let mut h = Harness::new(&[
        sent(&initialize),
        received(1),
        sent(&call),
        sent(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}})),
        json!({"event":"stdin_closed"}),
        json!({"event":"close"}),
    ]);
    let transport = h.connect().await;
    let mut peer = h.peer().await;
    transport
        .exchange(initialize, CancelContext::default())
        .await
        .unwrap();
    // Consume initialization's read event before waiting for the call's event.
    event(&mut peer, "read").await;
    let task_transport = transport.clone();
    let task = tokio::spawn(async move {
        task_transport
            .exchange(call, CancelContext::default())
            .await
    });
    event(&mut peer, "read").await;
    transport
        .cancel(cancel_request(
            2,
            ProtocolEra::Legacy,
            RequestPhase::Request,
        ))
        .await
        .unwrap();
    assert_eq!(
        task.await.unwrap().unwrap_err().kind,
        McpErrorKind::Cancelled
    );
    event(&mut peer, "complete").await;
    h.assert_released().await;
}
