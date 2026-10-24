//! OpenAI Chat Completions 互換 API の HTTP 送受信を提供します。

use std::sync::Arc;
use std::time::Duration;

use event_bus::EventBus;

use crate::auth::ProviderAuth;
use crate::error::ProviderError;
use crate::http::stream::{WireStreamInterpreter, adapt_sse_stream};
use crate::http::{UsageEmitter, build_http_client, map_request_error, map_response_error};
use crate::message::{ChatRequest, ChatResponse};
use crate::observe::AttemptObserver;
use crate::stream::DeltaStream;
use crate::wire::openai::{WireChatRequest, WireChatResponse, from_wire_response, to_wire_request};

const CHAT_COMPLETIONS_PROTOCOL: &str = "openai-chat-completions";

/// Output limit parameter supported by the selected provider client.
pub(crate) enum OutputLimitField {
    MaxTokens,
    MaxCompletionTokens,
}

impl OutputLimitField {
    pub(crate) fn for_compatible_endpoint(base_url: &str) -> Self {
        if reqwest::Url::parse(base_url).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("api.openai.com")
                && url.port_or_known_default() == Some(443)
        }) {
            Self::MaxCompletionTokens
        } else {
            Self::MaxTokens
        }
    }
}

/// Chat Completions の送受信処理を構築する内部設定。
pub(crate) struct ChatCompletionsConfig {
    pub(crate) output_limit_field: OutputLimitField,
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

/// OpenAI Chat Completions wire 形式の HTTP クライアント。
pub(crate) struct ChatCompletionsClient {
    output_limit_field: OutputLimitField,
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
            output_limit_field: config.output_limit_field,
            endpoint: format!("{}/chat/completions", config.base_url.trim_end_matches('/')),
            provider_label: config.provider_label,
            timeout: config.timeout,
            event_bus: config.event_bus,
            profile: config.profile,
        })
    }

    pub(crate) fn wire_request(&self, request: &ChatRequest, streaming: bool) -> WireChatRequest {
        let mut wire = to_wire_request(request, streaming);
        if matches!(
            self.output_limit_field,
            OutputLimitField::MaxCompletionTokens
        ) {
            wire.max_completion_tokens = wire.max_tokens.take();
        }
        wire
    }

    /// 非ストリーミング Chat Completions を送信する。
    ///
    /// usage は HTTP ステータス成功・ボディ読み取り・JSON パース・canonical
    /// 変換のすべてが成功した後にちょうど 1 回だけ発行する。どのエラー経路も
    /// 発行前に return するため、失敗した attempt の usage 発行件数は 0 に
    /// なる。このメソッドが非ストリーミング経路で唯一の usage 発行地点である。
    ///
    /// # Errors
    /// 送信、HTTP応答、JSON解析、canonical変換に失敗した場合 [`ProviderError`] を返す。
    pub(crate) async fn send(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let model = request.model.clone();
        let wire_request = self.wire_request(request, false);
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            self.provider_label.clone(),
            self.profile.clone(),
            CHAT_COMPLETIONS_PROTOCOL,
            model.clone(),
            false,
            request.observation.clone(),
        )
        .with_service_tier(request.service_tier)
        .with_reasoning_effort(wire_request.reasoning_effort.as_deref())
        .with_cache_observation(&wire_request)
        .with_known_secret(&auth.api_key);
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
    /// ストリーミングの usage 発行は SSE 完了ポンプ ([`adapt_sse_stream`]) が
    /// 担う。このメソッド自体は usage を発行せず、Err を返した場合はその
    /// attempt の usage 発行件数は 0 になる。
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
        let wire_request = self.wire_request(request, true);
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            self.provider_label.clone(),
            self.profile.clone(),
            CHAT_COMPLETIONS_PROTOCOL,
            model.clone(),
            true,
            request.observation.clone(),
        )
        .with_service_tier(request.service_tier)
        .with_reasoning_effort(wire_request.reasoning_effort.as_deref())
        .with_cache_observation(&wire_request)
        .with_known_secret(&auth.api_key);
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
