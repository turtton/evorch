//! web_search ツールが利用する keyless 検索プロバイダ層。
//!
//! MCP JSON-RPC による単発 tools/call transport、envelope 解析、そして
//! transport の上に成る keyless 検索 provider（[`SearchProvider`]）を提供する。

pub(crate) mod envelope;
pub mod error;
pub mod mcp;

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::network_guard::NetworkGuard;

pub use error::SearchError;
pub use mcp::{McpToolSuccess, McpTransport, NetworkGuardMcpTransport};

/// 検索 1 回あたりの要求 option。
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchOptions {
    /// 取得する最大 result 数（None は provider 既定）。
    pub max_results: Option<u32>,
}

/// keyless provider が組み立てた検索結果。
#[derive(Debug, Clone)]
pub struct SearchResults {
    /// provider 応答の本文（formatter 由来の text）。
    pub content: String,
    /// content から best-effort で数えた result 数。
    pub result_count: usize,
    /// keyless envelope には request id が無いため常に None（Q10 schema 用の field）。
    pub request_id: Option<String>,
    /// provider 固有の usage metadata（Exa の `_meta.searchTime` など）。
    pub usage: Option<serde_json::Value>,
}

/// keyless 検索 provider の抽象。
///
/// ## 第三の provider を非破壊で追加する方法
///
/// 1. [`McpTransport`] を実装する型（production は [`NetworkGuardMcpTransport`]、
///    別プロトコルなら独自実装）を用意する。
/// 2. 新しい provider 構造体で [`SearchProvider`] を実装する。endpoint・tool 名・
///    引数 shaping・必要な header はその provider が独自に持つ。既存 provider
///    （Exa / Tavily）への変更も、ここでの登録処理も不要。
/// 3. その provider インスタンスを web_search ツールに渡す。ツール側は
///    `dyn SearchProvider` としてのみ扱うため、既存経路に影響しない。
#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// provider の識別名。
    fn name(&self) -> &str;

    /// query を検索し、正規化した結果を返す。
    ///
    /// # Errors
    /// transport・envelope・provider 拒否のいずれかで失敗した場合、
    /// [`SearchError`] を返す。fallback 判定は [`SearchError::is_fallback_trigger`] で行う。
    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<SearchResults, SearchError>;
}

/// provider 共通の単発 tools/call 実行と SearchResults 組み立て。
pub(crate) struct KeylessMcpSearch {
    transport: Arc<dyn McpTransport>,
    tool_name: &'static str,
    max_results_key: &'static str,
}

impl KeylessMcpSearch {
    pub(crate) fn new(
        transport: Arc<dyn McpTransport>,
        tool_name: &'static str,
        max_results_key: &'static str,
    ) -> Self {
        Self {
            transport,
            tool_name,
            max_results_key,
        }
    }

    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<SearchResults, SearchError> {
        let mut arguments = serde_json::json!({ "query": query });
        if let Some(max_results) = options.max_results {
            arguments[self.max_results_key] = serde_json::json!(max_results);
        }
        let success = self.transport.call_tool(self.tool_name, arguments).await?;
        Ok(SearchResults {
            result_count: count_search_results(&success.text),
            content: success.text,
            request_id: None,
            usage: success.usage,
        })
    }
}

/// content 中の `Title: ` 行を数え、best-effort で result 数を近似する。
///
/// 形式の根拠: Exa formatter は result ごとに `Title:`/`URL:`/`Published:`/`Author:`/
/// `Highlights:` block を `\n\n---\n\n` で連結し、Tavily formatter は省略可能な
/// `Answer: ...` と `Detailed Results:` の後に `\nTitle: ...\nURL: ...\nContent: ...`
/// を並べる。いずれも result の 1 行目が `Title: ` で始まるため、この heuristic で
/// 数えられる（本文中の引用に `Title: ` 行が現れた場合は過大に数えうる）。
pub fn count_search_results(content: &str) -> usize {
    content
        .lines()
        .filter(|line| line.starts_with("Title: "))
        .count()
}

/// Exa の keyless MCP endpoint 向け provider。
pub struct ExaKeylessProvider {
    core: KeylessMcpSearch,
}

impl ExaKeylessProvider {
    /// Exa keyless MCP endpoint。
    pub const ENDPOINT: &'static str = "https://mcp.exa.ai/mcp";
    const TOOL_NAME: &'static str = "web_search_exa";

    /// 任意の transport（production は [`NetworkGuardMcpTransport`]、test は stub）で構築する。
    pub fn new(transport: Arc<dyn McpTransport>) -> Self {
        Self {
            core: KeylessMcpSearch::new(transport, Self::TOOL_NAME, "numResults"),
        }
    }

    /// production 用の構築: guard から既定 endpoint の guarded transport を組み立てる。
    pub fn with_guard(guard: Arc<NetworkGuard>) -> Self {
        Self::new(Arc::new(NetworkGuardMcpTransport::new(
            guard,
            Self::ENDPOINT,
            HeaderMap::new(),
        )))
    }
}

#[async_trait]
impl SearchProvider for ExaKeylessProvider {
    fn name(&self) -> &str {
        "exa"
    }

    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<SearchResults, SearchError> {
        self.core.search(query, options).await
    }
}

/// Tavily の keyless MCP endpoint 向け provider。
pub struct TavilyKeylessProvider {
    core: KeylessMcpSearch,
}

impl TavilyKeylessProvider {
    /// Tavily keyless MCP endpoint。公式形式に倣い trailing slash を付ける
    /// （POST 応答の redirect 拒否を回避する）。
    pub const ENDPOINT: &'static str = "https://mcp.tavily.com/mcp/";
    /// 現行 tavily-mcp ソースが登録する tool 名（`tavily-search` と書く docs は古い）。
    const TOOL_NAME: &'static str = "tavily_search";

    /// keyless アクセスモードを要求する provider 固有 header。
    pub fn extra_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("X-Tavily-Access-Mode", HeaderValue::from_static("keyless"));
        headers
    }

    /// 任意の transport（production は [`NetworkGuardMcpTransport`]、test は stub）で構築する。
    pub fn new(transport: Arc<dyn McpTransport>) -> Self {
        Self {
            core: KeylessMcpSearch::new(transport, Self::TOOL_NAME, "max_results"),
        }
    }

    /// production 用の構築: guard から既定 endpoint と [`Self::extra_headers`] の
    /// guarded transport を組み立てる。
    pub fn with_guard(guard: Arc<NetworkGuard>) -> Self {
        Self::new(Arc::new(NetworkGuardMcpTransport::new(
            guard,
            Self::ENDPOINT,
            Self::extra_headers(),
        )))
    }
}

#[async_trait]
impl SearchProvider for TavilyKeylessProvider {
    fn name(&self) -> &str {
        "tavily"
    }

    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<SearchResults, SearchError> {
        self.core.search(query, options).await
    }
}
