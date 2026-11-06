use agents::Role;
use async_trait::async_trait;
use event_bus::EventBus;
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, RunConfig, RuntimeError,
    mcp::{McpGuiService, McpRunService, PrepareMcpRun, ProjectMcpService, SaveMcpSecret},
};
use serde_json::json;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
struct WireState {
    calls: Mutex<Vec<serde_json::Value>>,
    lists: AtomicUsize,
    initializes: AtomicUsize,
    fail_calls: AtomicUsize,
    changed_schema: AtomicBool,
    legacy: AtomicBool,
}
#[derive(Default)]
struct Factory(AtomicUsize, Arc<WireState>);
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
        Ok(Arc::new(Transport(Mutex::new(slot), self.1.clone())))
    }
}
struct Transport(Mutex<Option<Box<dyn McpProcessPermit>>>, Arc<WireState>);
#[async_trait]
impl McpTransport for Transport {
    async fn exchange(
        &self,
        request: RpcRequest,
        _: CancelContext,
    ) -> Result<McpResponseEnvelope, McpError> {
        if request.method == "server/discover" && self.1.legacy.load(Ordering::SeqCst) {
            return McpResponseEnvelope::new(Some(400),McpResponseHeaders::default(),vec![serde_json::from_value(json!({"jsonrpc":"2.0","id":request.id,"error":{"code":-32601,"message":"Method not found"}})).unwrap()],64).map_err(error);
        }
        let result = match request.method.as_str() {
            "initialize" => {
                self.1.initializes.fetch_add(1, Ordering::SeqCst);
                json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
            }
            "server/discover" => {
                json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"})
            }
            "tools/list" => {
                self.1.lists.fetch_add(1, Ordering::SeqCst);
                let schema = if self.1.changed_schema.load(Ordering::SeqCst) {
                    json!({"type":"object","properties":{"changed":{"type":"string"}}})
                } else {
                    json!({"type":"object"})
                };
                json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"read","description":"remote definition","inputSchema":schema}]})
            }
            "tools/call" => {
                self.1
                    .calls
                    .lock()
                    .unwrap()
                    .push(request.params["arguments"].clone());
                if self
                    .1
                    .fail_calls
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
                {
                    return Err(error(McpErrorKind::OutcomeUnknown));
                }
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
    service.revoke_approval(scope(root.path())).await.unwrap();
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
    assert!(other.statuses().is_empty());
    assert!(
        service
            .subscribe_status(root.path().canonicalize().unwrap())
            .borrow()
            .is_empty()
    );
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
    let mut leases = Vec::new();
    for _ in 0..MAX_STDIO_PER_PROJECT {
        leases.push(
            service
                .prepare_new_run(request(root.path(), Role::Worker))
                .await
                .unwrap(),
        );
    }
    // Preserve the distinct new-run contract as well as the real GUI entry point:
    // overflow excludes only this server and still returns a usable run lease.
    let rejected = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    assert!(rejected.snapshot().tools.is_empty());
    assert_eq!(
        rejected.statuses()[0].status,
        McpServerStatus::ProcessLimitReached
    );
    let scope = scope(root.path());
    let revision = config::mcp::load_project(root.path()).unwrap().revision;
    assert_eq!(
        service
            .test_connection(scope.clone(), revision, CancelContext::default())
            .await
            .unwrap_err(),
        McpErrorKind::ProcessLimitReached
    );
    assert_eq!(factory.0.load(Ordering::SeqCst), MAX_STDIO_PER_PROJECT);
    leases
        .pop()
        .unwrap()
        .shutdown(ShutdownReason::Completed)
        .await;
    assert_eq!(
        service
            .test_connection(scope, revision, CancelContext::default())
            .await
            .unwrap(),
        McpServerStatus::Ready
    );
    assert_eq!(factory.0.load(Ordering::SeqCst), MAX_STDIO_PER_PROJECT + 1);
    // The short-lived GUI connection is reaped, not a fifth persistent session.
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
    observed_schemas: Mutex<Vec<Vec<providers::ToolSpec>>>,
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
            observed_schemas: Mutex::new(Vec::new()),
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
        self.observed_schemas.lock().unwrap().push(tools.to_vec());
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
        observed_schemas: Mutex::new(Vec::new()),
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
        observed_schemas: Mutex::new(Vec::new()),
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

fn scope(root: &Path) -> ApprovalScope {
    ApprovalScope {
        canonical_root: root.canonicalize().unwrap(),
        server_id: "fixture".into(),
    }
}
fn confirmation(preview: McpApprovalPreview) -> ConfirmMcpApproval {
    ConfirmMcpApproval {
        preview_id: preview.id,
        scope: preview.scope,
        expected_revision: preview.source_revision,
        expected_fingerprint: preview.fingerprint,
    }
}
#[tokio::test]
async fn gui_crud_and_preview_do_not_connect_or_change_existing_snapshot() {
    let root = project("");
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    let mut statuses =
        McpGuiService::subscribe_status(&service, root.path().canonicalize().unwrap());
    let before = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    let frozen = serde_json::to_vec(before.snapshot()).unwrap();
    let view = service.settings(root.path().into()).await.unwrap();
    assert_eq!(view.statuses[0].status, McpServerStatus::NeedsApproval);
    statuses.changed().await.unwrap();
    assert_eq!(
        statuses.borrow_and_update()[0].status,
        McpServerStatus::NeedsApproval
    );
    assert_eq!(
        service
            .test_connection(
                scope(root.path()),
                view.config.revision,
                CancelContext::default()
            )
            .await
            .unwrap_err(),
        McpErrorKind::NeedsApproval
    );
    let preview = service
        .preview_approval(scope(root.path()), view.config.revision)
        .await
        .unwrap();
    service
        .confirm_approval(confirmation(preview))
        .await
        .unwrap();
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
    assert_eq!(serde_json::to_vec(before.snapshot()).unwrap(), frozen);
    assert_eq!(
        service
            .test_connection(
                scope(root.path()),
                view.config.revision,
                CancelContext::default()
            )
            .await
            .unwrap(),
        McpServerStatus::Ready
    );
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
    let edited = service
        .save_override(
            root.path().into(),
            view.config.revision,
            config::mcp::McpOverrideMutation::Disable {
                server_id: "fixture".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(edited.statuses[0].status, McpServerStatus::Disabled);
    assert_eq!(
        service
            .save_override(
                root.path().into(),
                view.config.revision,
                config::mcp::McpOverrideMutation::RemoveOverride {
                    server_id: "fixture".into()
                }
            )
            .await
            .unwrap_err(),
        McpErrorKind::ConfigConflict
    );
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
    assert_eq!(serde_json::to_vec(before.snapshot()).unwrap(), frozen);
}
#[tokio::test]
async fn gui_approval_rejects_stale_preview_revision_and_fingerprint() {
    let root = project("");
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    let revision = config::mcp::load_project(root.path()).unwrap().revision;
    let preview = service
        .preview_approval(scope(root.path()), revision)
        .await
        .unwrap();
    let path = root.path().join(".evorch/mcp.toml");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{original}\n# edited externally\n")).unwrap();
    assert_eq!(
        service
            .confirm_approval(confirmation(preview))
            .await
            .unwrap_err(),
        McpErrorKind::StalePreview
    );
    let revision = config::mcp::load_project(root.path()).unwrap().revision;
    let preview = service
        .preview_approval(scope(root.path()), revision)
        .await
        .unwrap();
    let mut forged = confirmation(preview);
    forged.expected_fingerprint.0[0] ^= 1;
    assert_eq!(
        service.confirm_approval(forged).await.unwrap_err(),
        McpErrorKind::StalePreview
    );
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn gui_http_preview_pins_only_the_checked_destination_without_connecting() {
    let root = project("");
    std::fs::write(root.path().join(".evorch/mcp.toml"),"version=1\n[servers.fixture]\ntype=\"http\"\nurl=\"http://127.0.0.1:3000/mcp?token=private-query-value\"\n").unwrap();
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    let view = service.settings(root.path().into()).await.unwrap();
    assert_eq!(view.statuses[0].status, McpServerStatus::NeedsApproval);
    let preview = service
        .preview_approval(scope(root.path()), view.config.revision)
        .await
        .unwrap();
    assert!(
        !serde_json::to_string(&preview)
            .unwrap()
            .contains("private-query-value")
    );
    service
        .confirm_approval(confirmation(preview))
        .await
        .unwrap();
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        service
            .test_connection(
                scope(root.path()),
                view.config.revision,
                CancelContext::default()
            )
            .await
            .unwrap(),
        McpServerStatus::Ready
    );
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
}
#[derive(Default)]
struct MemorySecrets {
    values: Mutex<std::collections::BTreeMap<String, sandbox::Secret>>,
    after_set: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    after_delete: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    deletes: AtomicUsize,
    fail_delete: AtomicBool,
}
impl sandbox::CredentialStore for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<sandbox::Secret>, sandbox::CredentialError> {
        Ok(self.values.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &sandbox::Secret) -> Result<(), sandbox::CredentialError> {
        self.values
            .lock()
            .unwrap()
            .insert(key.into(), value.clone());
        if let Some(hook) = self.after_set.lock().unwrap().take() {
            hook();
        }
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), sandbox::CredentialError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(sandbox::CredentialError::KeychainLocked);
        }
        self.values.lock().unwrap().remove(key);
        if let Some(hook) = self.after_delete.lock().unwrap().take() {
            hook();
        }
        Ok(())
    }
}
fn secret_request(root: &Path, value: &str) -> SaveMcpSecret {
    let config = config::mcp::load_project(root).unwrap();
    let mut definition = config.servers["fixture"].definition.clone().unwrap();
    let config::mcp::McpTransportDefinition::Stdio { env, .. } = &mut definition.transport else {
        panic!("stdio fixture")
    };
    env.insert(
        "TOKEN".into(),
        config::mcp::ValueSource::Keychain {
            name: "token".into(),
        },
    );
    SaveMcpSecret {
        scope: scope(root),
        logical_name: "token".into(),
        value: sandbox::Secret::from(value.to_owned()),
        expected_revision: config.revision,
        mutation: config::mcp::McpOverrideMutation::Upsert {
            server_id: "fixture".into(),
            definition,
        },
    }
}
#[tokio::test]
async fn gui_secret_write_persists_only_reference_and_rotation_preserves_consent() {
    use sandbox::CredentialStore;
    let root = project("");
    let factory = Arc::new(Factory::default());
    let store = Arc::new(MemorySecrets::default());
    let service = service(
        root.path(),
        factory.clone(),
        McpCredentialBackend::Available(store.clone()),
    );
    let secret_command = secret_request(root.path(), "first-value-never-in-config");
    assert!(!format!("{secret_command:?}").contains("first-value-never-in-config"));
    let view = service.save_secret(secret_command).await.unwrap();
    assert!(!format!("{view:?}").contains("first-value-never-in-config"));
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
    let preview = service
        .preview_approval(scope(root.path()), view.config.revision)
        .await
        .unwrap();
    let fingerprint = preview.fingerprint;
    service
        .confirm_approval(confirmation(preview))
        .await
        .unwrap();
    let lease = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    let frozen = serde_json::to_vec(lease.snapshot()).unwrap();
    let view = service
        .save_secret(secret_request(root.path(), "second-value-never-in-config"))
        .await
        .unwrap();
    let next = service
        .preview_approval(scope(root.path()), view.config.revision)
        .await
        .unwrap();
    assert_eq!(next.fingerprint, fingerprint);
    let config = std::fs::read_to_string(root.path().join(".evorch/mcp.toml")).unwrap();
    assert!(config.contains("keychain"));
    assert!(!config.contains("first-value-never-in-config"));
    assert!(!config.contains("second-value-never-in-config"));
    assert_eq!(
        store
            .get(&tools::mcp::secrets::credential_key(
                &scope(root.path()),
                "token"
            ))
            .unwrap()
            .unwrap()
            .expose(),
        "second-value-never-in-config"
    );
    assert_eq!(serde_json::to_vec(lease.snapshot()).unwrap(), frozen);
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
    lease.shutdown(ShutdownReason::Completed).await;
}
#[tokio::test]
async fn gui_secret_failure_never_writes_plaintext_or_falls_back() {
    let root = project("");
    let before = std::fs::read(root.path().join(".evorch/mcp.toml")).unwrap();
    for (reason, kind) in [
        (
            SecretUnavailableReason::Unavailable,
            McpErrorKind::SecretUnavailable,
        ),
        (SecretUnavailableReason::Locked, McpErrorKind::SecretLocked),
        (SecretUnavailableReason::Denied, McpErrorKind::SecretDenied),
    ] {
        let factory = Arc::new(Factory::default());
        let service = service(
            root.path(),
            factory.clone(),
            McpCredentialBackend::Unavailable(reason),
        );
        assert_eq!(
            service
                .save_secret(secret_request(root.path(), "private-value"))
                .await
                .unwrap_err(),
            kind
        );
        assert_eq!(
            std::fs::read(root.path().join(".evorch/mcp.toml")).unwrap(),
            before
        );
        assert_eq!(factory.0.load(Ordering::SeqCst), 0);
        assert!(!root.path().join("credentials.json").exists());
    }
}
#[tokio::test]
async fn gui_failed_secret_save_cleans_only_new_unused_keys() {
    use sandbox::CredentialStore;
    for preexisting in [false, true] {
        let root = project("");
        let factory = Arc::new(Factory::default());
        let store = Arc::new(MemorySecrets::default());
        let key = tools::mcp::secrets::credential_key(&scope(root.path()), "token");
        if preexisting {
            store
                .set(&key, &sandbox::Secret::from("old-value".to_owned()))
                .unwrap();
        }
        let path = root.path().join(".evorch/mcp.toml");
        let original = std::fs::read_to_string(&path).unwrap();
        *store.after_set.lock().unwrap() = Some(Box::new(move || {
            std::fs::write(
                path,
                format!("{original}\n# changed during keychain write\n"),
            )
            .unwrap()
        }));
        let service = service(
            root.path(),
            factory.clone(),
            McpCredentialBackend::Available(store.clone()),
        );
        assert_eq!(
            service
                .save_secret(secret_request(root.path(), "new-value"))
                .await
                .unwrap_err(),
            McpErrorKind::ConfigConflict
        );
        assert_eq!(store.get(&key).unwrap().is_some(), preexisting);
        assert_eq!(
            store.deletes.load(Ordering::SeqCst),
            usize::from(!preexisting)
        );
        assert_eq!(factory.0.load(Ordering::SeqCst), 0);
    }
}

struct BlockingFactory {
    entered: Arc<tokio::sync::Notify>,
    shutdowns: Arc<AtomicUsize>,
}
#[async_trait]
impl McpTransportFactory for BlockingFactory {
    async fn connect(
        &self,
        server: ResolvedMcpServer,
        permit: ApprovalPermit,
        limiter: Arc<dyn McpProcessLimiter>,
        _: CancelContext,
    ) -> Result<Arc<dyn McpTransport>, McpError> {
        assert_eq!(server.fingerprint, permit.fingerprint());
        Ok(Arc::new(BlockingTransport {
            slot: Mutex::new(match server.transport {
                ResolvedMcpTransport::Stdio { .. } => Some(
                    limiter
                        .try_acquire(&server.scope.canonical_root)
                        .map_err(error)?,
                ),
                ResolvedMcpTransport::Http { .. } => None,
            }),
            entered: self.entered.clone(),
            shutdowns: self.shutdowns.clone(),
        }))
    }
}
struct BlockingTransport {
    slot: Mutex<Option<Box<dyn McpProcessPermit>>>,
    entered: Arc<tokio::sync::Notify>,
    shutdowns: Arc<AtomicUsize>,
}
#[async_trait]
impl McpTransport for BlockingTransport {
    async fn exchange(
        &self,
        request: RpcRequest,
        cancel: CancelContext,
    ) -> Result<McpResponseEnvelope, McpError> {
        if request.method == "tools/list" {
            self.entered.notify_one();
            cancel.cancelled().await;
            Err(error(McpErrorKind::Cancelled))
        } else {
            Transport(Mutex::new(None), Default::default())
                .exchange(request, cancel)
                .await
        }
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
        self.slot.lock().unwrap().take();
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[tokio::test]
async fn gui_cancelled_connection_test_releases_its_shared_process_permit() {
    let root = project("");
    approve(root.path(), &ledger(root.path()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(BlockingFactory {
        entered: entered.clone(),
        shutdowns: shutdowns.clone(),
    });
    let service = Arc::new(
        ProjectMcpService::new(
            unavailable(),
            ledger(root.path()),
            Arc::new(NetworkGuard::new().unwrap()),
        )
        .with_transport_factories(factory.clone(), factory),
    );
    let revision = config::mcp::load_project(root.path()).unwrap().revision;
    let scope = scope(root.path());
    let cancel = CancelContext::default();
    let task = tokio::spawn({
        let service = service.clone();
        let cancel = cancel.clone();
        let scope = scope.clone();
        async move { service.test_connection(scope, revision, cancel).await }
    });
    entered.notified().await;
    cancel.cancel();
    assert_eq!(task.await.unwrap().unwrap_err(), McpErrorKind::Cancelled);
    assert!(shutdowns.load(Ordering::SeqCst) > 0);
    let slots: Vec<_> = (0..MAX_STDIO_PER_PROJECT)
        .map(|_| {
            service
                .limiter()
                .try_acquire(&scope.canonical_root)
                .unwrap()
        })
        .collect();
    assert_eq!(slots.len(), MAX_STDIO_PER_PROJECT);
    assert_eq!(
        service.subscribe_status(scope.canonical_root).borrow()[0].reason,
        Some(McpErrorKind::Cancelled)
    );
}

#[tokio::test]
async fn gui_public_https_without_credentials_does_not_require_approval() {
    let root = project("");
    std::fs::write(
        root.path().join(".evorch/mcp.toml"),
        "version=1\n[servers.fixture]\ntype=\"http\"\nurl=\"https://8.8.8.8/mcp\"\n",
    )
    .unwrap();
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    let view = service.settings(root.path().into()).await.unwrap();
    assert_eq!(view.statuses[0].status, McpServerStatus::Unavailable);
    assert_eq!(view.statuses[0].reason, None);
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
    // The injected transport avoids actual I/O; the real NetworkGuard still checks
    // and pins this public literal destination before admitting the fake session.
    assert_eq!(
        service
            .test_connection(
                scope(root.path()),
                view.config.revision,
                CancelContext::default()
            )
            .await
            .unwrap(),
        McpServerStatus::Ready
    );
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
}

fn saved_executor(lease: &runtime::mcp::RunMcpLease) -> ToolExecutor {
    let bus = Arc::new(EventBus::new(64));
    lease
        .register_executor(&ToolExecutor::new(bus.clone()), bus)
        .unwrap()
}
fn tool_context() -> tools::ToolExecutionContext {
    tools::ToolExecutionContext {
        run_id: "restored".into(),
        ..Default::default()
    }
}
#[tokio::test]
async fn restore_registers_snapshot_without_reading_config_or_connecting() {
    let root = project("");
    let factory = Arc::new(Factory::default());
    approve(root.path(), &ledger(root.path()));
    let service = service(root.path(), factory.clone(), unavailable());
    let first = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    let snapshot = first.snapshot().clone();
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    first.shutdown(ShutdownReason::Completed).await;
    // A missing config cannot affect pure registration or the frozen schemas.
    std::fs::remove_file(root.path().join(".evorch/mcp.toml")).unwrap();
    let restored = service
        .restore_snapshot(runtime::mcp::RestoreMcpRun {
            context: request(root.path(), Role::Worker),
            snapshot,
        })
        .unwrap();
    let executor = saved_executor(&restored);
    assert!(executor.tool_permissions("mcp__fixture__read").is_some());
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
    assert_eq!(serde_json::to_vec(restored.snapshot()).unwrap(), bytes);
    let output = executor
        .execute(&tool_context(), "mcp__fixture__read", "missing", json!({}))
        .await
        .unwrap();
    assert!(output.is_error);
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
    restored.shutdown(ShutdownReason::Completed).await;
}
#[tokio::test]
async fn restore_first_call_connects_once_and_checks_live_definition() {
    for changed in [false, true] {
        let root = project("");
        let factory = Arc::new(Factory::default());
        approve(root.path(), &ledger(root.path()));
        let service = service(root.path(), factory.clone(), unavailable());
        let first = service
            .prepare_new_run(request(root.path(), Role::Worker))
            .await
            .unwrap();
        let snapshot = first.snapshot().clone();
        first.shutdown(ShutdownReason::Completed).await;
        factory.1.changed_schema.store(changed, Ordering::SeqCst);
        let restored = service
            .restore_snapshot(runtime::mcp::RestoreMcpRun {
                context: request(root.path(), Role::Worker),
                snapshot: snapshot.clone(),
            })
            .unwrap();
        let executor = saved_executor(&restored);
        assert_eq!(factory.0.load(Ordering::SeqCst), 1);
        let result = executor
            .execute(&tool_context(), "mcp__fixture__read", "lazy", json!({}))
            .await
            .unwrap();
        assert_eq!(result.is_error, changed);
        assert_eq!(factory.0.load(Ordering::SeqCst), 2);
        assert_eq!(factory.1.calls.lock().unwrap().len(), usize::from(!changed));
        assert_eq!(restored.snapshot(), &snapshot);
        if changed {
            assert_eq!(result.detail.unwrap()["class"], "definition_changed");
        }
        restored.shutdown(ShutdownReason::Completed).await;
        let slots: Vec<_> = (0..4)
            .map(|_| service.limiter().try_acquire(root.path()).unwrap())
            .collect();
        drop(slots);
    }
}
#[tokio::test]
async fn restored_snapshot_is_retained_when_host_service_is_unavailable() {
    let root = project("");
    let factory = Arc::new(Factory::default());
    approve(root.path(), &ledger(root.path()));
    let service = service(root.path(), factory.clone(), unavailable());
    let first = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    let snapshot = first.snapshot().clone();
    first.shutdown(ShutdownReason::Completed).await;
    let absent = runtime::mcp::UnavailableMcpService::new(unavailable());
    let restored = absent
        .restore_snapshot(runtime::mcp::RestoreMcpRun {
            context: request(root.path(), Role::Worker),
            snapshot: snapshot.clone(),
        })
        .unwrap();
    let executor = saved_executor(&restored);
    let output = executor
        .execute(
            &tool_context(),
            "mcp__fixture__read",
            "unavailable",
            json!({}),
        )
        .await
        .unwrap();
    assert!(output.is_error);
    assert_eq!(output.detail.unwrap()["class"], "unavailable");
    assert_eq!(restored.snapshot(), &snapshot);
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn restored_revoked_or_wrong_role_never_connects() {
    for role in [Role::Worker, Role::Advisor] {
        let root = project("");
        let factory = Arc::new(Factory::default());
        let approvals = ledger(root.path());
        approve(root.path(), &approvals);
        let service = service(root.path(), factory.clone(), unavailable());
        let first = service
            .prepare_new_run(request(root.path(), Role::Worker))
            .await
            .unwrap();
        let snapshot = first.snapshot().clone();
        first.shutdown(ShutdownReason::Completed).await;
        if role == Role::Worker {
            service.revoke_approval(scope(root.path())).await.unwrap();
        }
        let restored = service
            .restore_snapshot(runtime::mcp::RestoreMcpRun {
                context: request(root.path(), role),
                snapshot: snapshot.clone(),
            })
            .unwrap();
        let output = saved_executor(&restored)
            .execute(&tool_context(), "mcp__fixture__read", "denied", json!({}))
            .await
            .unwrap();
        assert!(output.is_error);
        assert_eq!(
            output.detail.unwrap()["class"],
            if role == Role::Worker {
                "needs_approval"
            } else {
                "role_not_allowed"
            }
        );
        assert_eq!(factory.0.load(Ordering::SeqCst), 1);
        assert_eq!(restored.snapshot(), &snapshot);
    }
}
#[tokio::test]
async fn role_filtered_run_preserves_project_server_status() {
    let root = project("");
    approve(root.path(), &ledger(root.path()));
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    let worker = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    let previous = service.settings(root.path().to_owned()).await.unwrap();
    assert_eq!(previous.statuses[0].status, McpServerStatus::Ready);
    let other = service
        .prepare_new_run(request(root.path(), Role::Advisor))
        .await
        .unwrap();
    assert!(other.snapshot().tools.is_empty());
    assert!(other.statuses().is_empty());
    let current = service.settings(root.path().to_owned()).await.unwrap();
    assert_eq!(current.statuses[0].status, McpServerStatus::Ready);
    assert_eq!(current.statuses[0].reason, previous.statuses[0].reason);
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
    worker.shutdown(ShutdownReason::Completed).await;
}

#[tokio::test]
async fn enabled_server_without_public_roles_is_not_off_and_calls_are_role_denied() {
    let root = project("");
    approve(root.path(), &ledger(root.path()));
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    let worker = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    let path = root.path().join(".evorch/mcp.toml");
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{raw}\nroles=[]\n")).unwrap();
    let view = service.settings(root.path().to_owned()).await.unwrap();
    assert_ne!(view.statuses[0].status, McpServerStatus::Disabled);
    assert_eq!(view.statuses[0].reason, Some(McpErrorKind::RoleNotAllowed));
    let output = saved_executor(&worker)
        .execute(
            &tool_context(),
            "mcp__fixture__read",
            "not-shared",
            json!({}),
        )
        .await
        .unwrap();
    assert!(output.is_error);
    assert_eq!(output.detail.unwrap()["class"], "role_not_allowed");
    assert!(factory.1.calls.lock().unwrap().is_empty());
    assert_eq!(factory.0.load(Ordering::SeqCst), 1);
    worker.shutdown(ShutdownReason::Completed).await;
}

#[tokio::test]
async fn enabled_server_does_not_keep_a_cached_off_status() {
    let root = project("enabled=false");
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    let disabled = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    assert_eq!(disabled.statuses()[0].status, McpServerStatus::Disabled);
    let path = root.path().join(".evorch/mcp.toml");
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, raw.replace("enabled=false", "enabled=true")).unwrap();
    approve(root.path(), &ledger(root.path()));
    let other = service
        .prepare_new_run(request(root.path(), Role::Advisor))
        .await
        .unwrap();
    assert!(other.statuses().is_empty());
    let view = service.settings(root.path().to_owned()).await.unwrap();
    assert_eq!(view.statuses[0].status, McpServerStatus::Unavailable);
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn http_preparation_limit_reports_admission_instead_of_response_size() {
    let root = public_http_project();
    let entered = Arc::new(tokio::sync::Notify::new());
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(BlockingFactory {
        entered: entered.clone(),
        shutdowns: shutdowns.clone(),
    });
    let service = Arc::new(
        ProjectMcpService::new(
            unavailable(),
            ledger(root.path()),
            Arc::new(NetworkGuard::new().unwrap()),
        )
        .with_transport_factories(factory.clone(), factory),
    );
    let mut tasks = Vec::new();
    for _ in 0..MAX_HTTP_PREPARATIONS {
        let request = request(root.path(), Role::Worker);
        let cancel = request.cancel.clone();
        let service = service.clone();
        tasks.push((
            cancel,
            tokio::spawn(async move { service.prepare_new_run(request).await }),
        ));
        entered.notified().await;
    }
    let excluded = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    assert!(excluded.snapshot().tools.is_empty());
    assert_eq!(
        excluded.statuses()[0].reason,
        Some(McpErrorKind::PreparationLimitReached)
    );
    let view = service.settings(root.path().to_owned()).await.unwrap();
    assert_eq!(
        view.statuses[0].reason,
        Some(McpErrorKind::PreparationLimitReached)
    );
    for (cancel, task) in tasks {
        cancel.cancel();
        task.await.unwrap().unwrap();
    }
    assert_eq!(shutdowns.load(Ordering::SeqCst), MAX_HTTP_PREPARATIONS);
}

fn public_http_project() -> tempfile::TempDir {
    let root = project("");
    std::fs::write(
        root.path().join(".evorch/mcp.toml"),
        "version=1\n[servers.fixture]\ntype='http'\nurl='https://8.8.8.8/mcp'\n",
    )
    .unwrap();
    root
}
#[tokio::test]
async fn modern_http_unknown_call_is_not_replayed_but_later_explicit_call_recovers() {
    let root = public_http_project();
    let factory = Arc::new(Factory::default());
    let service = service(root.path(), factory.clone(), unavailable());
    let lease = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    let snapshot = lease.snapshot().clone();
    let executor = saved_executor(&lease);
    factory.1.fail_calls.store(1, Ordering::SeqCst);
    let first = executor
        .execute(
            &tool_context(),
            "mcp__fixture__read",
            "unknown",
            json!({"request":"one"}),
        )
        .await
        .unwrap();
    assert!(first.is_error);
    assert_eq!(first.detail.unwrap()["class"], "outcome_unknown");
    assert_eq!(
        factory.1.calls.lock().unwrap().as_slice(),
        &[json!({"request":"one"})]
    );
    assert_eq!(factory.1.lists.load(Ordering::SeqCst), 1);
    let second = executor
        .execute(
            &tool_context(),
            "mcp__fixture__read",
            "other",
            json!({"request":"two"}),
        )
        .await
        .unwrap();
    assert!(!second.is_error);
    assert_eq!(factory.1.lists.load(Ordering::SeqCst), 2);
    assert_eq!(factory.1.initializes.load(Ordering::SeqCst), 0);
    assert_eq!(
        factory.1.calls.lock().unwrap().as_slice(),
        &[json!({"request":"one"}), json!({"request":"two"})]
    );
    assert_eq!(lease.snapshot(), &snapshot);
    lease.shutdown(ShutdownReason::Completed).await;
}
#[tokio::test]
async fn legacy_http_reinitializes_only_once_for_a_later_explicit_call() {
    let root = public_http_project();
    let factory = Arc::new(Factory::default());
    factory.1.legacy.store(true, Ordering::SeqCst);
    let service = service(root.path(), factory.clone(), unavailable());
    let lease = service
        .prepare_new_run(request(root.path(), Role::Worker))
        .await
        .unwrap();
    let executor = saved_executor(&lease);
    assert_eq!(factory.1.initializes.load(Ordering::SeqCst), 1);
    factory.1.fail_calls.store(1, Ordering::SeqCst);
    assert!(
        executor
            .execute(
                &tool_context(),
                "mcp__fixture__read",
                "one",
                json!({"request":1})
            )
            .await
            .unwrap()
            .is_error
    );
    assert_eq!(factory.1.initializes.load(Ordering::SeqCst), 1);
    assert!(
        !executor
            .execute(
                &tool_context(),
                "mcp__fixture__read",
                "two",
                json!({"request":2})
            )
            .await
            .unwrap()
            .is_error
    );
    assert_eq!(factory.1.initializes.load(Ordering::SeqCst), 2);
    factory.1.fail_calls.store(1, Ordering::SeqCst);
    assert!(
        executor
            .execute(
                &tool_context(),
                "mcp__fixture__read",
                "three",
                json!({"request":3})
            )
            .await
            .unwrap()
            .is_error
    );
    assert!(
        executor
            .execute(
                &tool_context(),
                "mcp__fixture__read",
                "four",
                json!({"request":4})
            )
            .await
            .unwrap()
            .is_error
    );
    assert_eq!(factory.1.initializes.load(Ordering::SeqCst), 2);
    assert_eq!(
        factory.1.calls.lock().unwrap().as_slice(),
        &[
            json!({"request":1}),
            json!({"request":2}),
            json!({"request":3})
        ]
    );
    lease.shutdown(ShutdownReason::Completed).await;
}
fn delete_request(root: &Path) -> runtime::mcp::DeleteMcpSecret {
    let config = config::mcp::load_project(root).unwrap();
    let mut definition = config.servers["fixture"].definition.clone().unwrap();
    let config::mcp::McpTransportDefinition::Stdio { env, .. } = &mut definition.transport else {
        panic!("stdio")
    };
    env.remove("TOKEN");
    runtime::mcp::DeleteMcpSecret {
        scope: scope(root),
        logical_name: "token".into(),
        expected_revision: config.revision,
        mutation: config::mcp::McpOverrideMutation::Upsert {
            server_id: "fixture".into(),
            definition,
        },
    }
}
#[tokio::test]
async fn gui_secret_delete_saves_reference_removal_before_deleting_only_its_key() {
    for failed in [false, true] {
        let root = project("");
        let store = Arc::new(MemorySecrets::default());
        let service = service(
            root.path(),
            Arc::new(Factory::default()),
            McpCredentialBackend::Available(store.clone()),
        );
        service
            .save_secret(secret_request(root.path(), "secret-delete-sentinel"))
            .await
            .unwrap();
        let key = tools::mcp::secrets::credential_key(&scope(root.path()), "token");
        store.values.lock().unwrap().insert(
            "unrelated-provider-key".into(),
            sandbox::Secret::from("untouched".to_owned()),
        );
        store.fail_delete.store(failed, Ordering::SeqCst);
        let result = service
            .delete_secret(delete_request(root.path()))
            .await
            .unwrap();
        assert_eq!(
            result.keychain_error,
            failed.then_some(McpErrorKind::SecretLocked)
        );
        let definition = result.view.config.servers["fixture"]
            .definition
            .as_ref()
            .unwrap();
        let config::mcp::McpTransportDefinition::Stdio { env, .. } = &definition.transport else {
            panic!("stdio")
        };
        assert!(!env.contains_key("TOKEN"));
        let values = store.values.lock().unwrap();
        assert_eq!(values.contains_key(&key), failed);
        assert!(values.contains_key("unrelated-provider-key"));
        assert!(!format!("{result:?}").contains("secret-delete-sentinel"));
        assert_eq!(store.deletes.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn gui_stale_or_still_referenced_secret_delete_never_deletes_a_key() {
    let root = project("");
    let store = Arc::new(MemorySecrets::default());
    let service = service(
        root.path(),
        Arc::new(Factory::default()),
        McpCredentialBackend::Available(store.clone()),
    );
    service
        .save_secret(secret_request(root.path(), "kept"))
        .await
        .unwrap();
    let mut invalid = delete_request(root.path());
    let config::mcp::McpOverrideMutation::Upsert { definition, .. } = &mut invalid.mutation else {
        panic!("upsert")
    };
    let config::mcp::McpTransportDefinition::Stdio { env, .. } = &mut definition.transport else {
        panic!("stdio")
    };
    env.insert(
        "SECOND_TOKEN".into(),
        config::mcp::ValueSource::Keychain {
            name: "token".into(),
        },
    );
    let before = std::fs::read(root.path().join(".evorch/mcp.toml")).unwrap();
    assert_eq!(
        service.delete_secret(invalid).await.unwrap_err(),
        McpErrorKind::Configuration
    );
    assert_eq!(
        std::fs::read(root.path().join(".evorch/mcp.toml")).unwrap(),
        before
    );
    let stale = delete_request(root.path());
    std::fs::write(
        root.path().join(".evorch/mcp.toml"),
        [before, b"\n# external edit\n".to_vec()].concat(),
    )
    .unwrap();
    assert_eq!(
        service.delete_secret(stale).await.unwrap_err(),
        McpErrorKind::ConfigConflict
    );
    assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
    assert_eq!(store.values.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn persisted_chat_restores_the_frozen_mcp_snapshot_without_discovery() {
    for prepared in [false, true] {
        let root = project("");
        approve(root.path(), &ledger(root.path()));
        let factory = Arc::new(Factory::default());
        let service = Arc::new(service(root.path(), factory.clone(), unavailable()));
        let config = storage::StorageConfig {
            db_path: root.path().join("restore.sqlite3"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let model = Arc::new(Model {
            observed: Default::default(),
            observed_schemas: Default::default(),
        });
        let build = || {
            let bus = Arc::new(EventBus::new(64));
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap())
        };
        let runtime = build();
        let runtime = if prepared {
            runtime.with_mcp_service(service.clone())
        } else {
            runtime
        };
        let first = runtime.delegate_background(
            Role::Worker,
            "prior".into(),
            RunConfig {
                name: Some(format!("chat:{}:frozen", Role::Worker.name())),
                project_root: Some(root.path().join(".")),
                ..Default::default()
            },
        );
        runtime.wait(first).await.unwrap();
        drop(runtime);
        if prepared {
            let record = storage::Database::open(&config)
                .unwrap()
                .run_context(&first.to_string())
                .unwrap()
                .unwrap();
            let descriptor: serde_json::Value = serde_json::from_str(&record.config_json).unwrap();
            assert_eq!(
                descriptor["project_root"].as_str().unwrap(),
                root.path().canonicalize().unwrap().to_str().unwrap()
            );
            assert!(descriptor["mcp_snapshot"].is_object());
        }
        let raw = std::fs::read(root.path().join(".evorch/mcp.toml")).unwrap();
        std::fs::remove_file(root.path().join(".evorch/mcp.toml")).unwrap();
        factory.1.changed_schema.store(true, Ordering::SeqCst);
        let runtime = build().with_mcp_service(service);
        let continued = runtime
            .delegate_chat(
                "frozen",
                Role::Worker,
                "followup".into(),
                RunConfig {
                    project_root: Some(root.path().to_owned()),
                    ..Default::default()
                },
            )
            .unwrap();
        runtime.wait(continued).await.unwrap();
        assert_eq!(factory.0.load(Ordering::SeqCst), usize::from(prepared));
        let external = |tools: &Vec<providers::ToolSpec>| {
            tools
                .iter()
                .filter(|tool| tool.name.starts_with("mcp__"))
                .map(|tool| serde_json::to_value(tool).unwrap())
                .collect::<Vec<_>>()
        };
        let original = {
            let observed = model.observed_schemas.lock().unwrap();
            assert_eq!(observed.len(), 2);
            let original = external(&observed[0]);
            assert_eq!(original.len(), usize::from(prepared));
            assert_eq!(original, external(&observed[1]));
            original
        };
        std::fs::write(root.path().join(".evorch/mcp.toml"), raw).unwrap();
        let fresh = runtime.delegate_background(
            Role::Worker,
            "fresh".into(),
            RunConfig {
                project_root: Some(root.path().to_owned()),
                ..Default::default()
            },
        );
        runtime.wait(fresh).await.unwrap();
        assert_eq!(factory.0.load(Ordering::SeqCst), usize::from(prepared) + 1);
        let observed = model.observed_schemas.lock().unwrap();
        assert_ne!(original, external(&observed[2]));
    }
}
#[tokio::test]
async fn gui_secret_delete_without_backend_is_a_reported_reference_only_success() {
    let root = project("");
    let store = Arc::new(MemorySecrets::default());
    let factory = Arc::new(Factory::default());
    let available = service(
        root.path(),
        factory.clone(),
        McpCredentialBackend::Available(store.clone()),
    );
    available
        .save_secret(secret_request(root.path(), "retained-unavailable"))
        .await
        .unwrap();
    let unavailable = service(root.path(), factory, unavailable());
    let result = unavailable
        .delete_secret(delete_request(root.path()))
        .await
        .unwrap();
    assert_eq!(result.keychain_error, Some(McpErrorKind::SecretUnavailable));
    assert_eq!(store.values.lock().unwrap().len(), 1);
    assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
    let config::mcp::McpTransportDefinition::Stdio { env, .. } =
        &result.view.config.servers["fixture"]
            .definition
            .as_ref()
            .unwrap()
            .transport
    else {
        panic!("stdio")
    };
    assert!(!env.contains_key("TOKEN"));
}

#[tokio::test(start_paused = true)]
async fn preparation_deadline_awaits_cleanup_for_gui_and_new_run() {
    for gui in [true, false] {
        let root = project("");
        approve(root.path(), &ledger(root.path()));
        let entered = Arc::new(tokio::sync::Notify::new());
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let factory = Arc::new(BlockingFactory {
            entered: entered.clone(),
            shutdowns: shutdowns.clone(),
        });
        let service = Arc::new(
            ProjectMcpService::new(
                unavailable(),
                ledger(root.path()),
                Arc::new(NetworkGuard::new().unwrap()),
            )
            .with_transport_factories(factory.clone(), factory),
        );
        let request = request(root.path(), Role::Worker);
        let scope = scope(root.path());
        let revision = config::mcp::load_project(root.path()).unwrap().revision;
        let task = tokio::spawn({
            let service = service.clone();
            async move {
                if gui {
                    assert_eq!(
                        service
                            .test_connection(scope, revision, CancelContext::default())
                            .await
                            .unwrap_err(),
                        McpErrorKind::Timeout
                    )
                } else {
                    let lease = service.prepare_new_run(request).await.unwrap();
                    assert!(lease.snapshot().tools.is_empty());
                    assert_eq!(lease.statuses()[0].reason, Some(McpErrorKind::Timeout));
                }
            }
        });
        entered.notified().await;
        tokio::time::advance(std::time::Duration::from_millis(
            tools::mcp::types::PREPARATION_TIMEOUT_MS,
        ))
        .await;
        task.await.unwrap();
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        let slots: Vec<_> = (0..4)
            .map(|_| service.limiter().try_acquire(root.path()).unwrap())
            .collect();
        drop(slots);
    }
}

#[tokio::test]
async fn gui_secret_delete_reports_committed_success_even_if_a_later_config_reload_fails() {
    let root = project("");
    let store = Arc::new(MemorySecrets::default());
    let service = service(
        root.path(),
        Arc::new(Factory::default()),
        McpCredentialBackend::Available(store.clone()),
    );
    service
        .save_secret(secret_request(root.path(), "removed"))
        .await
        .unwrap();
    let path = root.path().join(".evorch/mcp.toml");
    *store.after_delete.lock().unwrap() = Some(Box::new(move || {
        std::fs::write(path, "broken external edit [").unwrap()
    }));
    let result = service
        .delete_secret(delete_request(root.path()))
        .await
        .unwrap();
    assert_eq!(result.keychain_error, None);
    assert!(store.values.lock().unwrap().is_empty());
    assert!(!result.view.config.preparation_blocked);
    let config::mcp::McpTransportDefinition::Stdio { env, .. } =
        &result.view.config.servers["fixture"]
            .definition
            .as_ref()
            .unwrap()
            .transport
    else {
        panic!("stdio")
    };
    assert!(!env.contains_key("TOKEN"));
    assert!(
        config::mcp::load_project(root.path())
            .unwrap()
            .preparation_blocked
    );
}
