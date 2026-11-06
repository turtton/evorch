use agents::Role;
use async_trait::async_trait;
use event_bus::EventBus;
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, RunConfig, RuntimeError,
    mcp::{McpRunService, PrepareMcpRun, ProjectMcpService},
};
use serde_json::json;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tools::{
    NetworkGuard, ToolExecutor,
    mcp::{
        McpError, McpErrorKind, McpValueField,
        approval::{ApprovalLedger, inspect_stdio},
        transport::*,
        types::*,
    },
};

#[derive(Default)]
struct Factory(AtomicUsize);
#[async_trait]
impl McpTransportFactory for Factory {
    async fn connect(
        &self,
        server: ResolvedMcpServer,
        permit: ApprovalPermit,
        limiter: Arc<dyn McpProcessLimiter>,
        _: CancelContext,
    ) -> Result<Arc<dyn McpTransport>, McpError> {
        assert!(!permit.is_revoked());
        assert_eq!(permit.scope(), &server.scope);
        assert_eq!(permit.fingerprint(), server.fingerprint);
        let slot = match server.transport {
            ResolvedMcpTransport::Stdio { .. } => Some(
                limiter
                    .try_acquire(&server.scope.canonical_root)
                    .map_err(error)?,
            ),
            _ => None,
        };
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Transport(Mutex::new(slot))))
    }
}
struct Transport(Mutex<Option<Box<dyn McpProcessPermit>>>);
#[async_trait]
impl McpTransport for Transport {
    async fn exchange(
        &self,
        request: RpcRequest,
        _: CancelContext,
    ) -> Result<McpResponseEnvelope, McpError> {
        let result = match request.method.as_str() {
            "server/discover" => {
                json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"})
            }
            "tools/list" => {
                json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"read","description":"remote definition","inputSchema":{"type":"object"}}]})
            }
            "tools/call" => {
                json!({"resultType":"complete","content":[{"type":"text","text":"mcp-ok"}]})
            }
            _ => panic!("unexpected method"),
        };
        McpResponseEnvelope::new(
            Some(200),
            McpResponseHeaders::default(),
            vec![RpcFrame::Result {
                jsonrpc: "2.0".into(),
                id: request.id,
                result,
            }],
            128,
        )
        .map_err(error)
    }
    async fn notify(&self, _: RpcNotification) -> Result<(), McpError> {
        Ok(())
    }
    async fn respond(&self, _: RpcFrame) -> Result<(), McpError> {
        panic!("no server request")
    }
    async fn cancel(&self, _: CancelRequest) -> Result<(), McpError> {
        Ok(())
    }
    async fn shutdown(&self, _: ShutdownReason) -> Result<(), McpError> {
        self.0.lock().unwrap().take();
        Ok(())
    }
}
fn error(kind: McpErrorKind) -> McpError {
    McpError {
        server: "fixture".into(),
        method: "tools/call",
        request_id: None,
        kind,
    }
}
fn project(extra: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".evorch")).unwrap();
    let command = std::env::current_exe().unwrap().canonicalize().unwrap();
    std::fs::write(
        root.path().join(".evorch/mcp.toml"),
        format!(
            "version = 1\n[servers.fixture]\ntype = \"stdio\"\ncommand = {command:?}\n{extra}\n"
        ),
    )
    .unwrap();
    root
}
fn ledger(root: &Path) -> Arc<ApprovalLedger> {
    Arc::new(ApprovalLedger::at_user_config_dir(root.join("user-config")).unwrap())
}
fn approve(root: &Path, ledger: &ApprovalLedger) {
    let config = config::mcp::load_project(root).unwrap();
    let root = root.canonicalize().unwrap();
    let definition = config.servers["fixture"].definition.as_ref().unwrap();
    let config::mcp::McpTransportDefinition::Stdio { command, .. } = &definition.transport else {
        panic!("stdio fixture")
    };
    let command =
        config::mcp::expand::expand_template(command, &|name| std::env::var(name).ok()).unwrap();
    let inspection = inspect_stdio(
        ApprovalScope {
            canonical_root: root.clone(),
            server_id: "fixture".into(),
        },
        config.revision,
        definition,
        &Path::new(&command).canonicalize().unwrap(),
        &root,
    )
    .unwrap();
    let preview = ledger.preview(&inspection).unwrap();
    ledger
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
}

#[tokio::test]
async fn revocation_denies_existing_session_without_changing_snapshot() {
    let root = project("");
    let approvals = ledger(root.path());
    approve(root.path(), &approvals);
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    let lease = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    let snapshot = serde_json::to_vec(lease.snapshot()).unwrap();
    let bus = Arc::new(EventBus::new(64));
    let executor = lease
        .register_executor(&ToolExecutor::new(bus.clone()), bus)
        .unwrap();
    let context = tools::ToolExecutionContext {
        run_id: "test".into(),
        ..Default::default()
    };
    let first = executor
        .execute(&context, "mcp__fixture__read", "first", json!({}))
        .await
        .unwrap();
    assert!(!first.is_error);
    approvals
        .revoke(&ApprovalScope {
            canonical_root: root.path().canonicalize().unwrap(),
            server_id: "fixture".into(),
        })
        .unwrap();
    let refused = executor
        .execute(&context, "mcp__fixture__read", "revoked", json!({}))
        .await
        .unwrap();
    assert!(refused.is_error);
    assert_eq!(refused.detail.unwrap()["class"], "needs_approval");
    assert_eq!(serde_json::to_vec(lease.snapshot()).unwrap(), snapshot);
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
    lease.shutdown(ShutdownReason::ApprovalRevoked).await;
}

#[tokio::test]
async fn unknown_call_is_not_replayed_and_next_explicit_call_restarts_once() {
    let root = project("");
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("sh"))
        .find(|path| path.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    // Each generation handles deterministic fixture IDs. The first dies after
    // receiving a call; the replacement later dies too, exercising the one-restart
    // budget without any timer-based synchronization.
    let script = r#"
if [ -e marker ]; then first=0; else first=1; printf x > marker; fi
printf 'start\n' >> starts
IFS= read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}}'
IFS= read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"read","inputSchema":{"type":"object"}}],"ttlMs":0,"cacheScope":"private"}}'
IFS= read -r request
printf 'call\n' >> calls
if [ "$first" = 1 ]; then exit 0; fi
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","content":[{"type":"text","text":"mcp-ok"}]}}'
IFS= read -r request
printf 'call\n' >> calls
"#;
    std::fs::write(
        root.path().join(".evorch/mcp.toml"),
        format!(
            "version=1\n[servers.fixture]\ntype=\"stdio\"\ncommand={shell:?}\nargs=[\"-c\",{}]\n",
            serde_json::to_string(script).unwrap()
        ),
    )
    .unwrap();
    let approvals = ledger(root.path());
    approve(root.path(), &approvals);
    let service = ProjectMcpService::new(
        unavailable(),
        approvals,
        Arc::new(NetworkGuard::new().unwrap()),
    );
    let lease = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    assert_eq!(lease.snapshot().tools.len(), 1, "{:?}", lease.statuses());
    let snapshot = serde_json::to_vec(lease.snapshot()).unwrap();
    let bus = Arc::new(EventBus::new(64));
    let executor = lease
        .register_executor(&ToolExecutor::new(bus.clone()), bus)
        .unwrap();
    let context = tools::ToolExecutionContext {
        run_id: "test".into(),
        ..Default::default()
    };
    let first = executor
        .execute(&context, "mcp__fixture__read", "first", json!({}))
        .await
        .unwrap();
    assert!(first.is_error);
    assert_eq!(first.detail.unwrap()["class"], "outcome_unknown");
    assert_eq!(
        std::fs::read_to_string(root.path().join("starts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let second = executor
        .execute(&context, "mcp__fixture__read", "second", json!({}))
        .await
        .unwrap();
    assert!(!second.is_error, "{}", second.content);
    for call in ["third", "fourth"] {
        assert!(
            executor
                .execute(&context, "mcp__fixture__read", call, json!({}))
                .await
                .unwrap()
                .is_error
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("starts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        3
    );
    assert_eq!(serde_json::to_vec(lease.snapshot()).unwrap(), snapshot);
    lease.shutdown(ShutdownReason::Completed).await;
}
fn service(root: &Path, factory: Arc<Factory>, backend: McpCredentialBackend) -> ProjectMcpService {
    ProjectMcpService::new(
        backend,
        ledger(root),
        Arc::new(NetworkGuard::new().unwrap()),
    )
    .with_transport_factories(factory.clone(), factory)
}
fn request(root: &Path, role: Role) -> PrepareMcpRun {
    PrepareMcpRun {
        canonical_root: root.canonicalize().unwrap(),
        run_id: "test".into(),
        role,
        cancel: CancelContext::default(),
    }
}
fn unavailable() -> McpCredentialBackend {
    McpCredentialBackend::Unavailable(SecretUnavailableReason::Unavailable)
}

#[tokio::test]
async fn unapproved_stdio_never_spawns() {
    let root = project("[servers.fixture.env]\nTOKEN = { keychain = \"never-read\" }");
    let factory = Arc::new(Factory::default());
    let lease = service(root.path(), factory.clone(), unavailable())
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    assert!(lease.snapshot().tools.is_empty());
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
    assert_eq!(lease.statuses()[0].status, McpServerStatus::NeedsApproval);
}
#[tokio::test]
async fn unapproved_http_never_connects() {
    let root = project("");
    std::fs::write(root.path().join(".evorch/mcp.toml"),
        "version=1\n[servers.remote]\ntype=\"http\"\nurl=\"https://must-not-resolve.invalid/mcp\"\ntoken={keychain=\"never-read\"}\n").unwrap();
    let factory = Arc::new(Factory::default());
    let lease = service(root.path(), factory.clone(), unavailable())
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    assert!(lease.snapshot().tools.is_empty());
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        lease.statuses()[0].reason,
        Some(McpErrorKind::NeedsApproval)
    );
}
#[tokio::test]
async fn worker_is_default_and_other_roles_do_not_connect() {
    let root = project("");
    approve(root.path(), &ledger(root.path()));
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    let other = service
        .prepare_new_run(request(root.path(), Role::Planner))
        .await
        .unwrap();
    assert!(other.snapshot().tools.is_empty());
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
    let worker = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    let name = "mcp__fixture__read";
    assert_eq!(worker.snapshot().tools[0].public_name, name);
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
    let bus = Arc::new(EventBus::new(64));
    let base = ToolExecutor::new(bus.clone());
    let executor = worker.register_executor(&base, bus.clone()).unwrap();
    assert!(base.tool_permissions(name).is_none());
    assert!(executor.tool_permissions(name).is_some());
    let inherited = ToolExecutor::new(bus).inherit_host_tools(&executor);
    assert!(inherited.tool_permissions(name).is_none());
    worker.shutdown(ShutdownReason::Completed).await;
}
#[tokio::test]
async fn stdio_limit_is_shared_across_project_runs_and_gui_test() {
    let root = project("");
    approve(root.path(), &ledger(root.path()));
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    // GUI connection tests must use this same host-owned admission service.
    let gui = service
        .limiter()
        .try_acquire(&root.path().canonicalize().unwrap())
        .unwrap();
    let mut leases = Vec::new();
    for _ in 1..MAX_STDIO_PER_PROJECT {
        leases.push(
            service
                .prepare_new_run(request(root.path(), Role::Worker))
                .await
                .unwrap(),
        );
    }
    assert_eq!(factory.0.load(Ordering::SeqCst), MAX_STDIO_PER_PROJECT - 1);
    let rejected = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    assert!(rejected.snapshot().tools.is_empty());
    assert_eq!(
        rejected.statuses()[0].status,
        McpServerStatus::ProcessLimitReached
    );
    drop(gui);
    let admitted = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    assert_eq!(admitted.snapshot().tools.len(), 1);
    for lease in leases {
        lease.shutdown(ShutdownReason::Completed).await;
    }
    admitted.shutdown(ShutdownReason::Completed).await;
}
#[tokio::test]
async fn credential_failure_reasons_remain_distinct_without_fallback() {
    let root = project("[servers.fixture.env]\nTOKEN = {keychain=\"private-reference\"}");
    approve(root.path(), &ledger(root.path()));
    for (reason, kind) in [
        (
            SecretUnavailableReason::Unavailable,
            McpErrorKind::SecretUnavailable,
        ),
        (SecretUnavailableReason::Locked, McpErrorKind::SecretLocked),
        (SecretUnavailableReason::Denied, McpErrorKind::SecretDenied),
        (
            SecretUnavailableReason::Missing,
            McpErrorKind::SecretMissing,
        ),
    ] {
        let factory = Arc::new(Factory::default());
        let lease = service(
            root.path(),
            factory.clone(),
            McpCredentialBackend::Unavailable(reason),
        )
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
        assert_eq!(
            lease.statuses()[0].status,
            McpServerStatus::SecretUnavailable
        );
        assert_eq!(lease.statuses()[0].reason, Some(kind));
        assert_eq!(factory.0.load(Ordering::SeqCst), 0);
        assert!(!format!("{:?}", lease.statuses()).contains("private-reference"));
    }
}
#[tokio::test]
async fn missing_environment_is_safe_and_consistent_for_env_and_args() {
    let name = "EVORCH_MCP_UNSET_91C193D07F674F628AF2D52818010EFC";
    assert!(std::env::var_os(name).is_none());
    for (extra, field) in [
        (format!("args=[\"${{{name}}}\"]"), McpValueField::Args),
        (
            format!("[servers.fixture.env]\nTOKEN=\"${{{name}}}\""),
            McpValueField::Env,
        ),
    ] {
        let root = project(&extra);
        approve(root.path(), &ledger(root.path()));
        let factory = Arc::new(Factory::default());
        let lease = service(root.path(), factory.clone(), unavailable())
            .prepare_new_run(request(root.path(), Role::Worker))
            .await
            .unwrap();
        assert_eq!(lease.statuses()[0].status, McpServerStatus::Invalid);
        assert_eq!(
            lease.statuses()[0].reason,
            Some(McpErrorKind::MissingEnvironment(field))
        );
        assert_eq!(factory.0.load(Ordering::SeqCst), 0);
        assert!(
            !serde_json::to_string(lease.statuses())
                .unwrap()
                .contains(name)
        );
    }
}
struct Model {
    observed: Mutex<Vec<Vec<String>>>,
}

struct ServiceSpy(AtomicUsize);
#[async_trait]
impl McpRunService for ServiceSpy {
    async fn prepare_new_run(
        &self,
        _: PrepareMcpRun,
    ) -> Result<runtime::mcp::RunMcpLease, McpErrorKind> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(McpErrorKind::Unavailable)
    }
    fn restore_snapshot(
        &self,
        _: runtime::mcp::RestoreMcpRun,
    ) -> Result<runtime::mcp::RunMcpLease, McpErrorKind> {
        panic!("new run")
    }
}
struct Capture;
#[async_trait]
impl runtime::benchmark::BenchmarkRecorder for Capture {
    async fn capture(&self, _: &runtime::benchmark::BenchmarkCheckpoint) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn benchmark_never_prepares_mcp() {
    for benchmark in [false, true] {
        let root = project("");
        let spy = Arc::new(ServiceSpy(AtomicUsize::new(0)));
        let bus = Arc::new(EventBus::new(64));
        let model = Arc::new(Model {
            observed: Mutex::new(Vec::new()),
        });
        let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
            .with_mcp_service(spy.clone());
        let runtime = if benchmark {
            runtime
                .with_benchmark_recorder(
                    runtime::benchmark::BenchmarkSelector {
                        role: Role::Worker,
                        category: None,
                        occurrence: 1,
                    },
                    Arc::new(Capture),
                )
                .unwrap()
        } else {
            runtime
        };
        let run = runtime.delegate_background(
            Role::Worker,
            "test".into(),
            RunConfig {
                project_root: Some(root.path().to_owned()),
                ..RunConfig::default()
            },
        );
        runtime.wait(run).await.unwrap();
        assert_eq!(spy.0.load(Ordering::SeqCst), usize::from(!benchmark));
    }
}
#[async_trait]
impl AgentModel for Model {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "fixture".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.observed
            .lock()
            .unwrap()
            .push(tools.iter().map(|tool| tool.name.clone()).collect());
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: providers::Usage::default(),
            usage_reported: true,
        })
    }
}
#[tokio::test]
async fn unapproved_server_is_skipped_and_run_continues() {
    let root = project("");
    let factory = Arc::new(Factory::default());
    let service = Arc::new(service(root.path(), factory.clone(), unavailable()));
    let bus = Arc::new(EventBus::new(64));
    let model = Arc::new(Model {
        observed: Mutex::new(Vec::new()),
    });
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_mcp_service(service.clone());
    let run = runtime.delegate_background(
        Role::Worker,
        "test".into(),
        RunConfig {
            project_root: Some(root.path().to_owned()),
            ..RunConfig::default()
        },
    );
    runtime.wait(run).await.unwrap();
    assert_eq!(model.observed.lock().unwrap().len(), 1);
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
    let status = service.subscribe_status(root.path().canonicalize().unwrap());
    assert_eq!(status.borrow()[0].status, McpServerStatus::NeedsApproval);
}

#[tokio::test]
async fn approved_names_are_published_after_executor_rebuild_without_role_leakage() {
    let root = project("");
    approve(root.path(), &ledger(root.path()));
    let factory = Arc::new(Factory::default());
    let service = Arc::new(service(root.path(), factory.clone(), unavailable()));
    let bus = Arc::new(EventBus::new(64));
    let model = Arc::new(Model {
        observed: Mutex::new(Vec::new()),
    });
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_mcp_service(service);
    for role in [Role::Worker, Role::WebResearcher, Role::Worker] {
        let run = runtime.delegate_background(
            role,
            "test".into(),
            RunConfig {
                project_root: Some(root.path().to_owned()),
                ..Default::default()
            },
        );
        runtime.wait(run).await.unwrap();
    }
    let observed = model.observed.lock().unwrap();
    assert_eq!(observed.len(), 3);
    for (tools, enabled) in observed.iter().zip([true, false, true]) {
        assert_eq!(
            tools.iter().any(|name| name == "mcp__fixture__read"),
            enabled
        );
    }
    assert_eq!(factory.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn broken_toml_without_servers_is_reported_without_connection() {
    let root = project("");
    std::fs::write(root.path().join(".evorch/mcp.toml"), "not = [valid").unwrap();
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    assert_eq!(
        service
            .prepare_new_run(request(root.path(), Role::Worker))
            .await
            .err(),
        Some(McpErrorKind::Configuration)
    );
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
}
