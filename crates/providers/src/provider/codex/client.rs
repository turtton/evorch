//! Codex subscription backend の provider client を提供します。

mod compaction;
mod images;
mod web_search;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::EventBus;
use futures_util::StreamExt;
use uuid::Uuid;

use super::oauth::DeviceAuthClient;
use super::session::{AuthorizedCodexToken, CodexSessionManager};
use super::tokens::CodexTokenStore;
use crate::auth::ProviderAuth;
use crate::client::ProviderClient;
use crate::codex_catalog_version::{CodexClientVersion, CodexVersionResolution, catalog_failure};
use crate::error::ProviderError;
use crate::http::stream::adapt_sse_stream;
use crate::http::{
    UsageEmitter, build_http_client, build_http_client_with_read_timeout, map_request_error,
    map_response_error,
};
use crate::message::{ChatRequest, ChatResponse, ProviderCapabilities};
use crate::observe::AttemptObserver;
use crate::stream::{DeltaStream, StreamEvent};
use crate::wire::codex::{CodexResponsesRequest, CodexStreamInterpreter};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_AUTH_BASE_URL: &str = "https://auth.openai.com";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const COMPACTION_TIMEOUT: Duration = Duration::from_secs(600);
const COMPACTION_READ_TIMEOUT: Duration = Duration::from_secs(300);
const PROVIDER_LABEL: &str = "openai-codex";
const PROTOCOL: &str = "openai-codex-responses";
const ORIGINATOR: &str = "codex_cli_rs";

#[derive(Clone, Copy)]
enum ResponseMode {
    Streaming,
    NonStreaming,
    Compaction,
}

/// base URL から responses endpoint を一意に解決します。正の形は
/// `https://chatgpt.com/backend-api/codex` で、二重 append による 404 を防ぐため
/// `/responses` 込み・裸ホスト等の指定も正規化します（pi の resolveCodexUrl 方式）。
fn resolve_endpoint(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else if normalized.ends_with("/backend-api") {
        format!("{normalized}/codex/responses")
    } else {
        format!("{normalized}/backend-api/codex/responses")
    }
}

/// Codex subscription backend client の設定。
#[derive(Clone)]
pub struct CodexConfig {
    /// Codex backend のベース URL。
    pub base_url: String,
    /// OAuth refresh endpoint のベース URL。
    pub auth_base_url: String,
    /// 通常の非ストリーミング生成と web search のリクエスト全体のタイムアウト。
    pub timeout: Duration,
    /// usage と attempt 観測イベントの発行先。
    pub event_bus: Option<Arc<EventBus>>,
    /// 推論ヘッダーに付与するバージョン。クライアントごとに一度だけ解決する。
    pub client_version: CodexClientVersion,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            auth_base_url: DEFAULT_AUTH_BASE_URL.to_string(),
            timeout: DEFAULT_TIMEOUT,
            event_bus: None,
            client_version: CodexClientVersion::default(),
        }
    }
}

/// Codex subscription backend を canonical provider 契約へ接続します。
pub struct CodexClient {
    http_client: reqwest::Client,
    /// Compaction は通常生成より長い無応答期限と、本文受信までの全体期限を使う。
    compaction_http_client: reqwest::Client,
    endpoint: String,
    /// Catalog discovery appends `/models` to the configured base URL as given.
    catalog_base_url: String,
    timeout: Duration,
    event_bus: Option<Arc<EventBus>>,
    profile: Option<String>,
    client_version: CodexClientVersion,
    /// 解決結果を固定し、ターン間の prompt-cache affinity を維持する。
    resolved_client_version: tokio::sync::OnceCell<CodexVersionResolution>,
    session: CodexSessionManager,
    /// クライアントの生存期間を通じて維持するセッション識別子。
    session_id: String,
}

impl CodexClient {
    /// 設定とセッションマネージャから client を構築します。
    ///
    /// # Errors
    /// HTTP client を構築できない場合に返します。
    pub fn new(config: CodexConfig, session: CodexSessionManager) -> Result<Self, ProviderError> {
        crate::responses_replay::validate_endpoint(&config.base_url)?;
        Ok(Self {
            http_client: build_http_client(None)?,
            compaction_http_client: build_http_client_with_read_timeout(
                Some(COMPACTION_TIMEOUT),
                COMPACTION_READ_TIMEOUT,
            )?,
            endpoint: resolve_endpoint(&config.base_url),
            catalog_base_url: config.base_url.trim_end_matches('/').to_owned(),
            timeout: config.timeout,
            event_bus: config.event_bus,
            profile: None,
            client_version: config.client_version,
            resolved_client_version: tokio::sync::OnceCell::new(),
            session,
            session_id: Uuid::new_v4().to_string(),
        })
    }

    /// 設定と token store から client を構築します。
    ///
    /// # Errors
    /// HTTP client を構築できない場合に返します。
    pub fn with_config(
        config: CodexConfig,
        store: Arc<dyn CodexTokenStore>,
    ) -> Result<Self, ProviderError> {
        crate::responses_replay::validate_endpoint(&config.base_url)?;
        let http_client = build_http_client(None)?;
        let session = CodexSessionManager::new(
            store,
            DeviceAuthClient::new(&config.auth_base_url, http_client.clone()),
        );
        Ok(Self {
            http_client,
            compaction_http_client: build_http_client_with_read_timeout(
                Some(COMPACTION_TIMEOUT),
                COMPACTION_READ_TIMEOUT,
            )?,
            endpoint: resolve_endpoint(&config.base_url),
            catalog_base_url: config.base_url.trim_end_matches('/').to_owned(),
            timeout: config.timeout,
            event_bus: config.event_bus,
            profile: None,
            client_version: config.client_version,
            resolved_client_version: tokio::sync::OnceCell::new(),
            session,
            session_id: Uuid::new_v4().to_string(),
        })
    }

    /// Associates telemetry with the configured profile without changing the wire request.
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    fn observer(&self, request: &ChatRequest, streaming: bool) -> AttemptObserver {
        AttemptObserver::new(
            self.event_bus.clone(),
            PROVIDER_LABEL,
            self.profile.clone(),
            PROTOCOL,
            request.model.clone(),
            streaming,
            request.observation.clone(),
        )
        .with_service_tier(request.service_tier)
    }

    async fn execute(
        &self,
        request: &ChatRequest,
        streaming: bool,
    ) -> Result<DeltaStream, ProviderError> {
        let model_name = request.model.rsplit('/').next().unwrap_or_default();
        if ["kimi", "moonshot"].iter().any(|prefix| {
            model_name
                .get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        }) {
            return Err(ProviderError::Request(
                "Kimi full thinking requires an openai-compatible or kimi-subscription provider using openai-completions, not Codex reasoning summaries".into(),
            ));
        }
        let token = self.session.current().await?;
        let scope = self.responses_scope(request, &token);
        let wire_request = self.scoped_wire_request(request, &scope)?;
        let mut observer = self
            .observer(request, streaming)
            .with_reasoning_effort(wire_request.reasoning_effort())
            .with_cache_observation(&wire_request);
        let response = self
            .send_response(
                &wire_request,
                &token,
                if streaming {
                    ResponseMode::Streaming
                } else {
                    ResponseMode::NonStreaming
                },
                &mut observer,
            )
            .await?;
        // Pass the shared interpreter itself: a forwarding wrapper silently drops
        // defaulted hooks such as take_message/finalize_message.
        Ok(adapt_sse_stream(
            response.bytes_stream(),
            CodexStreamInterpreter::with_origin(scope),
            UsageEmitter::new(self.event_bus.clone(), PROVIDER_LABEL),
            request.model.clone(),
            observer,
        ))
    }

    fn responses_scope(
        &self,
        request: &ChatRequest,
        token: &AuthorizedCodexToken,
    ) -> crate::ResolvedResponsesScope {
        crate::ResolvedResponsesScope::resolve(
            request,
            crate::ResponsesDialect::Codex,
            self.profile.as_deref(),
            &self.endpoint,
            crate::ResponsesAccount::Codex {
                identity: token.chatgpt_account_id.clone(),
            },
        )
    }

    fn scoped_wire_request(
        &self,
        request: &ChatRequest,
        scope: &crate::ResolvedResponsesScope,
    ) -> Result<CodexResponsesRequest, ProviderError> {
        crate::wire::responses::to_wire_request_with_scope(
            request,
            &crate::wire::responses::ResponsesRequestOptions {
                dialect: crate::ResponsesDialect::Codex,
                prompt_cache_key: None,
            },
            scope,
        )
    }

    /// 通常生成と公式 compaction の HTTP エラー処理。
    async fn send_response(
        &self,
        wire_request: &CodexResponsesRequest,
        token: &AuthorizedCodexToken,
        mode: ResponseMode,
        observer: &mut AttemptObserver,
    ) -> Result<reqwest::Response, ProviderError> {
        let response = self
            .post_response_with_token(wire_request, token, mode, observer)
            .await?;
        if !response.status().is_success() {
            let error = map_response_error(response).await;
            observer.emit_failed(&error);
            return Err(error);
        }
        Ok(response)
    }

    /// All Responses paths share the selected account, refresh manager and headers.
    /// Status interpretation belongs to the caller (hosted search sanitizes errors).
    async fn post_response(
        &self,
        wire_request: &impl serde::Serialize,
        mode: ResponseMode,
        observer: &mut AttemptObserver,
    ) -> Result<reqwest::Response, ProviderError> {
        let token = self.session.current().await?;
        self.post_response_with_token(wire_request, &token, mode, observer)
            .await
    }

    async fn post_response_with_token(
        &self,
        wire_request: &impl serde::Serialize,
        token: &AuthorizedCodexToken,
        mode: ResponseMode,
        observer: &mut AttemptObserver,
    ) -> Result<reqwest::Response, ProviderError> {
        let http_client = match mode {
            ResponseMode::Compaction => &self.compaction_http_client,
            ResponseMode::Streaming | ResponseMode::NonStreaming => &self.http_client,
        };
        let mut builder = self
            .authenticated_request_with_token(http_client, &self.endpoint, token)
            .await
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&wire_request);
        if matches!(mode, ResponseMode::NonStreaming) {
            builder = builder.timeout(self.timeout);
        }
        let http_request = builder.build().map_err(map_request_error)?;
        observer.emit_started();
        let response = http_client
            .execute(http_request)
            .await
            .map_err(map_request_error)
            .inspect_err(|error| observer.emit_failed(error))?;
        Ok(response)
    }

    /// Share refreshed credentials and Codex session headers across endpoint families.
    async fn authenticated_request(
        &self,
        http: &reqwest::Client,
        endpoint: &str,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let token = self.session.current().await?;
        Ok(self
            .authenticated_request_with_token(http, endpoint, &token)
            .await)
    }

    /// Responses replay must use the exact lease already checked against its scope.
    async fn authenticated_request_with_token(
        &self,
        http: &reqwest::Client,
        endpoint: &str,
        token: &AuthorizedCodexToken,
    ) -> reqwest::RequestBuilder {
        let turn_id = Uuid::new_v4().to_string();
        let client_version = self
            .resolved_client_version
            .get_or_init(|| self.client_version.resolve_with_evidence())
            .await;
        http.post(endpoint)
            .bearer_auth(&token.access_token)
            .header("chatgpt-account-id", &token.chatgpt_account_id)
            .header("originator", ORIGINATOR)
            .header("OpenAI-Beta", "responses=experimental")
            .header("version", &client_version.value.version)
            .header(
                reqwest::header::USER_AGENT,
                format!("codex_cli_rs/{}", client_version.value.version),
            )
            .header("session-id", &self.session_id)
            .header("thread-id", &turn_id)
            .header("x-client-request-id", &turn_id)
    }
}

/// `ProviderAuth` は使用せず、セッションの token bundle から認証します。
#[async_trait]
impl ProviderClient for CodexClient {
    async fn resolve_responses_scope(
        &self,
        _: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<Option<crate::ResolvedResponsesScope>, ProviderError> {
        let token = self.session.current().await?;
        Ok(Some(self.responses_scope(request, &token)))
    }
    /// Discovery goes through the request session so an idle profile refreshes its token,
    /// and uses the catalog version its requests send.
    async fn list_models(&self, _: &ProviderAuth) -> Result<Option<Vec<String>>, ProviderError> {
        let started = std::time::Instant::now();
        let version_cache_hit = self.resolved_client_version.initialized();
        let client_version = self
            .resolved_client_version
            .get_or_init(|| self.client_version.resolve_with_evidence())
            .await;
        let catalog_started = std::time::Instant::now();
        let result = async {
            let token = self.session.current().await?;
            crate::list_codex_models(
                &self.catalog_base_url,
                &ProviderAuth::new(token.access_token),
                &token.chatgpt_account_id,
                &client_version.value.version,
            )
            .await
            .map(|models| {
                Some(
                    models
                        .into_iter()
                        .map(|model| model.slug)
                        .collect::<Vec<_>>(),
                )
            })
        }
        .await;
        if let Some(bus) = &self.event_bus {
            let failure = result.as_ref().err().map(catalog_failure);
            let count = result.as_ref().ok().and_then(Option::as_ref).map(Vec::len);
            bus.emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                source: "providers::codex::catalog".into(),
                severity: if client_version.value.warning.is_some() || result.is_err() {
                    event_bus::DiagnosticSeverity::Warning
                } else {
                    event_bus::DiagnosticSeverity::Info
                },
                code: event_bus::event::diagnostic_codes::PROVIDER_CATALOG_FETCH.into(),
                detail: format!(
                    "profile={} {} client_version_cache_hit={version_cache_hit} catalog_cache_hit=false catalog_result={} model_count={} catalog_elapsed_ms={} elapsed_ms={}{}",
                    self.profile.as_deref().unwrap_or("unknown"),
                    client_version.detail(),
                    failure.as_deref().unwrap_or("ok"),
                    count.map_or_else(|| "unknown".into(), |n| n.to_string()),
                    catalog_started.elapsed().as_millis(),
                    started.elapsed().as_millis(),
                    client_version.value.warning.as_ref().map_or_else(String::new, |warning| format!("; {warning}")),
                ),
                run_id: None,
                thread_id: None,
                call_id: None,
            }));
        }
        result.map_err(|error| match error {
            ProviderError::Http { status, .. } => ProviderError::Http {
                status,
                body: "catalog verification failed".into(),
            },
            ProviderError::Request(_) | ProviderError::Transport { .. } => {
                ProviderError::Request("catalog verification failed (transport/auth)".into())
            }
            ProviderError::InvalidJson { .. } => ProviderError::InvalidJson {
                detail: "catalog verification failed".into(),
            },
            other => other,
        })
    }

    fn catalog_degradation(&self) -> Option<String> {
        let resolution = self.resolved_client_version.get()?;
        resolution.value.warning.as_ref()?;
        Some(format!(
            "catalog degraded: {}; temporarily unable to verify catalog; retry with a new runtime/composition after version lookup recovers",
            resolution.detail(),
        ))
    }

    fn hosted_web_search(&self) -> Option<&dyn crate::HostedWebSearch> {
        Some(self)
    }

    fn image_generation(&self) -> Option<&dyn crate::ImageGeneration> {
        Some(self)
    }

    fn compactor(&self) -> Option<&dyn crate::Compactor> {
        Some(self)
    }

    fn supports_structured_output(&self) -> bool {
        true
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: true,
        }
    }

    async fn send(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let mut stream = self.execute(request, false).await?;
        while let Some(event) = stream.next().await {
            if let StreamEvent::Completed { response } = event? {
                return Ok(response);
            }
        }
        Err(ProviderError::Request(
            "codex response ended without completion".to_string(),
        ))
    }

    async fn stream(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        self.execute(request, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_endpoint;

    #[test]
    fn resolve_endpoint_normalizes_base_url_shapes() {
        let canonical = "https://chatgpt.com/backend-api/codex/responses";
        for (input, expected) in [
            ("https://chatgpt.com/backend-api/codex", canonical),
            ("https://chatgpt.com/backend-api/codex/", canonical),
            ("https://chatgpt.com/backend-api/codex/responses", canonical),
            ("https://chatgpt.com/backend-api", canonical),
            ("https://chatgpt.com", canonical),
        ] {
            assert_eq!(resolve_endpoint(input), expected, "input: {input}");
        }
    }
}
