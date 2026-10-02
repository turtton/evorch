//! OpenAI 公開 Responses API の web_search provider。
//!
//! OAuth token の取得・更新は行わない。呼び出し元が既存のアクセストークンを
//! `OPENAI_OAUTH_TOKEN` に設定する。内部 endpoint は使用しない。

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, InvalidHeaderValue};
use serde_json::{Value, json};

use super::{SearchError, SearchOptions, SearchProvider, SearchResults, count_search_results};
use crate::network_guard::{NetworkGuard, NetworkGuardError};

/// Responses API の単発リクエストを担う transport 境界。
#[async_trait]
pub trait ResponsesTransport: Send + Sync {
    /// JSON body を送信し、成功レスポンスを返す。
    ///
    /// # Errors
    /// 非 2xx・timeout・通信失敗・JSON 解析失敗を [`SearchError`] に写像する。
    async fn create_response(&self, body: &Value) -> Result<Value, SearchError>;
}

/// OpenAI 検索用の資格情報。token は Debug・ログ・metadata に公開しない。
pub struct OpenAiSearchCredential {
    token: String,
    /// 値ではなく取得元の環境変数名のみを公開する。
    pub source: &'static str,
}

impl OpenAiSearchCredential {
    /// API key を OAuth token より優先し、空文字は未設定として扱う。
    pub fn resolve(env_lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        ["OPENAI_API_KEY", "OPENAI_OAUTH_TOKEN"]
            .into_iter()
            .find_map(|source| {
                env_lookup(source)
                    .filter(|token| !token.is_empty())
                    .map(|token| Self { token, source })
            })
    }
}

/// [`NetworkGuard`] 経由で Authorization header 付きの POST を送る transport。
pub struct NetworkGuardResponsesTransport {
    guard: Arc<NetworkGuard>,
    endpoint: String,
    headers: HeaderMap,
}

impl NetworkGuardResponsesTransport {
    /// guard・endpoint・資格情報から構築する（endpoint 注入は fixture 検証用）。
    ///
    /// # Errors
    /// token を HTTP header にできない場合、値を含まない [`InvalidHeaderValue`] を返す。
    pub fn new(
        guard: Arc<NetworkGuard>,
        endpoint: impl Into<String>,
        credential: OpenAiSearchCredential,
    ) -> Result<Self, InvalidHeaderValue> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", credential.token))?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        Ok(Self {
            guard,
            endpoint: endpoint.into(),
            headers,
        })
    }
}

#[async_trait]
impl ResponsesTransport for NetworkGuardResponsesTransport {
    async fn create_response(&self, body: &Value) -> Result<Value, SearchError> {
        let response = self
            .guard
            .post_json(&self.endpoint, self.headers.clone(), body)
            .await
            .map_err(map_guard_error)?;
        if !response.status.is_success() {
            // HTTP error body は資格情報を反射する可能性があるため公開しない。
            return Err(SearchError::HttpStatus(response.status.as_u16()));
        }
        serde_json::from_slice(&response.body)
            .map_err(|_| SearchError::Protocol("Responses API returned invalid JSON".to_owned()))
    }
}

/// timeout のみ fallback 対象とし、その他は fail-closed にする。
fn map_guard_error(error: NetworkGuardError) -> SearchError {
    if let NetworkGuardError::Http(inner) = &error
        && inner.is_timeout()
    {
        return SearchError::Timeout;
    }
    // 下位 error の Debug/Display に request 情報を含めない。
    SearchError::Transport("Responses API guarded request failed".to_owned())
}

/// OpenAI 公開 Responses API の hosted web_search を使う provider。
pub struct OpenAiResponsesProvider {
    transport: Arc<dyn ResponsesTransport>,
    model: String,
    search_context_size: &'static str,
}

impl OpenAiResponsesProvider {
    /// 公開 API のみを使う production endpoint。
    pub const ENDPOINT: &'static str = "https://api.openai.com/v1/responses";
    /// 非 reasoning の web search 対応モデル。`OPENAI_WEB_SEARCH_MODEL` で上書き可能。
    ///
    /// 2026-10-02 確認: GPT-4.1 は Responses web search 対応（検索 context は 128k）。
    /// 入力 $2 / cached input $0.50 / 出力 $8 per 1M tokens、web_search は
    /// $10 / 1k calls + 検索 content tokens のモデル料金（料金は将来変更されうる）。
    /// - <https://developers.openai.com/api/docs/guides/tools-web-search>
    /// - <https://developers.openai.com/api/docs/models/gpt-4.1>
    /// - <https://developers.openai.com/api/docs/pricing>
    pub const DEFAULT_MODEL: &'static str = "gpt-4.1";
    /// web search に使う既定 context size。
    pub const DEFAULT_CONTEXT_SIZE: &'static str = "medium";

    /// 任意の transport とモデルで構築する。
    pub fn new(transport: Arc<dyn ResponsesTransport>, model: impl Into<String>) -> Self {
        Self {
            transport,
            model: model.into(),
            search_context_size: Self::DEFAULT_CONTEXT_SIZE,
        }
    }

    /// production guard と資格情報から構築する。
    ///
    /// # Errors
    /// token を HTTP header にできない場合 [`InvalidHeaderValue`] を返す。
    pub fn with_guard(
        guard: Arc<NetworkGuard>,
        credential: OpenAiSearchCredential,
    ) -> Result<Self, InvalidHeaderValue> {
        let transport = NetworkGuardResponsesTransport::new(guard, Self::ENDPOINT, credential)?;
        let model = std::env::var("OPENAI_WEB_SEARCH_MODEL")
            .ok()
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| Self::DEFAULT_MODEL.to_owned());
        Ok(Self::new(Arc::new(transport), model))
    }

    /// `store: false` は web_search と併用する。
    ///
    /// 2026-10-02 確認: Responses の store は response 保存制御であり、web_search
    /// の必須条件ではない。公式のデータ制御には ZDR（store が false になる）での
    /// web_search 利用も記載されている。これは abuse monitoring 全体の無保存保証ではない。
    /// - <https://developers.openai.com/api/docs/guides/your-data>
    /// - <https://developers.openai.com/api/docs/guides/tools-web-search>
    fn request_body(&self, query: &str) -> Value {
        json!({
            "model": self.model,
            "tools": [{"type": "web_search", "search_context_size": self.search_context_size}],
            "input": query,
            "store": false,
        })
    }
}

#[async_trait]
impl SearchProvider for OpenAiResponsesProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn uses_credentials(&self) -> bool {
        true
    }

    async fn search(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<SearchResults, SearchError> {
        let response = self
            .transport
            .create_response(&self.request_body(query))
            .await?;
        parse_response(&response, options)
    }
}

fn parse_response(response: &Value, options: &SearchOptions) -> Result<SearchResults, SearchError> {
    match response["status"].as_str() {
        Some("completed" | "incomplete") => {}
        Some("failed" | "cancelled") => {
            return Err(SearchError::ProviderRejected(
                response["error"]["message"]
                    .as_str()
                    .unwrap_or("Responses API response failed or was cancelled")
                    .to_owned(),
            ));
        }
        _ => {
            return Err(SearchError::Protocol(
                "Unexpected Responses API status".to_owned(),
            ));
        }
    }
    let content = response["output"]
        .as_array()
        .and_then(|output| output.iter().find(|item| item["type"] == "message"))
        .and_then(|message| message["content"].as_array())
        .ok_or_else(|| SearchError::Protocol("Responses API message is missing".to_owned()))?;
    let mut texts = Vec::new();
    let mut citations = Vec::new();
    let mut seen = HashSet::new();
    for item in content.iter().filter(|item| item["type"] == "output_text") {
        if let Some(text) = item["text"].as_str() {
            texts.push(text);
        }
        for annotation in item["annotations"].as_array().into_iter().flatten() {
            if annotation["type"] != "url_citation" {
                continue;
            }
            if let Some(url) = annotation["url"].as_str()
                && seen.insert(url)
            {
                let title = annotation["title"].as_str().unwrap_or(url);
                citations.push(format!("Title: {title}\nURL: {url}"));
            }
        }
    }
    if texts.is_empty() {
        return Err(SearchError::Protocol(
            "Responses API output_text is missing".to_owned(),
        ));
    }
    if let Some(max_results) = options.max_results {
        citations.truncate(max_results as usize);
    }
    let mut content = texts.join("\n");
    if !citations.is_empty() {
        content.push_str("\n\n");
        content.push_str(&citations.join("\n\n---\n\n"));
    }
    Ok(SearchResults {
        result_count: count_search_results(&content),
        content,
        request_id: response["id"].as_str().map(str::to_owned),
        usage: response.get("usage").cloned(),
    })
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Mutex;

    use super::*;

    struct StubResponsesTransport {
        response: Value,
        bodies: Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl ResponsesTransport for StubResponsesTransport {
        async fn create_response(&self, body: &Value) -> Result<Value, SearchError> {
            self.bodies.lock().expect("bodies mutex").push(body.clone());
            Ok(self.response.clone())
        }
    }

    fn response() -> Value {
        json!({
            "id": "resp_search", "status": "completed", "usage": {"input_tokens": 12},
            "output": [
                {"type": "web_search_call"},
                {"type": "message", "content": [
                    {"type": "output_text", "text": "Answer.", "annotations": [
                        {"type": "url_citation", "title": "First", "url": "https://example.com/1"},
                        {"type": "url_citation", "title": "Duplicate", "url": "https://example.com/1"}
                    ]},
                    {"type": "output_text", "text": "More.", "annotations": [
                        {"type": "url_citation", "title": "Second", "url": "https://example.com/2"},
                        {"type": "file_citation", "url": "https://example.com/ignored"}
                    ]}
                ]},
                {"type": "message", "content": [{"type": "output_text", "text": "Ignored."}]}
            ]
        })
    }

    // Given: citation 付き応答 / When: search / Then: 公開 API body と正規化結果を得る
    #[tokio::test]
    async fn shapes_body_and_parses_first_message_with_ordered_unique_citations() {
        let stub = Arc::new(StubResponsesTransport {
            response: response(),
            bodies: Mutex::new(Vec::new()),
        });
        let provider = OpenAiResponsesProvider::new(stub.clone(), "test-model");

        let result = provider
            .search("query", &SearchOptions::default())
            .await
            .expect("search");

        assert_eq!(provider.name(), "openai");
        assert!(provider.uses_credentials());
        assert_eq!(
            *stub.bodies.lock().expect("bodies mutex"),
            vec![json!({
                "model": "test-model", "tools": [{"type": "web_search", "search_context_size": "medium"}],
                "input": "query", "store": false,
            })]
        );
        assert_eq!(
            result.content,
            "Answer.\nMore.\n\nTitle: First\nURL: https://example.com/1\n\n---\n\nTitle: Second\nURL: https://example.com/2"
        );
        assert_eq!(result.result_count, 2);
        assert_eq!(result.request_id.as_deref(), Some("resp_search"));
        assert_eq!(result.usage, Some(json!({"input_tokens": 12})));
    }

    // Given: citation の有無と bound / When: parse / Then: 本文を維持し引用のみ打ち切る
    #[test]
    fn bounds_citations_after_deduplication_and_accepts_uncited_answers() {
        for (max_results, count) in [(Some(0), 0), (Some(1), 1), (Some(10), 2), (None, 2)] {
            let result =
                parse_response(&response(), &SearchOptions { max_results }).expect("parse");
            assert_eq!(result.result_count, count);
            assert!(result.content.starts_with("Answer.\nMore."));
        }
        let uncited = json!({"status": "completed", "output": [{"type": "message", "content": [
            {"type": "output_text", "text": "No citations."}
        ]}]});
        let result = parse_response(&uncited, &SearchOptions::default()).expect("uncited");
        assert_eq!(result.content, "No citations.");
        assert_eq!(result.result_count, 0);
        assert!(result.request_id.is_none());
        assert!(result.usage.is_none());
    }

    // Given: 終端/非終端 status / When: parse / Then: incomplete は本文必須、拒否は非 fallback
    #[test]
    fn classifies_status_and_missing_message() {
        let mut value = response();
        value["status"] = json!("incomplete");
        assert!(parse_response(&value, &SearchOptions::default()).is_ok());
        for status in [
            "completed",
            "incomplete",
            "queued",
            "in_progress",
            "unknown",
        ] {
            let error = parse_response(
                &json!({"status": status, "output": []}),
                &SearchOptions::default(),
            )
            .expect_err("missing message");
            assert!(matches!(error, SearchError::Protocol(_)));
            assert!(!error.is_fallback_trigger());
        }
        for status in ["failed", "cancelled"] {
            value["status"] = json!(status);
            value["error"] = json!({"message": "Search rejected"});
            let error = parse_response(&value, &SearchOptions::default()).expect_err("rejected");
            assert!(
                matches!(&error, SearchError::ProviderRejected(message) if message == "Search rejected")
            );
            assert!(!error.is_fallback_trigger());
        }
        assert!(parse_response(&json!({}), &SearchOptions::default()).is_err());
    }

    // Given: 両資格情報・空文字・未設定 / When: resolve / Then: API key 優先で空値を無視する
    #[test]
    fn resolves_credentials_in_priority_order() {
        for (api_key, oauth, expected) in [
            (Some("api"), Some("oauth"), Some(("OPENAI_API_KEY", "api"))),
            (
                Some(""),
                Some("oauth"),
                Some(("OPENAI_OAUTH_TOKEN", "oauth")),
            ),
            (None, Some("oauth"), Some(("OPENAI_OAUTH_TOKEN", "oauth"))),
            (Some(""), Some(""), None),
            (None, None, None),
        ] {
            let credential = OpenAiSearchCredential::resolve(|key| match key {
                "OPENAI_API_KEY" => api_key.map(str::to_owned),
                "OPENAI_OAUTH_TOKEN" => oauth.map(str::to_owned),
                _ => None,
            });
            assert_eq!(
                credential.as_ref().map(|c| (c.source, c.token.as_str())),
                expected
            );
        }
    }

    // Given: 不正 header / When: transport 構築 / Then: token を含まない error で拒否する
    #[test]
    fn rejects_invalid_credentials_without_disclosing_the_token() {
        let credential = OpenAiSearchCredential::resolve(|_| Some("secret\ninvalid".to_owned()))
            .expect("credential");
        let error = NetworkGuardResponsesTransport::new(
            Arc::new(NetworkGuard::new().expect("guard")),
            OpenAiResponsesProvider::ENDPOINT,
            credential,
        )
        .err()
        .expect("invalid header");
        assert!(!error.to_string().contains("secret"));
    }

    // Given: guard が private IP を拒否 / When: 写像 / Then: fallback しない Transport
    #[test]
    fn maps_non_timeout_guard_errors_fail_closed() {
        let error = map_guard_error(NetworkGuardError::BlockedIp {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        });
        assert!(matches!(error, SearchError::Transport(_)));
        assert!(!error.is_fallback_trigger());
    }
}
