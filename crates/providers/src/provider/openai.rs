//! OpenAI provider 実装を提供します。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::EventBus;

use crate::auth::ProviderAuth;
use crate::client::ProviderClient;
use crate::error::ProviderError;
use crate::http::stream::{FrameInterpretation, WireStreamInterpreter, adapt_sse_stream};
use crate::http::{UsageEmitter, build_http_client, map_request_error, map_response_error};
use crate::message::{ChatRequest, ChatResponse, FinishReason, ProviderCapabilities};
use crate::observe::AttemptObserver;
use crate::sse::SseFrame;
use crate::stream::DeltaStream;
use crate::wire::openai::{
    OpenAiStreamInterpreter, WireChatResponse, from_wire_response, to_wire_request,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const OPENAI_PROVIDER_LABEL: &str = "openai";
const OPENAI_PROTOCOL: &str = "openai-chat-completions";

/// OpenAI Chat Completions クライアントの設定。
#[derive(Clone)]
pub struct OpenAiConfig {
    /// API のベース URL。
    pub base_url: String,
    /// 非ストリーミングリクエスト全体のタイムアウト。
    pub timeout: Duration,
    /// usage イベントの発行先。未指定なら発行しない。
    pub event_bus: Option<Arc<EventBus>>,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: Duration::from_secs(60),
            event_bus: None,
        }
    }
}

/// OpenAI Chat Completions API を呼び出す provider クライアント。
pub struct OpenAiClient {
    inner: ChatCompletionsClient,
}

impl OpenAiClient {
    /// 設定から OpenAI クライアントを構築する。
    ///
    /// # Errors
    /// HTTP クライアントを構築できない場合 [`ProviderError`] を返す。
    pub fn new(config: OpenAiConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            inner: ChatCompletionsClient::new(ChatCompletionsConfig {
                base_url: config.base_url,
                provider_label: OPENAI_PROVIDER_LABEL.to_string(),
                timeout: config.timeout,
                event_bus: config.event_bus,
                profile: None,
            })?,
        })
    }

    /// 観測イベントへ記録する provider profile を設定する。
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.inner.profile = Some(profile.into());
        self
    }
}

#[async_trait]
impl ProviderClient for OpenAiClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: false,
        }
    }

    async fn send(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.inner.send(auth, request).await
    }

    async fn stream(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        self.inner
            .stream(
                auth,
                request,
                OpenAiInterpreterAdapter(OpenAiStreamInterpreter::new()),
            )
            .await
    }
}

/// OpenAI形式の送受信処理を共有する内部設定。
pub(crate) struct ChatCompletionsConfig {
    /// API のベース URL。
    pub(crate) base_url: String,
    /// usage イベントに記録するプロバイダ識別子。
    pub(crate) provider_label: String,
    /// 非ストリーミングリクエスト全体のタイムアウト。
    pub(crate) timeout: Duration,
    /// usage イベントの発行先。
    pub(crate) event_bus: Option<Arc<EventBus>>,
    /// 観測イベントへ記録する provider profile。
    pub(crate) profile: Option<String>,
}

/// OpenAI wire 形式を共有する Chat Completions HTTP クライアント。
pub(crate) struct ChatCompletionsClient {
    http: reqwest::Client,
    endpoint: String,
    provider_label: String,
    timeout: Duration,
    event_bus: Option<Arc<EventBus>>,
    pub(crate) profile: Option<String>,
}

impl ChatCompletionsClient {
    /// 共通設定から HTTP クライアントを構築する。
    ///
    /// # Errors
    /// reqwest クライアントの構築に失敗した場合 [`ProviderError`] を返す。
    pub(crate) fn new(config: ChatCompletionsConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            http: build_http_client(None)?,
            endpoint: format!("{}/chat/completions", config.base_url.trim_end_matches('/')),
            provider_label: config.provider_label,
            timeout: config.timeout,
            event_bus: config.event_bus,
            profile: config.profile,
        })
    }

    /// 非ストリーミング Chat Completions を送信する。
    ///
    /// # Errors
    /// 送信、HTTP応答、JSON解析、canonical変換に失敗した場合 [`ProviderError`] を返す。
    pub(crate) async fn send(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let model = request.model.clone();
        let wire_request = to_wire_request(request, false);
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            self.provider_label.clone(),
            self.profile.clone(),
            OPENAI_PROTOCOL,
            model.clone(),
            false,
        );
        let request = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&auth.api_key)
            .json(&wire_request)
            .timeout(self.timeout)
            .build()
            .map_err(map_request_error)?;
        observer.emit_started();
        let response = self
            .http
            .execute(request)
            .await
            .map_err(map_request_error)
            .inspect_err(|error| {
                observer.emit_failed(error);
            })?;
        if !response.status().is_success() {
            let error = map_response_error(response).await;
            observer.emit_failed(&error);
            return Err(error);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(map_request_error)
            .inspect_err(|error| {
                observer.emit_failed(error);
            })?;
        let wire_response: WireChatResponse = serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::InvalidJson {
                detail: format!("OpenAI response の解析に失敗しました: {error}"),
            })
            .inspect_err(|error| {
                observer.emit_failed(error);
            })?;
        let response = from_wire_response(&wire_response).inspect_err(|error| {
            observer.emit_failed(error);
        })?;
        UsageEmitter::new(self.event_bus.clone(), self.provider_label.clone())
            .emit_usage(&model, &response.usage);
        observer.emit_completed(&response.usage, response.finish_reason.clone());
        Ok(response)
    }

    /// Chat Completions SSE を canonical 差分ストリームへ変換する。
    ///
    /// # Errors
    /// リクエスト送信またはHTTP応答に失敗した場合 [`ProviderError`] を返す。
    pub(crate) async fn stream<I>(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
        interpreter: I,
    ) -> Result<DeltaStream, ProviderError>
    where
        I: WireStreamInterpreter + 'static,
    {
        let model = request.model.clone();
        let wire_request = to_wire_request(request, true);
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            self.provider_label.clone(),
            self.profile.clone(),
            OPENAI_PROTOCOL,
            model.clone(),
            true,
        );
        let request = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&auth.api_key)
            .json(&wire_request)
            .build()
            .map_err(map_request_error)?;
        observer.emit_started();
        let response = self
            .http
            .execute(request)
            .await
            .map_err(map_request_error)
            .inspect_err(|error| {
                observer.emit_failed(error);
            })?;
        if !response.status().is_success() {
            let error = map_response_error(response).await;
            observer.emit_failed(&error);
            return Err(error);
        }
        Ok(adapt_sse_stream(
            response.bytes_stream(),
            interpreter,
            UsageEmitter::new(self.event_bus.clone(), self.provider_label.clone()),
            model,
            observer,
        ))
    }
}

struct OpenAiInterpreterAdapter(OpenAiStreamInterpreter);

impl WireStreamInterpreter for OpenAiInterpreterAdapter {
    fn interpret(&mut self, frame: SseFrame) -> Result<FrameInterpretation, ProviderError> {
        let events = self.0.interpret(&frame)?;
        let completion = if self.0.is_done() {
            let (usage, reason) = self.0.take_result();
            Some((
                usage.unwrap_or_default(),
                reason.unwrap_or(FinishReason::Stop),
            ))
        } else {
            None
        };
        Ok(FrameInterpretation { events, completion })
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
        Ok(FrameInterpretation::default())
    }
}
