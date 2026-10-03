mod common;

use std::{
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tools::{
    ExaKeylessProvider, McpToolSuccess, McpTransport, NetworkGuard, NetworkGuardError,
    NetworkGuardMcpTransport, SearchError, SearchOptions, SearchProvider, TavilyKeylessProvider,
};

use common::{FixtureServer, TestResult, response_with_status};

/// Exa formatter 形式の golden fixture（`Title:` block を `\n\n---\n\n` で連結）。
const EXA_GOLDEN: &str = "Title: Evorch release notes\nURL: https://example.com/a\nPublished: 2026-01-01\nAuthor: Jane Doe\nHighlights: first result text\n\n---\n\nTitle: Second result\nURL: https://example.com/b\nPublished: 2026-02-02\nAuthor: John Roe\nHighlights: second result text";

/// Tavily formatter 形式の golden fixture（`Answer:` + `Detailed Results:` + result block）。
const TAVILY_GOLDEN: &str = "Answer: Something about the query.\nDetailed Results:\n\nTitle: First\nURL: https://example.com/1\nContent: body of first\n\nTitle: Second\nURL: https://example.com/2\nContent: body of second";

/// Exa が空結果時に返す text。
const EXA_EMPTY_RESULTS: &str = "No search results found. Please try a different query.";

struct RecordedCall {
    tool_name: String,
    arguments: Value,
}

struct StubTransport {
    response: Result<McpToolSuccess, SearchError>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl StubTransport {
    fn success(text: &str, usage: Option<Value>) -> Self {
        Self {
            response: Ok(McpToolSuccess {
                text: text.to_owned(),
                usage,
            }),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn failure(error: SearchError) -> Self {
        Self {
            response: Err(error),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn last_call(&self) -> (String, Value) {
        let call = self
            .calls
            .lock()
            .expect("calls mutex")
            .pop()
            .expect("search が transport を 1 回呼ぶ");
        (call.tool_name, call.arguments)
    }
}

#[async_trait]
impl McpTransport for StubTransport {
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpToolSuccess, SearchError> {
        self.calls.lock().expect("calls mutex").push(RecordedCall {
            tool_name: tool_name.to_owned(),
            arguments,
        });
        self.response.clone()
    }
}

struct CountingResolver {
    addr: IpAddr,
    calls: AtomicUsize,
}

#[async_trait]
impl tools::DnsResolver for CountingResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![self.addr])
    }
}

// Given: Exa golden text を返す stub / When: max_results 付きで search / Then: tool 名と arguments が query + numResults shaping になり name は exa
#[tokio::test]
async fn exa_maps_query_and_max_results_to_num_results() -> TestResult {
    let stub = Arc::new(StubTransport::success(EXA_GOLDEN, None));
    let provider = ExaKeylessProvider::new(stub.clone());

    let results = provider
        .search(
            "evorch",
            &SearchOptions {
                max_results: Some(5),
            },
        )
        .await?;

    assert_eq!(results.content, EXA_GOLDEN);
    assert_eq!(provider.name(), "exa");
    let (tool_name, arguments) = stub.last_call();
    assert_eq!(tool_name, "web_search_exa");
    assert_eq!(arguments, json!({"query": "evorch", "numResults": 5}));
    Ok(())
}

// Given: Exa provider / When: max_results なしで search / Then: arguments は query だけになる
#[tokio::test]
async fn exa_omits_num_results_when_unset() -> TestResult {
    let stub = Arc::new(StubTransport::success(EXA_GOLDEN, None));
    let provider = ExaKeylessProvider::new(stub.clone());

    provider
        .search("evorch", &SearchOptions { max_results: None })
        .await?;

    let (tool_name, arguments) = stub.last_call();
    assert_eq!(tool_name, "web_search_exa");
    assert_eq!(arguments, json!({"query": "evorch"}));
    Ok(())
}

// Given: Tavily golden text を返す stub / When: max_results 付きで search / Then: tool 名と arguments が query + max_results shaping になり name は tavily
#[tokio::test]
async fn tavily_maps_query_and_max_results_to_max_results_key() -> TestResult {
    let stub = Arc::new(StubTransport::success(TAVILY_GOLDEN, None));
    let provider = TavilyKeylessProvider::new(stub.clone());

    let results = provider
        .search(
            "evorch",
            &SearchOptions {
                max_results: Some(5),
            },
        )
        .await?;

    assert_eq!(results.content, TAVILY_GOLDEN);
    assert_eq!(provider.name(), "tavily");
    let (tool_name, arguments) = stub.last_call();
    assert_eq!(tool_name, "tavily_search");
    assert_eq!(arguments, json!({"query": "evorch", "max_results": 5}));
    Ok(())
}

// Given: Exa golden text / When: search / Then: result_count は Title 行数の 2 になり request_id は None
#[tokio::test]
async fn exa_counts_golden_format_results() -> TestResult {
    let stub = Arc::new(StubTransport::success(EXA_GOLDEN, None));
    let provider = ExaKeylessProvider::new(stub);

    let results = provider
        .search("q", &SearchOptions { max_results: None })
        .await?;

    assert_eq!(results.result_count, 2);
    assert_eq!(results.request_id, None);
    Ok(())
}

// Given: Answer 接頭辞付き Tavily golden text / When: search / Then: result_count は 2 になる
#[tokio::test]
async fn tavily_counts_golden_format_results_with_answer_prefix() -> TestResult {
    let stub = Arc::new(StubTransport::success(TAVILY_GOLDEN, None));
    let provider = TavilyKeylessProvider::new(stub);

    let results = provider
        .search("q", &SearchOptions { max_results: None })
        .await?;

    assert_eq!(results.result_count, 2);
    Ok(())
}

// Given: Exa の空結果 text / When: search / Then: result_count は 0 になる
#[tokio::test]
async fn empty_results_text_counts_zero() -> TestResult {
    let stub = Arc::new(StubTransport::success(EXA_EMPTY_RESULTS, None));
    let provider = ExaKeylessProvider::new(stub);

    let results = provider
        .search("q", &SearchOptions { max_results: None })
        .await?;

    assert_eq!(results.result_count, 0);
    Ok(())
}

// Given: usage metadata を返す stub / When: search / Then: usage が SearchResults へ透過される
#[tokio::test]
async fn passes_usage_through_to_results() -> TestResult {
    let stub = Arc::new(StubTransport::success(
        EXA_GOLDEN,
        Some(json!({"searchTime": 42})),
    ));
    let provider = ExaKeylessProvider::new(stub);

    let results = provider
        .search("q", &SearchOptions { max_results: None })
        .await?;

    assert_eq!(results.usage, Some(json!({"searchTime": 42})));
    Ok(())
}

// Given: 429 を返す stub / When: search / Then: error は変換されず fallback trigger のまま伝播する
#[tokio::test]
async fn passes_fallback_trigger_error_through_unchanged() -> TestResult {
    let stub = Arc::new(StubTransport::failure(SearchError::HttpStatus(429)));
    let provider = ExaKeylessProvider::new(stub);

    let error = provider
        .search("q", &SearchOptions { max_results: None })
        .await
        .expect_err("429 はそのまま伝播する");

    assert!(matches!(error, SearchError::HttpStatus(429)));
    assert!(error.is_fallback_trigger());
    Ok(())
}

// Given: Tavily provider の extra header を載せた guarded transport / When: fixture に対して search / Then: X-Tavily-Access-Mode: keyless が wire に乗り result が組み上がる
#[tokio::test]
async fn tavily_provider_headers_reach_the_wire() -> TestResult {
    let server = FixtureServer::start(|_path| {
        response_with_status(
            "200 OK",
            &["Content-Type: application/json".to_owned()],
            br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"Title: Wire\nURL: https://example.com/w"}]}}"#,
        )
    })
    .await?;
    let guard = Arc::new(NetworkGuard::with_resolver_and_root_certificate(
        Arc::new(CountingResolver {
            addr: server.resolver_addr(),
            calls: AtomicUsize::new(0),
        }),
        server.certificate(),
    ));
    let transport = Arc::new(NetworkGuardMcpTransport::new(
        guard,
        server.url("/mcp/"),
        TavilyKeylessProvider::extra_headers(),
    ));
    let provider = TavilyKeylessProvider::new(transport);

    let results = provider
        .search("wire", &SearchOptions { max_results: None })
        .await?;

    assert_eq!(results.result_count, 1);
    let captured = server.captured_requests();
    assert_eq!(captured.len(), 1);
    let request = String::from_utf8(captured.into_iter().next().expect("1 件記録済み"))?;
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-tavily-access-mode: keyless")
    );
    Ok(())
}

struct StubResponsesTransport {
    response: Value,
    bodies: Mutex<Vec<Value>>,
}

#[async_trait]
impl tools::ResponsesTransport for StubResponsesTransport {
    async fn create_response(&self, body: &Value) -> Result<Value, SearchError> {
        self.bodies.lock().expect("bodies mutex").push(body.clone());
        Ok(self.response.clone())
    }
}

fn openai_response() -> Value {
    json!({
        "id": "resp_fixture", "status": "completed", "usage": {"input_tokens": 7, "output_tokens": 9},
        "output": [
            {"type": "web_search_call", "status": "completed"},
            {"type": "message", "content": [{"type": "output_text", "text": "An answer.", "annotations": [
                {"type": "url_citation", "title": "First", "url": "https://example.com/first"},
                {"type": "url_citation", "title": "Repeated", "url": "https://example.com/first"},
                {"type": "url_citation", "title": "Second", "url": "https://example.com/second"}
            ]}]}
        ]
    })
}

fn responses_transport(
    server: &FixtureServer,
) -> Result<tools::NetworkGuardResponsesTransport, reqwest::header::InvalidHeaderValue> {
    let guard = Arc::new(NetworkGuard::with_resolver_and_root_certificate(
        Arc::new(CountingResolver {
            addr: server.resolver_addr(),
            calls: AtomicUsize::new(0),
        }),
        server.certificate(),
    ));
    let credential = tools::OpenAiSearchCredential::resolve(|key| {
        (key == "OPENAI_API_KEY").then(|| "fixture-token-not-a-real-secret".to_owned())
    })
    .expect("fixture credential");
    tools::NetworkGuardResponsesTransport::new(guard, server.url("/v1/responses"), credential)
}

// Given: Responses stub / When: bounded search / Then: hosted web_search body、dedup と bound、usage が契約どおり
#[tokio::test]
async fn openai_shapes_public_request_and_bounds_unique_citations() -> TestResult {
    let stub = Arc::new(StubResponsesTransport {
        response: openai_response(),
        bodies: Mutex::new(Vec::new()),
    });
    let provider = tools::OpenAiResponsesProvider::new(stub.clone(), "chosen-model");

    let result = provider
        .search(
            "evorch",
            &SearchOptions {
                max_results: Some(1),
            },
        )
        .await?;

    assert_eq!(
        *stub.bodies.lock().expect("bodies mutex"),
        vec![json!({
            "model": "chosen-model", "tools": [{"type": "web_search", "search_context_size": "medium"}],
            "input": "evorch", "store": false
        })]
    );
    assert_eq!(
        result.content,
        "An answer.\n\nTitle: First\nURL: https://example.com/first"
    );
    assert_eq!(result.result_count, 1);
    assert_eq!(result.request_id.as_deref(), Some("resp_fixture"));
    assert_eq!(
        result.usage,
        Some(json!({"input_tokens": 7, "output_tokens": 9}))
    );
    assert_eq!(
        tools::OpenAiResponsesProvider::ENDPOINT,
        "https://api.openai.com/v1/responses"
    );
    Ok(())
}

// Given: HTTPS fixture と guarded Responses transport / When: search / Then: bearer header と JSON が wire に乗り結果が組み上がる
#[tokio::test]
async fn openai_authorization_and_request_reach_the_wire() -> TestResult {
    let server = FixtureServer::start(|_| {
        response_with_status(
            "200 OK",
            &["Content-Type: application/json".to_owned()],
            openai_response().to_string().as_bytes(),
        )
    })
    .await?;
    let provider =
        tools::OpenAiResponsesProvider::new(Arc::new(responses_transport(&server)?), "wire-model");

    let result = provider
        .search("wire query", &SearchOptions::default())
        .await?;

    assert_eq!(result.result_count, 2);
    assert_eq!(result.request_id.as_deref(), Some("resp_fixture"));
    let requests = server.captured_requests();
    assert_eq!(requests.len(), 1);
    let request = String::from_utf8(requests[0].clone())?;
    let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP request");
    assert!(headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-token-not-a-real-secret\r\n")
    );
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert_eq!(
        serde_json::from_str::<Value>(body)?,
        json!({
            "model": "wire-model", "tools": [{"type": "web_search", "search_context_size": "medium"}],
            "input": "wire query", "store": false
        })
    );
    Ok(())
}

// Given: 非 2xx と不正 JSON / When: guarded search / Then: HTTP status/Protocol を写像し error body の秘密は破棄する
#[tokio::test]
async fn openai_wire_errors_preserve_fallback_contract_without_echoing_credentials() -> TestResult {
    for (status, code, trigger) in [
        ("401 Unauthorized", 401, false),
        ("403 Forbidden", 403, false),
        ("429 Too Many Requests", 429, true),
        ("500 Internal Server Error", 500, true),
        ("503 Service Unavailable", 503, true),
        ("200 OK", 200, false),
    ] {
        let server = FixtureServer::start(move |_| {
            response_with_status(status, &[], b"fixture-token-not-a-real-secret")
        })
        .await?;
        let provider = tools::OpenAiResponsesProvider::new(
            Arc::new(responses_transport(&server)?),
            "wire-model",
        );

        let error = provider
            .search("wire", &SearchOptions::default())
            .await
            .expect_err("invalid response");

        if code == 200 {
            assert!(matches!(error, SearchError::Protocol(_)));
        } else {
            assert!(matches!(error, SearchError::HttpStatus(actual) if actual == code));
        }
        assert_eq!(error.is_fallback_trigger(), trigger);
        assert!(
            !error
                .to_string()
                .contains("fixture-token-not-a-real-secret")
        );
    }
    Ok(())
}

// Given: POST redirect / When: guarded Responses search / Then: fail-closed Transport で header を転送せず fallback もしない
#[tokio::test]
async fn openai_post_redirect_is_fail_closed() -> TestResult {
    let server = FixtureServer::start(|_| {
        response_with_status(
            "302 Found",
            &["Location: https://example.com/other".to_owned()],
            b"",
        )
    })
    .await?;
    let provider =
        tools::OpenAiResponsesProvider::new(Arc::new(responses_transport(&server)?), "wire-model");

    let error = provider
        .search("wire", &SearchOptions::default())
        .await
        .expect_err("redirect rejected");

    assert!(matches!(error, SearchError::Transport(_)));
    assert!(!error.is_fallback_trigger());
    assert_eq!(server.captured_requests().len(), 1);
    Ok(())
}
