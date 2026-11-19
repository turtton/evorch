//! Metadata-only MCP: there is deliberately no server configuration, endpoint,
//! command, live session or transport factory in either process. Ready + two
//! successful turns prove disconnected frozen definitions survive without a
//! connection/spawn dependency. This does NOT observe zero connection attempts
//! when configured servers exist; the production disposable-preview-bus parity
//! test frozen_mcp_preview_matches_live_registration_pipeline_without_discovery_or_events
//! in runtime::agent_loop::tool_catalog covers inert registration separately.
use async_trait::async_trait;
use runtime::mcp::{
    McpErrorKind, McpHostServices, McpRunService, PrepareMcpRun, RestoreMcpRun, RunMcpLease,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tools::mcp::{FrozenMcpSnapshot, McpServerTools, McpToolTransport, types::ApprovalFingerprint};

pub(super) fn snapshot() -> FrozenMcpSnapshot {
    FrozenMcpSnapshot::from_servers([
        ("disconnected_stdio", McpToolTransport::Stdio),
        ("disconnected_http", McpToolTransport::Http),
    ].map(|(server_id, transport)| McpServerTools {
        server_id: server_id.into(), transport,
        approval_fingerprint: ApprovalFingerprint([17; 32]),
        definitions: vec![serde_json::from_value(json!({
            "name":"evidence", "description":"Frozen disconnected evidence",
            "inputSchema":{"type":"object", "properties":{"index":{"type":"integer"}}, "required":["index"]}
        })).unwrap()],
    })).unwrap()
}
struct FrozenService(Arc<AtomicUsize>);
#[async_trait]
impl McpRunService for FrozenService {
    async fn prepare_new_run(&self, _: PrepareMcpRun) -> Result<RunMcpLease, McpErrorKind> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(RunMcpLease::restore_snapshot(snapshot()))
    }
    fn restore_snapshot(&self, request: RestoreMcpRun) -> Result<RunMcpLease, McpErrorKind> {
        Ok(RunMcpLease::restore_snapshot(request.snapshot))
    }
}
pub(super) fn services(config_dir: &Path) -> (McpHostServices, Arc<AtomicUsize>) {
    let mut services = McpHostServices::at_config_dir(
        runtime::mcp::McpCredentialBackend::Unavailable(
            runtime::mcp::SecretUnavailableReason::Unavailable,
        ),
        config_dir.to_path_buf(),
    );
    let prepared = Arc::new(AtomicUsize::new(0));
    services.runs = Arc::new(FrozenService(prepared.clone()));
    (services, prepared)
}
pub(super) fn is_external(name: &str) -> bool {
    snapshot().tools.iter().any(|tool| tool.public_name == name)
}
pub(super) fn assert_frozen_wire(requests: &[Value]) {
    let expected = snapshot();
    assert_eq!(expected.tools.len(), 2);
    let select = |request: &Value| {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| is_external(tool["function"]["name"].as_str().unwrap()))
            .cloned()
            .collect::<Vec<_>>()
    };
    let original = select(&requests[0]);
    assert_eq!(
        original.len(),
        expected.tools.len(),
        "predecessor actually sent frozen MCP tools"
    );
    for request in requests.iter().skip(1) {
        assert_eq!(
            select(request),
            original,
            "preserve all MCP names, descriptions, schemas and order"
        );
    }
}
