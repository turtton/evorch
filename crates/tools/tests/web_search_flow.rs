//! web_search ツールのフロー結合テスト（fallback 規律・メタデータ・引数パース）。
//!
//! stub provider を注入し、ToolExecutor を介さない Tool としての振る舞いを
//! 検証する。スキーマ検証は既存ツールと同様に `jsonschema::validator_for` で
//! 行う（tools::tools::tests の house pattern）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use serde_json::json;
use tools::{SearchError, SearchOptions, SearchProvider, SearchResults, Tool, WebSearch};

/// 呼び出し回数を数える stub provider。
struct StubProvider {
    name: &'static str,
    result: Result<SearchResults, SearchError>,
    calls: AtomicU32,
}

impl StubProvider {
    fn new(name: &'static str, result: Result<SearchResults, SearchError>) -> Self {
        Self {
            name,
            result,
            calls: AtomicU32::new(0),
        }
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SearchProvider for StubProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn search(
        &self,
        _query: &str,
        _options: &SearchOptions,
    ) -> Result<SearchResults, SearchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

fn exa_ok() -> Result<SearchResults, SearchError> {
    Ok(SearchResults {
        content: "Title: Exa result\nURL: https://example.com/exa".to_owned(),
        result_count: 1,
        request_id: Some("req-exa-1".to_owned()),
        usage: Some(json!({ "searchTime": 21 })),
    })
}

fn tavily_ok() -> Result<SearchResults, SearchError> {
    Ok(SearchResults {
        content: "Title: Tavily result\nURL: https://example.com/tavily".to_owned(),
        result_count: 1,
        request_id: Some("req-tavily-1".to_owned()),
        usage: Some(json!({ "tavilyUsage": true })),
    })
}

/// primary / fallback の stub から web_search を組む（credential 判定は常に keyless）。
fn web_search(
    primary: Result<SearchResults, SearchError>,
    fallback: Result<SearchResults, SearchError>,
) -> (WebSearch, Arc<StubProvider>, Arc<StubProvider>) {
    let primary_stub = Arc::new(StubProvider::new("exa", primary));
    let fallback_stub = Arc::new(StubProvider::new("tavily", fallback));
    let primary: Arc<dyn SearchProvider> = primary_stub.clone();
    let fallback: Arc<dyn SearchProvider> = fallback_stub.clone();
    let tool = WebSearch::for_providers_with_env_lookup(
        primary,
        fallback,
        Arc::new(|_key: &str| -> Option<String> { None }),
    );
    (tool, primary_stub, fallback_stub)
}

async fn execute_default_query(tool: &WebSearch) -> Result<tools::ToolResult, tools::ToolError> {
    Tool::execute(tool, json!({ "query": "evorch" })).await
}

// Given: web_search の静的スキーマ / When: jsonschema::validator_for でコンパイル / Then: 成功する
#[test]
fn schema_compiles() {
    let tool = web_search(exa_ok(), tavily_ok()).0;

    jsonschema::validator_for(&Tool::schema(&tool))
        .unwrap_or_else(|error| panic!("web_search のスキーマのコンパイルに失敗: {error}"));
}

// Given: primary が成功する stub 群 / When: web_search を実行 / Then: fallback は呼ばれず、metadata は primary 成功の内容になる
#[tokio::test]
async fn happy_path_uses_primary_without_fallback() {
    let (tool, primary, fallback) = web_search(exa_ok(), tavily_ok());

    let result = execute_default_query(&tool).await.expect("検索は成功する");

    assert!(!result.is_error);
    assert!(result.content.contains("Title: Exa result"));
    let detail = result.detail.expect("成功時は metadata が付く");
    assert_eq!(detail["provider"], "exa");
    assert_eq!(detail["used_fallback"], false);
    assert_eq!(detail["fallback_attempts"], 0);
    assert_eq!(detail["result_count"], 1);
    assert_eq!(detail["request_id"], "req-exa-1");
    assert_eq!(detail["credential_status"], "keyless");
    assert!(detail["latency_ms"].is_u64(), "latency_ms は数値: {detail}");
    assert_eq!(detail["usage"], json!({ "searchTime": 21 }));
    assert_eq!(primary.calls(), 1);
    assert_eq!(fallback.calls(), 0);
}

// Given: primary が 429・fallback が成功する stub 群 / When: web_search を実行 / Then: 1 回の fallback で成功し、fallback の usage が透過される
#[tokio::test]
async fn rate_limited_primary_falls_back_once_and_passes_usage_through() {
    let (tool, primary, fallback) = web_search(Err(SearchError::HttpStatus(429)), tavily_ok());

    let result = execute_default_query(&tool).await.expect("検索は成功する");

    assert!(!result.is_error);
    assert!(result.content.contains("Title: Tavily result"));
    let detail = result.detail.expect("成功時は metadata が付く");
    assert_eq!(detail["provider"], "tavily");
    assert_eq!(detail["used_fallback"], true);
    assert_eq!(detail["fallback_attempts"], 1);
    assert_eq!(detail["request_id"], "req-tavily-1");
    assert_eq!(detail["usage"], json!({ "tavilyUsage": true }));
    assert_eq!(primary.calls(), 1);
    assert_eq!(fallback.calls(), 1);
}

// Given: primary が fallback 非対象の 400 で失敗する stub 群 / When: web_search を実行 / Then: fallback は一切呼ばれず、is_error の結果に metadata が付く
#[tokio::test]
async fn non_trigger_error_skips_fallback() {
    let (tool, _primary, fallback) = web_search(Err(SearchError::HttpStatus(400)), tavily_ok());

    let result = execute_default_query(&tool)
        .await
        .expect("結果は値として返る");

    assert!(result.is_error);
    assert!(
        result.content.contains("exa"),
        "primary 名を含む: {}",
        result.content
    );
    let detail = result.detail.expect("失敗時も metadata が付く");
    assert_eq!(detail["provider"], "exa");
    assert_eq!(detail["used_fallback"], false);
    assert_eq!(detail["fallback_attempts"], 0);
    assert_eq!(detail["result_count"], 0);
    assert_eq!(detail["request_id"], serde_json::Value::Null);
    assert_eq!(detail["usage"], serde_json::Value::Null);
    assert_eq!(
        fallback.calls(),
        0,
        "fallback 非対象の error では fallback を呼ばない"
    );
}

// Given: primary が timeout・fallback が成功する stub 群 / When: web_search を実行 / Then: 1 回の fallback で成功する
#[tokio::test]
async fn timeout_primary_falls_back() {
    let (tool, _primary, fallback) = web_search(Err(SearchError::Timeout), tavily_ok());

    let result = execute_default_query(&tool).await.expect("検索は成功する");

    assert!(!result.is_error);
    let detail = result.detail.expect("成功時は metadata が付く");
    assert_eq!(detail["provider"], "tavily");
    assert_eq!(detail["used_fallback"], true);
    assert_eq!(detail["fallback_attempts"], 1);
    assert_eq!(fallback.calls(), 1);
}

// Given: primary が 429・fallback が 500 で失敗する stub 群 / When: web_search を実行 / Then: 本文が両 provider 名と error 概要を名指しし、metadata は fallback 失敗の内容になる
#[tokio::test]
async fn both_fail_reports_both_providers_in_content_and_detail() {
    let (tool, _primary, fallback) = web_search(
        Err(SearchError::HttpStatus(429)),
        Err(SearchError::HttpStatus(500)),
    );

    let result = execute_default_query(&tool)
        .await
        .expect("結果は値として返る");

    assert!(result.is_error);
    assert!(
        result.content.contains("exa"),
        "primary 名を含む: {}",
        result.content
    );
    assert!(
        result.content.contains("tavily"),
        "fallback 名を含む: {}",
        result.content
    );
    assert!(
        result.content.contains("429"),
        "primary の error 概要を含む: {}",
        result.content
    );
    assert!(
        result.content.contains("500"),
        "fallback の error 概要を含む: {}",
        result.content
    );
    let detail = result.detail.expect("失敗時も metadata が付く");
    assert_eq!(detail["provider"], "tavily");
    assert_eq!(detail["used_fallback"], true);
    assert_eq!(detail["fallback_attempts"], 1);
    assert_eq!(detail["result_count"], 0);
    assert_eq!(detail["request_id"], serde_json::Value::Null);
    assert_eq!(detail["usage"], serde_json::Value::Null);
    assert_eq!(fallback.calls(), 1);
}

// Given: primary・fallback とも 429 を返す stub 群 / When: web_search を実行 / Then: fallback 呼び出しは 1 回に制限される
#[tokio::test]
async fn fallback_attempt_is_capped_at_one() {
    let (tool, _primary, fallback) = web_search(
        Err(SearchError::HttpStatus(429)),
        Err(SearchError::HttpStatus(429)),
    );

    let result = execute_default_query(&tool)
        .await
        .expect("結果は値として返る");

    assert!(result.is_error);
    let detail = result.detail.expect("失敗時も metadata が付く");
    assert_eq!(detail["fallback_attempts"], 1);
    assert_eq!(fallback.calls(), 1, "fallback が 429 でも 2 回目は呼ばない");
}

// Given: EXA_API_KEY を返す env lookup / When: web_search を実行 / Then: credential_status は key_present_unused になる
#[tokio::test]
async fn credential_status_reflects_key_presence() {
    let primary: Arc<dyn SearchProvider> = Arc::new(StubProvider::new("exa", exa_ok()));
    let fallback: Arc<dyn SearchProvider> = Arc::new(StubProvider::new("tavily", tavily_ok()));
    let tool = WebSearch::for_providers_with_env_lookup(
        primary,
        fallback,
        Arc::new(|key: &str| -> Option<String> { (key == "EXA_API_KEY").then(|| "k".to_owned()) }),
    );

    let result = execute_default_query(&tool).await.expect("検索は成功する");

    let detail = result.detail.expect("成功時は metadata が付く");
    assert_eq!(detail["credential_status"], "key_present_unused");
}

// Given: 空文字列のキーだけを返す env lookup / When: web_search を実行 / Then: 空のキーは存在しないものと扱われ credential_status は keyless になる
#[tokio::test]
async fn empty_key_value_is_treated_as_keyless() {
    let primary: Arc<dyn SearchProvider> = Arc::new(StubProvider::new("exa", exa_ok()));
    let fallback: Arc<dyn SearchProvider> = Arc::new(StubProvider::new("tavily", tavily_ok()));
    let tool = WebSearch::for_providers_with_env_lookup(
        primary,
        fallback,
        Arc::new(|_key: &str| -> Option<String> { Some(String::new()) }),
    );

    let result = execute_default_query(&tool).await.expect("検索は成功する");

    let detail = result.detail.expect("成功時は metadata が付く");
    assert_eq!(detail["credential_status"], "keyless");
}

// Given: query を欠く引数 / When: web_search を直接実行 / Then: InvalidArgs で拒否される
#[tokio::test]
async fn missing_query_is_invalid_args() {
    let (tool, primary, _fallback) = web_search(exa_ok(), tavily_ok());

    let error = Tool::execute(&tool, json!({ "max_results": 3 }))
        .await
        .expect_err("query 欠落は InvalidArgs になる");

    assert!(
        matches!(error, tools::ToolError::InvalidArgs { .. }),
        "実際: {error:?}"
    );
    assert_eq!(primary.calls(), 0, "引数パース失敗時は provider を呼ばない");
}
