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
