//! Metadata-only frozen MCP fixture: never constructs a network or process backend.
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tools::{
    Tool,
    mcp::{
        McpError, McpErrorKind, McpRegistryConnector, McpRegistrySession, McpServerTools,
        McpToolDefinition, McpToolRegistry, McpToolTransport,
        types::{ApprovalFingerprint, FrozenMcpSnapshot, FrozenMcpTool},
    },
};

#[derive(Default)]
pub struct InertConnector {
    checks: AtomicUsize,
    connections: AtomicUsize,
}
impl InertConnector {
    pub fn assert_unused(&self) {
        assert_eq!(
            self.checks.load(Ordering::SeqCst),
            0,
            "metadata must not inspect execution authority"
        );
        assert_eq!(
            self.connections.load(Ordering::SeqCst),
            0,
            "metadata must not connect"
        );
    }
}
#[async_trait]
impl McpRegistryConnector for InertConnector {
    fn tool_transport(&self, _: &str) -> McpToolTransport {
        McpToolTransport::Http
    }
    async fn check_execution(&self, _: &FrozenMcpTool) -> Result<(), McpError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn connect(&self, tool: &FrozenMcpTool) -> Result<Arc<dyn McpRegistrySession>, McpError> {
        self.connections.fetch_add(1, Ordering::SeqCst);
        Err(McpError {
            server: tool.remote.server_id.clone(),
            method: "tools/call",
            request_id: None,
            kind: McpErrorKind::Unavailable,
        })
    }
}

pub struct DormantMcp {
    pub registry: Arc<McpToolRegistry>,
    pub connector: Arc<InertConnector>,
}
impl DormantMcp {
    pub fn new(definition: McpToolDefinition) -> Self {
        let snapshot = FrozenMcpSnapshot::from_servers([McpServerTools {
            server_id: "fixture".into(),
            approval_fingerprint: ApprovalFingerprint::from_projection(b"fixture"),
            definitions: vec![definition],
        }])
        .expect("snapshot");
        let connector = Arc::new(InertConnector::default());
        let registry = Arc::new(
            McpToolRegistry::from_snapshot(snapshot, connector.clone()).expect("registry"),
        );
        Self {
            registry,
            connector,
        }
    }
    pub fn tool(&self) -> Arc<dyn Tool> {
        let mut tools = self.registry.tools().expect("frozen tools");
        assert_eq!(tools.len(), 1);
        Arc::new(tools.remove(0))
    }
}
