//! OpenAI provider 実装を提供します。
//!
//! 公式 OpenAI API へは Responses API (`POST {base_url}/responses`) で接続します。
//! wire 変換と SSE 解釈は Codex と共有する [`crate::wire::responses`] が担います。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::EventBus;
use futures_util::StreamExt;

use crate::auth::ProviderAuth;
use crate::client::ProviderClient;
use crate::error::ProviderError;
use crate::http::stream::adapt_sse_stream;
use crate::http::{UsageEmitter, build_http_client, map_request_error, map_response_error};
use crate::message::{ChatRequest, ChatResponse, ProviderCapabilities};
use crate::observe::AttemptObserver;
use crate::stream::{DeltaStream, StreamEvent};
use crate::wire::responses::{
    ResponsesDialect, ResponsesRequestOptions, ResponsesStreamInterpreter, to_wire_request,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const OPENAI_PROVIDER_LABEL: &str = "openai";
const OPENAI_PROTOCOL: &str = "openai-responses";

/// OpenAI Responses クライアントの設定。
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

/// OpenAI Responses API を呼び出す provider クライアント。
pub struct OpenAiClient {
    http: reqwest::Client,
    endpoint: String,
    timeout: Duration,
    event_bus: Option<Arc<EventBus>>,
    profile: Option<String>,
}

impl OpenAiClient {
    /// 設定から OpenAI クライアントを構築する。
    ///
    /// # Errors
    /// HTTP クライアントを構築できない場合 [`ProviderError`] を返す。
    pub fn new(config: OpenAiConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            http: build_http_client(None)?,
            endpoint: format!("{}/responses", config.base_url.trim_end_matches('/')),
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

    /// Responses SSE を canonical 差分ストリームへ変換する。
    ///
    /// Responses API は送信方式に関わらず SSE で応答させるため、非ストリーミングも
    /// 同じ経路を使います。usage 発行は SSE 完了ポンプ ([`adapt_sse_stream`]) が担い、
    /// Err を返した attempt の usage 発行件数は 0 です。全体タイムアウトは長い正当な
    /// SSE を切らないよう非ストリーミング時だけ付けます。
    async fn execute(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
        streaming: bool,
    ) -> Result<DeltaStream, ProviderError> {
        let wire_request = to_wire_request(
            request,
            &ResponsesRequestOptions {
                dialect: ResponsesDialect::OpenAi,
                prompt_cache_key: request
                    .observation
                    .as_ref()
                    .map(|context| context.run_id.clone()),
            },
        );
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            OPENAI_PROVIDER_LABEL,
            self.profile.clone(),
            OPENAI_PROTOCOL,
            request.model.clone(),
            streaming,
            request.observation.clone(),
        )
        .with_service_tier(request.service_tier)
        .with_reasoning_effort(wire_request.reasoning_effort())
        .with_cache_observation(&wire_request)
        .with_known_secret(&auth.api_key);
        let mut builder = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&auth.api_key)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&wire_request);
        if !streaming {
            builder = builder.timeout(self.timeout);
        }
        let http_request = builder.build().map_err(map_request_error)?;
        observer.emit_started();
        let response = self
            .http
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
            ResponsesStreamInterpreter::new(),
            UsageEmitter::new(self.event_bus.clone(), OPENAI_PROVIDER_LABEL),
            request.model.clone(),
            observer,
        ))
    }
}

#[async_trait]
impl ProviderClient for OpenAiClient {
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
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let mut stream = self.execute(auth, request, false).await?;
        while let Some(event) = stream.next().await {
            if let StreamEvent::Completed { response } = event? {
                return Ok(response);
            }
        }
        Err(ProviderError::Request(
            "openai response ended without completion".to_string(),
        ))
    }

    async fn stream(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        self.execute(auth, request, true).await
    }
}
