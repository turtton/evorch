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
    deletes: AtomicUsize,
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
        self.values.lock().unwrap().remove(key);
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
            slot: Mutex::new(Some(
                limiter
                    .try_acquire(&server.scope.canonical_root)
                    .map_err(error)?,
            )),
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
            Transport(Mutex::new(None)).exchange(request, cancel).await
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
