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

async fn fixture(status: &'static str, body: &'static str) -> FixtureServer {
    let count = AtomicUsize::new(0);
    FixtureServer::start(move |_| {
        match count.fetch_add(1, Ordering::SeqCst) {
            0 => response_with_status("200 OK", &["Mcp-Session-Id: secret-session".into()], br#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}"#),
            1 => response_with_status("202 Accepted", &[], b""),
            _ => response_with_status(status, &["Content-Type: application/json".into()], body.as_bytes()),
        }
    }).await.expect("fixture")
}

async fn connect(server: &FixtureServer) -> Result<McpClient, tools::mcp::McpError> {
    let guard = Arc::new(NetworkGuard::with_resolver_and_root_certificate(
        Arc::new(Resolver(server.resolver_addr())),
        server.certificate(),
    ));
    McpClient::connect(
        guard,
        McpClientConfig {
            server_label: "fixture".into(),
            endpoint: server.url("/mcp"),
            extra_headers: Default::default(),
            timeout: Duration::from_secs(2),
        },
        McpClientInfo {
            name: "evorch-test".into(),
            version: "1".into(),
        },
    )
    .await
}

// Given: a tools page / When: list_tools / Then: typed definitions preserve schemas.
#[tokio::test]
async fn lists_definitions_when_server_returns_tools() -> TestResult {
    let server = fixture("200 OK", r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}"#).await;
    let mut client = connect(&server).await?;
    let definitions = client.list_tools().await?;
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "echo");
    assert_eq!(definitions[0].description.as_deref(), Some("Echo"));
    assert_eq!(
        Value::Object(definitions[0].input_schema.clone()),
        json!({"type":"object","properties":{"text":{"type":"string"}}})
    );
    Ok(())
}

// Given: text result / When: call_tool / Then: text survives the real HTTP round trip.
#[tokio::test]
async fn returns_content_when_tool_call_succeeds() -> TestResult {
    let server = fixture(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hello"}]}}"#,
    )
    .await;
    let mut client = connect(&server).await?;
    let result = client.call_tool("echo", json!({"text":"hello"})).await?;
    assert_eq!(result.text(), "hello");
    Ok(())
}

// Given: secret-bearing failures / When: list_tools / Then: only classified detail escapes.
#[tokio::test]
async fn sanitizes_details_when_response_fails() -> TestResult {
    for (status, body, kind) in [
        (
            "500 Internal Server Error",
            "secret-body",
            McpErrorKind::HttpStatus(500),
        ),
        ("200 OK", "secret-body", McpErrorKind::Protocol),
        (
            "200 OK",
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"secret-body"}}"#,
            McpErrorKind::ServerRejected,
        ),
        (
            "200 OK",
            r#"{"jsonrpc":"2.0","id":999,"result":{"tools":[]}}"#,
            McpErrorKind::Protocol,
        ),
    ] {
        let server = fixture(status, body).await;
        let mut client = connect(&server).await?;
        let error = client.list_tools().await.expect_err("fail closed");
        assert_eq!(error.kind, kind);
        let detail = error.detail();
        let keys: Vec<_> = detail
            .as_object()
            .expect("detail object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["class", "method", "request_id", "server", "status"]
        );
        assert_eq!(detail["server"], "fixture");
        assert_eq!(detail["method"], "tools/list");
        assert_eq!(detail["request_id"], 2);
        assert!(!format!("{error:?}{error}{detail}").contains("secret"));
    }
    Ok(())
}

// Given: fresh client / When: connect / Then: initialization precedes notification and session headers propagate.
#[tokio::test]
async fn initializes_before_sending_ready_notification() -> TestResult {
    let server = fixture("200 OK", "{}").await;
    let _client = connect(&server).await?;
    let requests = server.captured_requests();
    assert_eq!(requests.len(), 2);
    let bodies: Vec<Value> = requests
        .iter()
        .map(|request| {
            let text = std::str::from_utf8(request).expect("utf8");
            serde_json::from_str(text.split_once("\r\n\r\n").expect("body").1).expect("json")
        })
        .collect();
    assert_eq!(bodies[0]["params"]["protocolVersion"], "2025-03-26");
    assert_eq!(
        bodies[0]["params"]["clientInfo"],
        json!({"name":"evorch-test","version":"1"})
    );
    assert_eq!(bodies[1]["method"], "notifications/initialized");
    assert!(bodies[1].get("id").is_none());
    assert!(
        String::from_utf8_lossy(&requests[1])
            .to_ascii_lowercase()
            .contains("mcp-session-id: secret-session")
    );
    Ok(())
}
