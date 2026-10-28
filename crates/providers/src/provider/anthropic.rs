//! Anthropic provider 実装を提供します。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::EventBus;
use serde::Serialize;

mod transport;
pub(super) mod web_search;
use super::claude::transport::ClaudeTransport;
use transport::PreparedRequest;

use super::claude::{
    ClaudeInstallationId, ClaudeReasoningControls, ClaudeRequestContext, ClaudeToolAdapter,
    ClaudeTransportContext, ClaudeWireProfile,
};
use crate::auth::ProviderAuth;
use crate::client::ProviderClient;
use crate::error::ProviderError;
use crate::http::stream::{
    FrameInterpretation, WireStreamInterpreter, adapt_sse_stream_with_errors,
};
use crate::http::{UsageEmitter, build_http_client_without_redirects};
use crate::message::{
    ChatRequest, ChatResponse, Message, ObservationContext, ProviderCapabilities,
};
use crate::observe::AttemptObserver;
use crate::sse::SseFrame;
use crate::stream::DeltaStream;
use crate::wire::anthropic::{
    AnthropicStreamInterpreter, WireMessagesResponse, bind_response, discard_unbound_thinking,
    from_wire_response, prefix_hash, to_wire_request,
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
#[derive(Clone)]
pub struct AnthropicClient {
    http_client: reqwest::Client,
    base_url: String,
    timeout: Duration,
    event_bus: Option<Arc<EventBus>>,
    profile: Option<String>,
    claude_oauth: bool,
    claude_transport: Option<ClaudeTransport>,
    claude_account_id: Option<String>,
    claude_session_id: String,
    claude_installation_id: Option<ClaudeInstallationId>,
    claude_request_context: ClaudeTransportContext,
}

#[derive(Serialize)]
struct MessagesRequest {
    #[serde(flatten)]
    wire: crate::wire::anthropic::WireMessagesRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
    #[serde(flatten)]
    reasoning: Option<ClaudeReasoningControls>,
}

impl MessagesRequest {
    fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning
            .as_ref()
            .and_then(|controls| controls.output_config.as_ref())
            .and_then(|config| config["effort"].as_str())
    }
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
            claude_transport: None,
            claude_account_id: None,
            claude_session_id: uuid::Uuid::new_v4().to_string(),
            claude_installation_id: None,
            claude_request_context: ClaudeTransportContext::default(),
        })
    }

    /// 観測イベントへ記録する provider profile を設定する。
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Enable Claude subscription Bearer authentication and stable CLI identity.
    pub fn with_claude_oauth(mut self) -> Result<Self, ProviderError> {
        self.claude_transport = Some(ClaudeTransport::new()?);
        self.claude_oauth = true;
        self.claude_installation_id = Some(super::claude::process_installation_id());
        Ok(self)
    }

    /// Pin the authenticated bundle's account on a request-local client clone.
    pub(super) fn with_claude_account_id(mut self, account_id: Option<String>) -> Self {
        self.claude_account_id = account_id;
        self
    }

    pub(super) fn with_claude_installation_id(mut self, id: ClaudeInstallationId) -> Self {
        self.claude_installation_id = Some(id);
        self
    }

    fn wire_request(
        &self,
        request: &ChatRequest,
        stream: bool,
    ) -> Result<(MessagesRequest, Option<ClaudeToolAdapter>), ProviderError> {
        let mut wire = to_wire_request(request, stream);
        let reasoning = self.reasoning_controls(
            &request.model,
            wire.max_tokens,
            request.reasoning_effort.as_deref(),
        )?;
        // Thinking needs the default temperature on every transport; the stricter
        // per-model sampling rule is only applied to the pinned Claude subscription profile.
        if reasoning
            .as_ref()
            .is_some_and(ClaudeReasoningControls::is_enabled)
            || (self.claude_oauth && ClaudeWireProfile.requires_default_temperature(&request.model))
        {
            if wire.temperature.is_some_and(|value| value != 1.0) {
                return Err(ProviderError::Request(
                    "Claude thinking and this model require the default temperature".into(),
                ));
            }
            wire.temperature = None;
        }
        let adapter = self
            .claude_oauth
            .then(|| super::claude::shape_oauth_request(&mut wire))
            .transpose()?;
        discard_unbound_thinking(&mut wire);
        let metadata = self.claude_oauth.then(|| {
            super::claude::request_metadata(
                &self
                    .claude_installation_id
                    .unwrap_or_else(super::claude::process_installation_id),
                self.claude_account_id.as_deref(),
                &self.claude_session_id,
                request.observation.as_ref().map(|o| o.run_id.as_str()),
            )
        });
        Ok((
            MessagesRequest {
                wire,
                metadata,
                reasoning,
            },
            adapter,
        ))
    }

    /// Subscription requests always carry a (possibly empty) profile and fail on unsupported
    /// efforts; API-key requests carry controls only for a supported explicit effort.
    fn reasoning_controls(
        &self,
        model: &str,
        max_tokens: u64,
        effort: Option<&str>,
    ) -> Result<Option<ClaudeReasoningControls>, ProviderError> {
        if self.claude_oauth {
            return ClaudeWireProfile
                .reasoning(model, max_tokens, effort)
                .map(Some);
        }
        Ok(
            Some(ClaudeWireProfile.api_key_reasoning(model, max_tokens, effort))
                .filter(|controls| *controls != ClaudeReasoningControls::default()),
        )
    }

    fn authenticated_request(
        &self,
        auth: &ProviderAuth,
        request: &MessagesRequest,
        timeout: Option<Duration>,
        observation: Option<&ObservationContext>,
    ) -> Result<(PreparedRequest, Option<ClaudeRequestContext>), ProviderError> {
        self.authenticated_request_with_tools(auth, request, timeout, observation, None)
    }

    fn authenticated_request_with_tools(
        &self,
        auth: &ProviderAuth,
        request: &MessagesRequest,
        timeout: Option<Duration>,
        observation: Option<&ObservationContext>,
        server_tools: Option<&[serde_json::Value]>,
    ) -> Result<(PreparedRequest, Option<ClaudeRequestContext>), ProviderError> {
        if let Some(transport) = &self.claude_transport {
            let identity: serde_json::Value = serde_json::from_str(
                request
                    .metadata
                    .as_ref()
                    .and_then(|value| value["user_id"].as_str())
                    .ok_or_else(|| {
                        ProviderError::Request("Claude request identity missing".into())
                    })?,
            )
            .map_err(|_| ProviderError::Request("Claude request identity invalid".into()))?;
            let session_id = identity["session_id"]
                .as_str()
                .ok_or_else(|| ProviderError::Request("Claude session identity missing".into()))?;
            let device_id = identity["device_id"]
                .as_str()
                .ok_or_else(|| ProviderError::Request("Claude device identity missing".into()))?;
            let controls = request
                .reasoning
                .as_ref()
                .ok_or_else(|| ProviderError::Request("Claude request profile missing".into()))?;
            let context = self
                .claude_request_context
                .begin(session_id, device_id, observation)?;
            let mut headers = ClaudeWireProfile.headers(session_id, controls)?;
            context.apply_headers(&mut headers)?;
            let body = match server_tools {
                Some(tools) => super::claude::serialize_request_with_tools(
                    &request.wire,
                    request.metadata.as_ref(),
                    controls,
                    Some(&context),
                    Some(tools),
                )?,
                None => super::claude::serialize_request(
                    &request.wire,
                    request.metadata.as_ref(),
                    controls,
                    Some(&context),
                )?,
            };
            let mut builder = transport
                .request(wreq::Method::POST, &self.messages_url(), auth, headers)
                .body(body);
            if let Some(timeout) = timeout {
                builder = builder.timeout(timeout);
            }
            Ok((PreparedRequest::claude(builder)?, Some(context)))
        } else {
            let mut builder = self
                .http_client
                .post(self.messages_url())
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header("x-api-key", &auth.api_key);
            builder = if let Some(tools) = server_tools {
                let mut body = serde_json::to_value(request).map_err(|_| {
                    ProviderError::Request("Anthropic request serialization failed".into())
                })?;
                body["tools"] = serde_json::to_value(tools).map_err(|_| {
                    ProviderError::Request("Anthropic search tools serialization failed".into())
                })?;
                builder.json(&body)
            } else {
                builder.json(request)
            };
            if let Some(timeout) = timeout {
                builder = builder.timeout(timeout);
            }
            Ok((PreparedRequest::api(builder)?, None))
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
}

#[async_trait]
impl ProviderClient for AnthropicClient {
    fn hosted_web_search(&self) -> Option<&dyn crate::HostedWebSearch> {
        Some(self)
    }

    async fn list_models(&self, auth: &ProviderAuth) -> Result<Option<Vec<String>>, ProviderError> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;
        let mut seen_pages = std::collections::HashSet::new();
        loop {
            let url = format!("{}/models", self.base_url);
            let prepared = if let Some(transport) = &self.claude_transport {
                let mut builder = transport
                    .request(
                        wreq::Method::GET,
                        &url,
                        auth,
                        ClaudeWireProfile.headers(
                            &self.claude_session_id,
                            &ClaudeReasoningControls::default(),
                        )?,
                    )
                    .timeout(self.timeout)
                    .query(&[("limit", "1000")]);
                if let Some(after_id) = &after_id {
                    builder = builder.query(&[("after_id", after_id)]);
                }
                PreparedRequest::claude(builder)?
            } else {
                let mut builder = self
                    .http_client
                    .get(url)
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .header("x-api-key", &auth.api_key)
                    .timeout(self.timeout)
                    .query(&[("limit", "1000")]);
                if let Some(after_id) = &after_id {
                    builder = builder.query(&[("after_id", after_id)]);
                }
                PreparedRequest::api(builder)?
            };
            let response = prepared.execute().await?;
            if !response.is_success() {
                return Err(response.error().await);
            }
            let body = response.bounded_json().await?;
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
            reasoning: true,
        }
    }

    async fn send(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let (wire_request, tool_adapter) = self.wire_request(request, false)?;
        let response_prefix = prefix_hash(&wire_request.wire, wire_request.wire.messages.len());
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            self.provider_label(),
            self.profile.clone(),
            ANTHROPIC_PROTOCOL,
            request.model.clone(),
            false,
            request.observation.clone(),
        )
        .with_reasoning_effort(wire_request.reasoning_effort())
        .with_cache_observation(&wire_request)
        .with_known_secret(&auth.api_key);
        let (http_request, claude_context) = self.authenticated_request(
            auth,
            &wire_request,
            Some(self.timeout),
            request.observation.as_ref(),
        )?;
        observer.emit_started();
        let response = http_request.execute().await.inspect_err(|error| {
            observer.emit_failed(error);
        })?;
        if !response.is_success() {
            let error = response.error().await;
            observer.emit_failed(&error);
            return Err(error);
        }
        let provider_request_id = response.request_id().map(str::to_owned);
        let wire = response
            .json::<WireMessagesResponse>()
            .await
            .inspect_err(|error| {
                observer.emit_failed(error);
            })?;
        let provider_message_id = wire.id.clone();
        let mut response = from_wire_response(wire);
        bind_response(&mut response.message, &response_prefix);
        if let Some(adapter) = tool_adapter {
            adapter.restore_message(&mut response.message);
        }
        if let Some(context) = claude_context {
            context.complete(
                provider_request_id.as_deref(),
                provider_message_id.as_deref(),
            );
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
        let (wire_request, tool_adapter) = self.wire_request(request, true)?;
        let response_prefix = prefix_hash(&wire_request.wire, wire_request.wire.messages.len());
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            self.provider_label(),
            self.profile.clone(),
            ANTHROPIC_PROTOCOL,
            request.model.clone(),
            true,
            request.observation.clone(),
        )
        .with_reasoning_effort(wire_request.reasoning_effort())
        .with_cache_observation(&wire_request)
        .with_known_secret(&auth.api_key);
        let (http_request, claude_context) =
            self.authenticated_request(auth, &wire_request, None, request.observation.as_ref())?;
        observer.emit_started();
        let response = http_request.execute().await.inspect_err(|error| {
            observer.emit_failed(error);
        })?;
        if !response.is_success() {
            let error = response.error().await;
            observer.emit_failed(&error);
            return Err(error);
        }
        let provider_request_id = response.request_id().map(str::to_owned);
        Ok(adapt_sse_stream_with_errors(
            response.bytes_stream(),
            AnthropicInterpreterAdapter {
                inner: AnthropicStreamInterpreter::new(),
                tool_adapter,
                response_prefix,
                claude_context,
                provider_request_id,
            },
            UsageEmitter::new(self.event_bus.clone(), self.provider_label()),
            request.model.clone(),
            observer,
        ))
    }
}

struct AnthropicInterpreterAdapter {
    inner: AnthropicStreamInterpreter,
    tool_adapter: Option<ClaudeToolAdapter>,
    response_prefix: String,
    claude_context: Option<ClaudeRequestContext>,
    provider_request_id: Option<String>,
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
            if self.tool_adapter.is_some() {
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
        if let Some(adapter) = &mut self.tool_adapter {
            adapter.restore_events(&frame, &mut events)?;
        }
        let completion = self.inner.is_done().then(|| {
            if let Some(context) = self.claude_context.take() {
                context.complete(self.provider_request_id.as_deref(), self.inner.message_id());
            }
            self.inner.take_result()
        });
        Ok(FrameInterpretation { events, completion })
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
        Ok(FrameInterpretation::default())
    }

    fn take_message(&mut self) -> Option<Message> {
        let mut message = self.inner.take_message();
        bind_response(&mut message, &self.response_prefix);
        if let Some(adapter) = &self.tool_adapter {
            adapter.restore_message(&mut message);
        }
        Some(message)
    }
}
