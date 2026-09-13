#[path = "../../tools/tests/common/mod.rs"]
mod common;

use agents::{NetworkAccess, Role};
use common::{FixtureServer, response_with_status};
use event_bus::{EventBus, EventKind, ToolEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, RunConfig, RuntimeError};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tools::{DnsResolver, NetworkGuard, NetworkGuardError, ToolExecutor};

struct Resolver(std::net::IpAddr);
#[async_trait::async_trait]
impl DnsResolver for Resolver {
    async fn resolve(&self, _: &str) -> Result<Vec<std::net::IpAddr>, NetworkGuardError> {
        Ok(vec![self.0])
    }
}

struct Model(AtomicUsize);
#[async_trait::async_trait]
impl AgentModel for Model {
    fn selected_model(&self, _: Role) -> String {
        "fixture".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![if first {
                    ContentBlock::ToolUse {
                        id: "mcp-call".into(),
                        name: "read".into(),
                        input: json!({}),
                    }
                } else {
                    ContentBlock::Text {
                        text: "done".into(),
                    }
                }],
            },
            finish_reason: if first {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
            usage: Usage::default(),
        })
    }
}

async fn scenario(
    role: Role,
    failure: bool,
    approval: Option<bool>,
) -> (FixtureServer, Vec<event_bus::Event>, String) {
    // Given: a fresh HTTPS server logging every request, and a dormant MCP registration.
    let count = AtomicUsize::new(0);
    let server = FixtureServer::start(move |_| {
        let body = match count.fetch_add(1, Ordering::SeqCst) {
            0 => json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}),
            1 => return response_with_status("202 Accepted", &[], b""),
            2 => json!({"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"read","inputSchema":{"type":"object"}}]}}),
            _ if failure => json!({"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"secret-body-token"}}),
            _ => json!({"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"mcp-ok"}]}}),
        };
        response_with_status("200 OK", &["Content-Type: application/json".into()], body.to_string().as_bytes())
    }).await.expect("fixture");
    let guard = Arc::new(NetworkGuard::with_resolver_and_root_certificate(
        Arc::new(Resolver(server.resolver_addr())),
        server.certificate(),
    ));
    let registry = Arc::new(tools::mcp::McpToolRegistry::new(
        guard,
        tools::mcp::McpClientConfig {
            server_label: "fixture".into(),
            endpoint: server.url("/mcp"),
            extra_headers: Default::default(),
            timeout: Duration::from_secs(2),
        },
        tools::mcp::McpClientInfo {
            name: "test".into(),
            version: "1".into(),
        },
    ));
    let definition = serde_json::from_value(json!({"name":"read","inputSchema":{"type":"object"}}))
        .expect("definition");
    let bus = Arc::new(EventBus::new(128));
    let mut approvals = bus.subscribe();
    let approval_bus = bus.clone();
    let mut tasks = tokio::task::JoinSet::new();
    if let Some(approved) = approval {
        tasks.spawn(async move {
            loop {
                if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) =
                    approvals.recv().await.expect("approval").kind
                {
                    assert!(call_id.ends_with(":mcp-call"));
                    approval_bus.emit(event_bus::Event::new(ToolEvent::ApprovalResolved {
                        call_id,
                        approved,
                    }));
                    break;
                }
            }
        });
    }
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(registry.tool(definition)))
        .expect("register");
    if approval.is_some() {
        executor.set_policy(
            sandbox::ApprovalPolicy::standard(sandbox::ApprovalMode::OnRequest)
                .with_override("read", sandbox::PolicyDecision::Ask),
        );
    }
    let runtime = AgentRuntime::new(
        bus,
        Arc::new(executor),
        Arc::new(Model(AtomicUsize::new(0))),
    );
    // When: the actual runtime dispatches the model's MCP call.
    let run = runtime.delegate_background(
        role,
        "test".into(),
        RunConfig {
            network_access: NetworkAccess::Allowed,
            ..RunConfig::default()
        },
    );
    tokio::time::timeout(Duration::from_secs(5), runtime.wait(run))
        .await
        .expect("bounded run")
        .expect("run");
    while let Some(result) = tasks.join_next().await {
        result.expect("approval task");
    }
    let mut events = Vec::new();
    // Runtime termination is the synchronization barrier; no tool task remains.
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(10), receiver.recv()).await
    {
        events.push(event);
    }
    (server, events, run.to_string())
}

#[tokio::test]
async fn denies_without_communication_when_role_has_no_network_grant() {
    let (server, events, run) = scenario(Role::Worker, false, None).await;
    // Then: even initialize never reached the server, and denial is correlated.
    assert!(
        server.captured_requests().is_empty(),
        "denial must produce ZERO HTTP requests"
    );
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ExecutionDenied { call_id, reason, .. }) if call_id == "mcp-call" && reason == "role_network.network")));
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Diagnostic(d) if d.call_id.as_deref() == Some("mcp-call") && d.run_id.as_deref() == Some(&run) && d.detail.contains("role_network") && d.detail.contains("network"))));
}

#[tokio::test]
async fn completes_with_correlation_when_scope_allows() -> common::TestResult {
    let (server, events, run) = scenario(Role::Librarian, false, None).await;
    // Then: initialize, initialized, list and call all happen, with one lifecycle pair.
    assert_eq!(server.captured_requests().len(), 4);
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolStarted { call_id, run_id, .. }) if call_id == "mcp-call" && run_id.as_deref() == Some(&run))));
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolCompleted { call_id, run_id, is_error: false, output, .. }) if call_id == "mcp-call" && run_id.as_deref() == Some(&run) && output.as_deref() == Some("mcp-ok"))));
    Ok(())
}

#[tokio::test]
async fn sanitizes_completed_detail_when_server_rejects() {
    let (_, events, run) = scenario(Role::Librarian, true, None).await;
    // Then: the executor emits an error result with only the MCP metadata allowlist.
    let (output, detail) = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Tool(ToolEvent::ToolCompleted {
                call_id,
                run_id,
                is_error: true,
                output,
                detail: Some(detail),
                ..
            }) if call_id == "mcp-call" && run_id.as_deref() == Some(&run) => {
                Some((output, detail))
            }
            _ => None,
        })
        .expect("MCP error completion");
    assert_eq!(
        detail
            .as_object()
            .expect("detail")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["class", "method", "request_id", "server", "status"]
    );
    assert_eq!(detail["class"], "server_rejected");
    assert!(!format!("{output:?}{detail}").contains("secret-body-token"));
}

#[tokio::test]
async fn executes_once_when_scope_approval_is_granted() {
    // Given/When: a per-tool Ask is approved through the real event bus.
    let (server, events, _) = scenario(Role::Librarian, false, Some(true)).await;
    // Then: one approval, one successful MCP call, no second executor approval.
    assert_eq!(server.captured_requests().len(), 4);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.kind,
                EventKind::Tool(ToolEvent::ApprovalRequested { .. })
            ))
            .count(),
        1
    );
    assert!(events.iter().any(|event| matches!(
        event.kind,
        EventKind::Tool(ToolEvent::ToolCompleted {
            is_error: false,
            ..
        })
    )));
}

#[tokio::test]
async fn denies_without_communication_when_scope_approval_is_refused() {
    // Given/When: a per-tool Ask is rejected through the real event bus.
    let (server, events, _) = scenario(Role::Librarian, false, Some(false)).await;
    // Then: refusal precedes even the first initialize request.
    assert!(server.captured_requests().is_empty());
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Diagnostic(d) if d.detail == "per_tool_policy.network" && d.call_id.as_deref() == Some("mcp-call"))));
}
