mod common;

use async_trait::async_trait;
use common::{FixtureServer, TestResult, response_with_status};
use serde_json::{Value, json};
use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tools::{
    DnsResolver, NetworkGuard, NetworkGuardError,
    mcp::{McpClient, McpClientConfig, McpClientInfo, McpErrorKind},
};

struct Resolver(IpAddr);
#[async_trait]
impl DnsResolver for Resolver {
    async fn resolve(&self, _: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        Ok(vec![self.0])
    }
}

fn config(server: &FixtureServer) -> McpClientConfig {
    McpClientConfig {
        server_label: "fixture".into(),
        endpoint: server.url("/mcp"),
        extra_headers: Default::default(),
        timeout: Duration::from_secs(1),
    }
}

fn guard(server: &FixtureServer) -> Arc<NetworkGuard> {
    Arc::new(NetworkGuard::with_resolver_and_root_certificate(
        Arc::new(Resolver(server.resolver_addr())),
        server.certificate(),
    ))
}

fn info() -> McpClientInfo {
    McpClientInfo {
        name: "test".into(),
        version: "1".into(),
    }
}

// Given: SSE pages and tool responses / When: a session runs / Then: ids increase and pagination is consumed.
#[tokio::test]
async fn uses_unique_ids_across_paginated_list_and_calls() -> TestResult {
    let sequence = AtomicUsize::new(0);
    let server = FixtureServer::start(move |_| {
        let (id, result) = match sequence.fetch_add(1, Ordering::SeqCst) {
            0 => (1, json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"test","version":"1"}})),
            1 => return response_with_status("202 Accepted", &[], b""),
            2 => (2, json!({"tools":[],"nextCursor":"page2"})),
            3 => (3, json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})),
            4 => (4, json!({"content":[{"type":"text","text":"first"}]})),
            _ => (5, json!({"content":[{"type":"text","text":"second"}]})),
        };
        let body = format!("data: {}\n\n", json!({"jsonrpc":"2.0","id":id,"result":result}));
        response_with_status("200 OK", &["Content-Type: text/event-stream".into()], body.as_bytes())
    }).await?;
    let mut client = McpClient::connect(guard(&server), config(&server), info()).await?;
    let tools = client.list_tools().await?;
    let first = client.call_tool("echo", json!({})).await?;
    let second = client.call_tool("echo", json!({})).await?;
    assert_eq!(tools.len(), 1);
    assert_eq!(first.text(), "first");
    assert_eq!(second.text(), "second");
    let bodies: Vec<Value> = server
        .captured_requests()
        .iter()
        .map(|request| {
            let text = std::str::from_utf8(request).expect("utf8");
            serde_json::from_str(text.split_once("\r\n\r\n").expect("body").1).expect("json")
        })
        .collect();
    assert_eq!(
        bodies
            .iter()
            .filter_map(|body| body["id"].as_i64())
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(bodies[3]["params"]["cursor"], "page2");
    Ok(())
}

struct PendingResolver;
#[async_trait]
impl DnsResolver for PendingResolver {
    async fn resolve(&self, _: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        std::future::pending().await
    }
}

// Given: DNS never completes / When: connect reaches its deadline / Then: sanitized timeout includes correlation.
#[tokio::test]
async fn bounds_initialization_when_dns_stalls() -> TestResult {
    let server = FixtureServer::start(|_| response_with_status("200 OK", &[], b"{}")).await?;
    let guard = Arc::new(NetworkGuard::with_resolver_and_root_certificate(
        Arc::new(PendingResolver),
        server.certificate(),
    ));
    let mut config = config(&server);
    config.timeout = Duration::from_millis(10);
    let result = McpClient::connect(guard, config, info()).await;
    let error = result.err().expect("timeout");
    assert_eq!(error.kind, McpErrorKind::Timeout);
    assert_eq!(error.request_id, Some(1));
    assert_eq!(error.method, "initialize");
    Ok(())
}
