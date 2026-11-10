#[path = "../../../tools/tests/common/mod.rs"]
mod common;
#[path = "mcp_scope_diagnostics/mod.rs"]
mod diagnostics;
use diagnostics::Failure;
#[path = "mcp_scope_diagnostics/service.rs"]
mod service;

use agents::Role;
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
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
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
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![if first {
                    ContentBlock::ToolUse {
                        id: "mcp-call".into(),
                        name: "mcp__fixture__read".into(),
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
    failure: Failure,
    approval: Option<bool>,
) -> (FixtureServer, Vec<event_bus::Event>, String) {
    scenario_with_policy(role, failure, approval, sandbox::PolicyDecision::AutoAllow).await
}

async fn scenario_with_policy(
    role: Role,
    failure: Failure,
    approval: Option<bool>,
    policy: sandbox::PolicyDecision,
) -> (FixtureServer, Vec<event_bus::Event>, String) {
    // Given: a fresh HTTPS server logging every request, and a dormant MCP registration.
    let count = AtomicUsize::new(0);
    let server = FixtureServer::start(move |_| {
        let index = count.fetch_add(1, Ordering::SeqCst);
        if failure == Failure::Http(index) {
            return response_with_status("503 Service Unavailable", &[], b"secret-body-token");
        }
        let body = match index {
            0 => json!({"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}}),
            1 => json!({"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"read","inputSchema":{"type":"object"}}]}}),
            _ if failure == Failure::Server => json!({"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"secret-body-token"}}),
            _ => json!({"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","content":[{"type":"text","text":"mcp-ok"}]}}),
        };
        response_with_status("200 OK", &["Content-Type: application/json".into()], body.to_string().as_bytes())
    }).await.expect("fixture");
    let guard = Arc::new(NetworkGuard::with_resolver_and_root_certificate(
        Arc::new(Resolver(server.resolver_addr())),
        server.certificate(),
    ));
    let service = Arc::new(service::Service::new(guard, server.url("/mcp")).await);
    let root = service.root.clone();
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
                    assert!(call_id.contains(":mcp-call:"));
                    sandbox::approval::ApprovalRegistry::for_bus(&approval_bus)
                        .resolve_presented(
                            &(call_id),
                            approved,
                            sandbox::approval::ApprovalProvenance::CLI,
                        )
                        .expect("resolve live approval");
                    break;
                }
            }
        });
    }
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.set_policy(
        sandbox::ApprovalPolicy::standard(sandbox::ApprovalMode::OnRequest).with_override(
            "mcp__fixture__read",
            if approval.is_some() {
                sandbox::PolicyDecision::Ask
            } else {
                policy
            },
        ),
    );
    let runtime = AgentRuntime::new(
        bus,
        Arc::new(executor),
        Arc::new(Model(AtomicUsize::new(0))),
    )
    .with_mcp_service(service);
    // When: the actual runtime dispatches the model's MCP call.
    let run = runtime.delegate_background(
        role,
        "test".into(),
        RunConfig {
            project_root: Some(root),
            ..RunConfig::default()
        },
    );
    runtime.wait(run).await.expect("run");
    while let Some(result) = tasks.join_next().await {
        result.expect("approval task");
    }
    // Terminal lifecycle is the synchronization barrier, never a timeout.
    let events = receiver.drain_pending_snapshot();
    (server, events, run.to_string())
}

#[tokio::test]
async fn denies_without_communication_when_tool_policy_denies() {
    let (server, events, run) = scenario_with_policy(
        Role::Worker,
        Failure::None,
        None,
        sandbox::PolicyDecision::Deny,
    )
    .await;
    // Then: even initialize never reached the server, and denial is correlated.
    assert!(
        server.captured_requests().is_empty(),
        "denial must produce ZERO HTTP requests"
    );
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ExecutionDenied { call_id, reason, .. }) if call_id == "mcp-call" && reason == "per_tool_policy.network")));
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Diagnostic(d) if d.call_id.as_deref() == Some("mcp-call") && d.run_id.as_deref() == Some(&run) && d.detail.contains("per_tool_policy") && d.detail.contains("network"))));
}

#[tokio::test]
async fn worker_mcp_network_call_completes_when_tool_policy_allows() -> common::TestResult {
    let (server, events, run) = scenario(Role::Worker, Failure::None, None).await;
    // Then: discover, list and call happen, with one lifecycle pair.
    assert_eq!(server.captured_requests().len(), 3);
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolStarted { call_id, run_id, .. }) if call_id == "mcp-call" && run_id.as_deref() == Some(&run))));
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolCompleted { call_id, run_id, is_error: false, output, .. }) if call_id == "mcp-call" && run_id.as_deref() == Some(&run) && output.as_deref() == Some("mcp-ok"))));
    Ok(())
}

#[tokio::test]
async fn sanitizes_completed_detail_when_server_rejects() {
    let (_, events, run) = scenario(Role::WebResearcher, Failure::Server, None).await;
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
    let (server, events, _) = scenario(Role::WebResearcher, Failure::None, Some(true)).await;
    // Then: one approval, one successful MCP call, no second executor approval.
    assert_eq!(server.captured_requests().len(), 3);
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
    let (server, events, _) = scenario(Role::WebResearcher, Failure::None, Some(false)).await;
    // Then: refusal precedes even the first initialize request.
    assert!(server.captured_requests().is_empty());
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Diagnostic(d) if d.detail == "per_tool_policy.network" && d.call_id.as_deref() == Some("mcp-call"))));
}
