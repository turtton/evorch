//! Known-snapshot service for dispatch tests. The real preparation pipeline is
//! covered separately; scope denial must precede even a lazy connection here.
use super::*;
use async_trait::async_trait;
use runtime::mcp::{McpRunService, PrepareMcpRun, RestoreMcpRun, RunMcpLease};
use tools::mcp::{
    McpClient, McpClientInfo, McpError, McpErrorKind, McpRegistryConnector, McpRegistrySession,
    McpServerTools, McpToolDefinition, McpToolRegistry, McpToolResult, McpToolTransport,
    approval::{ApprovalLedger, inspect_http},
    transport::{CancelContext, TransportKind, http::HttpTransport},
    types::{
        ApprovalScope, ConfirmMcpApproval, FrozenMcpSnapshot, FrozenMcpTool, McpApprovalInspection,
        McpApprovalStore, ResolvedMcpServer, ResolvedMcpTransport,
    },
};

pub(super) struct Service {
    registry: Arc<McpToolRegistry>,
    pub root: std::path::PathBuf,
    _dir: tempfile::TempDir,
}
impl Service {
    pub async fn new(guard: Arc<NetworkGuard>, endpoint: String) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".evorch")).unwrap();
        std::fs::write(
            root.join(".evorch/mcp.toml"),
            format!("version = 1\n[servers.fixture]\ntype = \"http\"\nurl = {endpoint:?}\n"),
        )
        .unwrap();
        let config = config::mcp::load_project(&root).unwrap();
        let definition = config.servers["fixture"].definition.as_ref().unwrap();
        let inspection = guard.inspect_mcp_destination(&endpoint).await.unwrap();
        let inspection = inspect_http(
            ApprovalScope {
                canonical_root: root.clone(),
                server_id: "fixture".into(),
            },
            config.revision,
            definition,
            &url::Url::parse(&endpoint).unwrap(),
            inspection.ips,
        )
        .unwrap();
        let approvals = ApprovalLedger::at_user_config_dir(root.join("user-config")).unwrap();
        let preview = approvals.preview(&inspection).unwrap();
        approvals
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
        let tool =
            serde_json::from_value(json!({"name":"read","inputSchema":{"type":"object"}})).unwrap();
        let snapshot = FrozenMcpSnapshot::from_servers([McpServerTools {
            server_id: "fixture".into(),
            transport: tools::mcp::McpToolTransport::Http,
            approval_fingerprint: inspection.fingerprint,
            definitions: vec![tool],
        }])
        .unwrap();
        let connector = Arc::new(Connector {
            guard,
            endpoint,
            inspection,
            approvals,
        });
        let registry = Arc::new(McpToolRegistry::from_snapshot(snapshot, connector).unwrap());
        Self {
            registry,
            root,
            _dir: dir,
        }
    }
}
#[async_trait]
impl McpRunService for Service {
    async fn prepare_new_run(&self, _: PrepareMcpRun) -> Result<RunMcpLease, McpErrorKind> {
        RunMcpLease::from_registry(self.registry.clone())
    }
    fn restore_snapshot(&self, _: RestoreMcpRun) -> Result<RunMcpLease, McpErrorKind> {
        Err(McpErrorKind::Unavailable)
    }
}
struct Connector {
    guard: Arc<NetworkGuard>,
    endpoint: String,
    inspection: McpApprovalInspection,
    approvals: ApprovalLedger,
}
#[async_trait]
impl McpRegistryConnector for Connector {
    fn tool_transport(&self, _: &str) -> McpToolTransport {
        McpToolTransport::Http
    }
    async fn check_execution(&self, _: &FrozenMcpTool) -> Result<(), McpError> {
        self.approvals
            .check(&self.inspection)
            .map(|_| ())
            .map_err(error)
    }
    async fn connect(&self, _: &FrozenMcpTool) -> Result<Arc<dyn McpRegistrySession>, McpError> {
        let permit = self.approvals.check(&self.inspection).map_err(error)?;
        let destination = self
            .guard
            .approve_mcp_destination(&self.endpoint, &permit, &self.inspection.approved_ips)
            .await
            .map_err(error)?;
        let cancel = CancelContext::default();
        let transport = HttpTransport::connect(
            self.guard.clone(),
            ResolvedMcpServer {
                scope: self.inspection.scope.clone(),
                fingerprint: self.inspection.fingerprint,
                transport: ResolvedMcpTransport::Http {
                    url: sandbox::Secret::from(self.endpoint.clone()),
                    headers: Default::default(),
                    token: None,
                    destination,
                },
                startup_timeout: Duration::from_secs(60),
                request_timeout: Duration::from_secs(60),
            },
            permit,
            cancel.clone(),
        )?;
        let mut client = McpClient::connect_transport_with_timeouts(
            transport,
            "fixture".into(),
            TransportKind::Http,
            McpClientInfo {
                name: "test".into(),
                version: "1".into(),
            },
            Duration::from_secs(60),
            Duration::from_secs(60),
            cancel,
        )
        .await?;
        let definitions = client.list_tools().await?;
        Ok(Arc::new(Session {
            client: tokio::sync::Mutex::new(client),
            definitions,
        }))
    }
}
struct Session {
    client: tokio::sync::Mutex<McpClient>,
    definitions: Vec<McpToolDefinition>,
}
#[async_trait]
impl McpRegistrySession for Session {
    fn definitions(&self) -> &[McpToolDefinition] {
        &self.definitions
    }
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(McpToolResult, serde_json::Value), McpError> {
        let mut client = self.client.lock().await;
        let result = client.call_tool(name, args).await?;
        Ok((result, client.call_success_detail()))
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
