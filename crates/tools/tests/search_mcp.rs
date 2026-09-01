mod common;

use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use tools::{
    DnsResolver, McpTransport, NetworkGuard, NetworkGuardError, NetworkGuardMcpTransport,
    SearchError,
};

use common::{FixtureServer, TestResult, response_with_status};

struct CountingResolver {
    addr: IpAddr,
    calls: AtomicUsize,
}

#[async_trait]
impl DnsResolver for CountingResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![self.addr])
    }
}

fn guard(server: &FixtureServer) -> Arc<NetworkGuard> {
    Arc::new(NetworkGuard::with_resolver_and_root_certificate(
        Arc::new(CountingResolver {
            addr: server.resolver_addr(),
            calls: AtomicUsize::new(0),
        }),
        server.certificate(),
    ))
}

fn extra_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-provider-key", HeaderValue::from_static("fixture-key"));
    headers
}

// Given: JSON 応答を返す fixture / When: call_tool を 1 回呼ぶ / Then: 単発の JSON-RPC envelope・Accept・provider header が wire に乗り、text が返る
#[tokio::test]
async fn posts_single_jsonrpc_call_with_accept_and_extra_headers() -> TestResult {
    let server = FixtureServer::start(|_path| {
        response_with_status(
            "200 OK",
            &["Content-Type: application/json".to_owned()],
            br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"fixture text"}]}}"#,
        )
    })
    .await?;
    let transport =
        NetworkGuardMcpTransport::new(guard(&server), server.url("/mcp"), extra_headers());

    let success = transport
        .call_tool("web_search_test", json!({"query": "rust"}))
        .await?;

    assert_eq!(success.text, "fixture text");
    let captured = server.captured_requests();
    assert_eq!(
        captured.len(),
        1,
        "単発 design: request は 1 回だけ送られる"
    );
    let request = String::from_utf8(captured.into_iter().next().expect("1 件記録済み"))?;
    assert!(request.starts_with("POST /mcp "));
    let (head, body_text) = request
        .split_once("\r\n\r\n")
        .expect("request は header と body を持つ");
    assert!(
        head.to_ascii_lowercase()
            .contains("accept: application/json, text/event-stream")
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("x-provider-key: fixture-key")
    );
    let body: serde_json::Value = serde_json::from_str(body_text)?;
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    assert_eq!(body["method"], "tools/call");
    assert_eq!(body["params"]["name"], "web_search_test");
    assert_eq!(body["params"]["arguments"], json!({"query": "rust"}));
    Ok(())
}

// Given: SSE frame で JSON-RPC 応答を返す fixture / When: call_tool / Then: data frame の text が返る
#[tokio::test]
async fn parses_sse_framed_response_from_fixture() -> TestResult {
    let sse = concat!(
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"sse text\"}]}}\n",
        "\n",
    );
    let server = FixtureServer::start(move |_path| {
        response_with_status(
            "200 OK",
            &["Content-Type: text/event-stream".to_owned()],
            sse.as_bytes(),
        )
    })
    .await?;
    let transport =
        NetworkGuardMcpTransport::new(guard(&server), server.url("/mcp"), HeaderMap::new());

    let success = transport.call_tool("t", json!({})).await?;

    assert_eq!(success.text, "sse text");
    Ok(())
}

// Given: 429 を返す fixture / When: call_tool / Then: HttpStatus(429) かつ fallback trigger になる
#[tokio::test]
async fn maps_429_to_fallback_trigger_status_error() -> TestResult {
    let server = FixtureServer::start(|_path| {
        response_with_status("429 Too Many Requests", &[], b"rate limited")
    })
    .await?;
    let transport =
        NetworkGuardMcpTransport::new(guard(&server), server.url("/mcp"), HeaderMap::new());

    let error = transport
        .call_tool("t", json!({}))
        .await
        .expect_err("429 は HttpStatus error になる");

    assert!(matches!(error, SearchError::HttpStatus(429)));
    assert!(error.is_fallback_trigger());
    Ok(())
}

// Given: 500 を返す fixture / When: call_tool / Then: HttpStatus(500) かつ fallback trigger になる
#[tokio::test]
async fn maps_500_to_fallback_trigger_status_error() -> TestResult {
    let server = FixtureServer::start(|_path| {
        response_with_status("500 Internal Server Error", &[], b"boom")
    })
    .await?;
    let transport =
        NetworkGuardMcpTransport::new(guard(&server), server.url("/mcp"), HeaderMap::new());

    let error = transport
        .call_tool("t", json!({}))
        .await
        .expect_err("500 は HttpStatus error になる");

    assert!(matches!(error, SearchError::HttpStatus(500)));
    assert!(error.is_fallback_trigger());
    Ok(())
}

// Given: 400 を返す fixture / When: call_tool / Then: HttpStatus(400) で fallback trigger にならない
#[tokio::test]
async fn maps_400_to_non_trigger_status_error() -> TestResult {
    let server =
        FixtureServer::start(|_path| response_with_status("400 Bad Request", &[], b"bad request"))
            .await?;
    let transport =
        NetworkGuardMcpTransport::new(guard(&server), server.url("/mcp"), HeaderMap::new());

    let error = transport
        .call_tool("t", json!({}))
        .await
        .expect_err("400 は HttpStatus error になる");

    assert!(matches!(error, SearchError::HttpStatus(400)));
    assert!(!error.is_fallback_trigger());
    Ok(())
}
