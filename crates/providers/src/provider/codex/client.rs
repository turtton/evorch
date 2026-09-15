//! Codex subscription backend の provider client を提供します。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::EventBus;
use futures_util::StreamExt;
use uuid::Uuid;

use super::oauth::DeviceAuthClient;
use super::session::CodexSessionManager;
use super::tokens::CodexTokenStore;
use crate::auth::ProviderAuth;
use crate::client::ProviderClient;
use crate::error::ProviderError;
use crate::http::stream::{FrameInterpretation, WireStreamInterpreter, adapt_sse_stream};
use crate::http::{UsageEmitter, build_http_client, map_request_error, map_response_error};
use crate::message::{ChatRequest, ChatResponse, ProviderCapabilities};
use crate::observe::AttemptObserver;
use crate::sse::SseFrame;
use crate::stream::{DeltaStream, StreamEvent};
use crate::wire::codex::{CodexStreamInterpreter, to_wire_request};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_AUTH_BASE_URL: &str = "https://auth.openai.com";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const PROVIDER_LABEL: &str = "openai-codex";
const PROTOCOL: &str = "openai-codex-responses";
const ORIGINATOR: &str = "codex_cli_rs";

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
    /// 非ストリーミングリクエスト全体のタイムアウト。
    pub timeout: Duration,
    /// usage と attempt 観測イベントの発行先。
    pub event_bus: Option<Arc<EventBus>>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            auth_base_url: DEFAULT_AUTH_BASE_URL.to_string(),
            timeout: DEFAULT_TIMEOUT,
            event_bus: None,
        }
    }
}

/// Codex subscription backend を canonical provider 契約へ接続します。
pub struct CodexClient {
    http_client: reqwest::Client,
    endpoint: String,
    timeout: Duration,
    event_bus: Option<Arc<EventBus>>,
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
        Ok(Self {
            http_client: build_http_client(None)?,
            endpoint: resolve_endpoint(&config.base_url),
            timeout: config.timeout,
            event_bus: config.event_bus,
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
        let http_client = build_http_client(None)?;
        let session = CodexSessionManager::new(
            store,
            DeviceAuthClient::new(&config.auth_base_url, http_client.clone()),
        );
        Ok(Self {
            http_client,
            endpoint: resolve_endpoint(&config.base_url),
            timeout: config.timeout,
            event_bus: config.event_bus,
            session,
            session_id: Uuid::new_v4().to_string(),
        })
    }

    fn observer(&self, request: &ChatRequest, streaming: bool) -> AttemptObserver {
        AttemptObserver::new(
            self.event_bus.clone(),
            PROVIDER_LABEL,
            None,
            PROTOCOL,
            request.model.clone(),
            streaming,
            request.observation.clone(),
        )
    }

    async fn execute(
        &self,
        request: &ChatRequest,
        streaming: bool,
    ) -> Result<DeltaStream, ProviderError> {
        let token = self.session.current().await?;
        let wire_request = to_wire_request(request);
        let turn_id = Uuid::new_v4().to_string();
        let mut observer = self.observer(request, streaming);
        let mut builder = self
            .http_client
            .post(&self.endpoint)
            .bearer_auth(&token.access_token)
            .header("chatgpt-account-id", &token.chatgpt_account_id)
            .header("originator", ORIGINATOR)
            .header("OpenAI-Beta", "responses=experimental")
            .header("version", crate::CODEX_MODELS_CLIENT_VERSION)
            .header(
                reqwest::header::USER_AGENT,
                format!("codex_cli_rs/{}", crate::CODEX_MODELS_CLIENT_VERSION),
            )
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header("session-id", &self.session_id)
            .header("thread-id", &turn_id)
            .header("x-client-request-id", &turn_id)
            .json(&wire_request);
        if !streaming {
            builder = builder.timeout(self.timeout);
        }
        let http_request = builder.build().map_err(map_request_error)?;
        observer.emit_started();
        let response = self
            .http_client
            .execute(http_request)
            .await
            .map_err(map_request_error)
            .inspect_err(|error| observer.emit_failed(error))?;
        if !response.status().is_success() {
            let error = map_response_error(response).await;
            observer.emit_failed(&error);
            return Err(error);
        }
        Ok(adapt_sse_stream(
            response.bytes_stream(),
            CodexInterpreterAdapter(CodexStreamInterpreter::new()),
            UsageEmitter::new(self.event_bus.clone(), PROVIDER_LABEL),
            request.model.clone(),
            observer,
        ))
    }
}

/// `ProviderAuth` は使用せず、セッションの token bundle から認証します。
#[async_trait]
impl ProviderClient for CodexClient {
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

struct CodexInterpreterAdapter(CodexStreamInterpreter);

impl WireStreamInterpreter for CodexInterpreterAdapter {
    fn interpret(&mut self, frame: SseFrame) -> Result<FrameInterpretation, ProviderError> {
        self.0.interpret(frame)
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
        self.0.finish()
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
