//! Anthropic provider 実装を提供します。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::EventBus;
use serde::Serialize;

use crate::auth::ProviderAuth;
use crate::client::ProviderClient;
use crate::error::ProviderError;
use crate::http::stream::{FrameInterpretation, WireStreamInterpreter, adapt_sse_stream};
use crate::http::{
    UsageEmitter, build_http_client_without_redirects, map_request_error, map_response_error,
};
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
    claude_oauth: bool,
    claude_account_id: Option<String>,
    claude_session_id: String,
}

#[derive(Serialize)]
struct MessagesRequest {
    #[serde(flatten)]
    wire: crate::wire::anthropic::WireMessagesRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
}

impl AnthropicClient {
    /// 設定から Anthropic クライアントを構築する。
    ///
    /// # Errors
    /// HTTP クライアントを構築できない場合 [`ProviderError`] を返す。
    pub fn new(config: AnthropicConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            http_client: build_http_client_without_redirects(None)?,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            timeout: config.timeout,
            event_bus: config.event_bus,
            profile: None,
            claude_oauth: false,
            claude_account_id: None,
            claude_session_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    /// 観測イベントへ記録する provider profile を設定する。
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Enable Claude subscription Bearer authentication and stable CLI identity.
    pub fn with_claude_oauth(mut self) -> Self {
        self.claude_oauth = true;
        self
    }

    /// Snapshot identity at construction; token refresh cannot rewrite sent metadata.
    pub(super) fn with_claude_account_id(mut self, account_id: Option<String>) -> Self {
        self.claude_account_id = account_id;
        self
    }

    fn wire_request(&self, request: &ChatRequest, stream: bool) -> MessagesRequest {
        let mut wire = to_wire_request(request, stream);
        // Canonical reasoning carries no provider signature. Anthropic rejects
        // unsigned assistant thinking, including histories from other providers.
        for message in &mut wire.messages {
            message.content.retain(|block| {
                !matches!(
                    block,
                    crate::wire::anthropic::WireContentBlock::Thinking { .. }
                )
            });
            if message.content.is_empty() {
                message
                    .content
                    .push(crate::wire::anthropic::WireContentBlock::Text {
                        text: "[Reasoning omitted from replay]".into(),
                        cache_control: None,
                    });
            }
        }
        if self.claude_oauth {
            super::claude::shape_oauth_request(&mut wire);
        }
        let metadata = self.claude_oauth.then(|| {
            super::claude::request_metadata(
                self.claude_account_id.as_deref(),
                self.profile.as_deref(),
                &self.base_url,
                &self.claude_session_id,
                request.observation.as_ref().map(|o| o.run_id.as_str()),
            )
        });
        MessagesRequest { wire, metadata }
    }

    fn authenticated_request(&self, auth: &ProviderAuth) -> reqwest::RequestBuilder {
        let builder = self
            .http_client
            .post(self.messages_url())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if self.claude_oauth {
            builder
                .bearer_auth(&auth.api_key)
                .header(reqwest::header::ACCEPT, "application/json")
                .header("anthropic-beta", super::claude::CLAUDE_INFERENCE_BETAS)
                .header("user-agent", super::claude::CLAUDE_USER_AGENT)
                .header("x-app", "cli")
                .header("anthropic-dangerous-direct-browser-access", "true")
        } else {
            builder.header("x-api-key", &auth.api_key)
        }
    }

    fn messages_url(&self) -> String {
        format!(
            "{}/messages{}",
            self.base_url,
            if self.claude_oauth { "?beta=true" } else { "" }
        )
    }

    fn provider_label(&self) -> &'static str {
        if self.claude_oauth {
            "claude"
        } else {
            PROVIDER_LABEL
        }
    }

    async fn response_error(&self, response: reqwest::Response) -> ProviderError {
        if !self.claude_oauth {
            return map_response_error(response).await;
        }
        let status = response.status().as_u16();
        if status == 429 {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            ProviderError::RateLimited { retry_after }
        } else {
            ProviderError::Http {
                status,
                body: "Claude request rejected".into(),
            }
        }
    }
}

#[async_trait]
impl ProviderClient for AnthropicClient {
    async fn list_models(&self, auth: &ProviderAuth) -> Result<Option<Vec<String>>, ProviderError> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;
        let mut seen_pages = std::collections::HashSet::new();
        loop {
            let url = format!("{}/models", self.base_url);
            let mut builder = self
                .http_client
                .get(url)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .timeout(self.timeout)
                .query(&[("limit", "1000")]);
            if self.claude_oauth {
                builder = builder
                    .bearer_auth(&auth.api_key)
                    .header("anthropic-beta", super::claude::CLAUDE_INFERENCE_BETAS)
                    .header("user-agent", super::claude::CLAUDE_USER_AGENT);
            } else {
                builder = builder.header("x-api-key", &auth.api_key);
            }
            if let Some(after_id) = &after_id {
                builder = builder.query(&[("after_id", after_id)]);
            }
            let response = builder.send().await.map_err(map_request_error)?;
            if !response.status().is_success() {
                return Err(self.response_error(response).await);
            }
            let body = super::claude::oauth::bounded_json(response)
                .await
                .map_err(|_| ProviderError::InvalidJson {
                    detail: "invalid Anthropic models response".into(),
                })?;
            let data = body["data"]
                .as_array()
                .ok_or_else(|| ProviderError::InvalidJson {
                    detail: "Anthropic models data missing".into(),
                })?;
            for entry in data {
                let id = entry["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ProviderError::InvalidJson {
                        detail: "Anthropic model id missing".into(),
                    })?;
                models.push(id.to_owned());
                if let Some(alias) = claude_model_alias(id) {
                    models.push(alias.into());
                }
            }
            if !body["has_more"].as_bool().unwrap_or(false) {
                break;
            }
            let last = body["last_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ProviderError::InvalidJson {
                    detail: "Anthropic pagination cursor missing".into(),
                })?
                .to_owned();
            if !seen_pages.insert(last.clone()) || seen_pages.len() > 100 {
                return Err(ProviderError::InvalidJson {
                    detail: "Anthropic model pagination did not advance".into(),
                });
            }
            after_id = Some(last);
        }
        models.sort();
        models.dedup();
        Ok(Some(models))
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: !self.claude_oauth,
        }
    }

    async fn send(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let wire_request = self.wire_request(request, false);
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            self.provider_label(),
            self.profile.clone(),
            ANTHROPIC_PROTOCOL,
            request.model.clone(),
            false,
            request.observation.clone(),
        )
        .with_cache_observation(&wire_request);
        let request_builder = self
            .authenticated_request(auth)
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
            let error = self.response_error(response).await;
            observer.emit_failed(&error);
            return Err(error);
        }
        let wire = response
            .json::<WireMessagesResponse>()
            .await
            .map_err(|error| ProviderError::InvalidJson {
                detail: if self.claude_oauth {
                    "invalid Claude response JSON".into()
                } else {
                    error.to_string()
                },
            })
            .inspect_err(|error| {
                observer.emit_failed(error);
            })?;
        let mut response = from_wire_response(wire);
        if self.claude_oauth {
            super::claude::restore_response_tool_names(&mut response);
        }
        UsageEmitter::new(self.event_bus.clone(), self.provider_label())
            .emit_usage(&request.model, &response.usage);
        observer.emit_completed(&response.usage, response.finish_reason.clone());
        Ok(response)
    }

    async fn stream(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        let wire_request = self.wire_request(request, true);
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            self.provider_label(),
            self.profile.clone(),
            ANTHROPIC_PROTOCOL,
            request.model.clone(),
            true,
            request.observation.clone(),
        )
        .with_cache_observation(&wire_request);
        let http_request = self
            .authenticated_request(auth)
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
            let error = self.response_error(response).await;
            observer.emit_failed(&error);
            return Err(error);
        }
        Ok(adapt_sse_stream(
            response.bytes_stream(),
            AnthropicInterpreterAdapter {
                inner: AnthropicStreamInterpreter::new(),
                claude_oauth: self.claude_oauth,
            },
            UsageEmitter::new(self.event_bus.clone(), self.provider_label()),
            request.model.clone(),
            observer,
        ))
    }
}

struct AnthropicInterpreterAdapter {
    inner: AnthropicStreamInterpreter,
    claude_oauth: bool,
}

/// New Claude family/version IDs have a stable alias for their dated snapshot.
/// Older `claude-3-5-sonnet-*` aliases use different naming and are not inferred.
fn claude_model_alias(id: &str) -> Option<&str> {
    let (alias, date) = id.rsplit_once('-')?;
    if date.len() != 8
        || !date.bytes().all(|b| b.is_ascii_digit())
        || chrono::NaiveDate::parse_from_str(date, "%Y%m%d").is_err()
    {
        return None;
    }
    let parts: Vec<_> = alias.split('-').collect();
    if let ["claude", "sonnet" | "opus" | "haiku", major, minor] = parts.as_slice()
        && [major, minor]
            .iter()
            .all(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
    {
        Some(alias)
    } else {
        None
    }
}

impl WireStreamInterpreter for AnthropicInterpreterAdapter {
    fn interpret(&mut self, frame: SseFrame) -> Result<FrameInterpretation, ProviderError> {
        let mut events = self.inner.interpret(&frame).map_err(|error| {
            if self.claude_oauth {
                match error {
                    ProviderError::Http { status, .. } => ProviderError::Http {
                        status,
                        body: "Claude stream rejected".into(),
                    },
                    ProviderError::RateLimited { retry_after } => {
                        ProviderError::RateLimited { retry_after }
                    }
                    _ => ProviderError::InvalidSse {
                        detail: "invalid Claude stream frame".into(),
                    },
                }
            } else {
                error
            }
        })?;
        let completion = self.inner.is_done().then(|| self.inner.take_result());
        if self.claude_oauth {
            for event in &mut events {
                super::claude::restore_event_tool_names(event);
            }
        }
        Ok(FrameInterpretation { events, completion })
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
        Ok(FrameInterpretation::default())
    }
}
