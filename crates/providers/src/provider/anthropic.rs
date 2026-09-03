//! Anthropic provider 実装を提供します。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::EventBus;

use crate::auth::ProviderAuth;
use crate::client::ProviderClient;
use crate::error::ProviderError;
use crate::http::stream::{FrameInterpretation, WireStreamInterpreter, adapt_sse_stream};
use crate::http::{UsageEmitter, build_http_client, map_request_error, map_response_error};
use crate::message::{ChatRequest, ChatResponse, ProviderCapabilities};
use crate::observe::AttemptObserver;
use crate::sse::SseFrame;
use crate::stream::DeltaStream;
use crate::wire::anthropic::{
    AnthropicStreamInterpreter, WireMessagesResponse, from_wire_response, to_wire_request,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const PROVIDER_LABEL: &str = "anthropic";
const ANTHROPIC_PROTOCOL: &str = "anthropic-messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Messages API クライアントの設定。
#[derive(Clone)]
pub struct AnthropicConfig {
    /// Messages API のベース URL。
    pub base_url: String,
    /// 非ストリーミングリクエスト全体のタイムアウト。
    pub timeout: Duration,
    /// usage を通知するイベントバス。未指定なら通知しない。
    pub event_bus: Option<Arc<EventBus>>,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: DEFAULT_TIMEOUT,
            event_bus: None,
        }
    }
}

/// Anthropic Messages API を canonical provider 契約へ接続するクライアント。
pub struct AnthropicClient {
    http_client: reqwest::Client,
    base_url: String,
    timeout: Duration,
    event_bus: Option<Arc<EventBus>>,
    profile: Option<String>,
}

impl AnthropicClient {
    /// 設定から Anthropic クライアントを構築する。
    ///
    /// # Errors
    /// HTTP クライアントを構築できない場合 [`ProviderError`] を返す。
    pub fn new(config: AnthropicConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            http_client: build_http_client(None)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            timeout: config.timeout,
            event_bus: config.event_bus,
            profile: None,
        })
    }

    /// 観測イベントへ記録する provider profile を設定する。
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    fn messages_url(&self) -> String {
        format!("{}/messages", self.base_url)
    }
}

#[async_trait]
impl ProviderClient for AnthropicClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: true,
        }
    }

    async fn send(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let wire_request = to_wire_request(request, false);
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            PROVIDER_LABEL,
            self.profile.clone(),
            ANTHROPIC_PROTOCOL,
            request.model.clone(),
            false,
            request.observation.clone(),
        );
        let request_builder = self
            .http_client
            .post(self.messages_url())
            .header("x-api-key", &auth.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&wire_request)
            .timeout(self.timeout);
        let http_request = request_builder.build().map_err(map_request_error)?;
        observer.emit_started();
        let response = self
            .http_client
            .execute(http_request)
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
        let wire = response
            .json::<WireMessagesResponse>()
            .await
            .map_err(|error| ProviderError::InvalidJson {
                detail: error.to_string(),
            })
            .inspect_err(|error| {
                observer.emit_failed(error);
            })?;
        let response = from_wire_response(wire);
        UsageEmitter::new(self.event_bus.clone(), PROVIDER_LABEL)
            .emit_usage(&request.model, &response.usage);
        observer.emit_completed(&response.usage, response.finish_reason.clone());
        Ok(response)
    }

    async fn stream(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        let wire_request = to_wire_request(request, true);
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            PROVIDER_LABEL,
            self.profile.clone(),
            ANTHROPIC_PROTOCOL,
            request.model.clone(),
            true,
            request.observation.clone(),
        );
        let http_request = self
            .http_client
            .post(self.messages_url())
            .header("x-api-key", &auth.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&wire_request)
            .build()
            .map_err(map_request_error)?;
        observer.emit_started();
        let response = self
            .http_client
            .execute(http_request)
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
            AnthropicInterpreterAdapter(AnthropicStreamInterpreter::new()),
            UsageEmitter::new(self.event_bus.clone(), PROVIDER_LABEL),
            request.model.clone(),
            observer,
        ))
    }
}

struct AnthropicInterpreterAdapter(AnthropicStreamInterpreter);

impl WireStreamInterpreter for AnthropicInterpreterAdapter {
    fn interpret(&mut self, frame: SseFrame) -> Result<FrameInterpretation, ProviderError> {
        let events = self.0.interpret(&frame)?;
        let completion = self.0.is_done().then(|| self.0.take_result());
        Ok(FrameInterpretation { events, completion })
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
        Ok(FrameInterpretation::default())
    }
}
