//! Isolated Messages exchanges using Anthropic's server-executed search tool.

mod errors;
mod response;

use async_trait::async_trait;
use event_bus::RequestPurpose;
use serde_json::json;

use super::{ANTHROPIC_PROTOCOL, AnthropicClient, MessagesRequest};
use crate::http::UsageEmitter;
use crate::observe::AttemptObserver;
use crate::provider::claude;
use crate::wire::anthropic::{WireContentBlock, WireMessage, WireMessagesRequest, WireRole};
use crate::{
    FinishReason, HostedWebSearch, HostedWebSearchError, HostedWebSearchRequest,
    HostedWebSearchResponse, ObservationContext, ProviderAuth, ProviderError,
};
pub(crate) use errors::sanitize;
use errors::{http_error, invalid};
use response::{SearchResults, completed_usage, read_response};

const MAX_CONTINUATIONS: usize = 2;
const MAX_TOKENS: u64 = 8192;
const SEARCH_INSTRUCTIONS: &str = "Use the web_search tool to search the web for the user's query. You must perform web search before answering. Return a concise answer grounded in retrieved sources with URL citations.";

#[async_trait]
impl HostedWebSearch for AnthropicClient {
    fn provider_name(&self) -> &'static str {
        self.provider_label()
    }

    async fn search(
        &self,
        auth: &ProviderAuth,
        request: &HostedWebSearchRequest,
    ) -> Result<HostedWebSearchResponse, HostedWebSearchError> {
        let observation = ObservationContext {
            attempts: None,
            purpose: RequestPurpose::WebSearch,
            ..request.observation.clone().unwrap_or_default()
        };
        let mut wire = self.search_request(request).map_err(sanitize)?;
        let tools = [json!({"type":"web_search_20250305", "name":"web_search", "max_uses":8})];
        let mut results = SearchResults::new(request.max_results);
        let emitter = UsageEmitter::new(self.event_bus.clone(), self.provider_label());
        for continuation in 0..=MAX_CONTINUATIONS {
            // Auxiliary search must not advance the conversation's cache baseline.
            let mut observer = AttemptObserver::new(
                self.event_bus.clone(),
                self.provider_label(),
                self.profile.clone(),
                ANTHROPIC_PROTOCOL,
                request.model.clone(),
                false,
                Some(observation.clone()),
            )
            .with_known_secret(&auth.api_key);
            let attempt = async {
                let (prepared, context) = self
                    .authenticated_request_with_tools(
                        auth,
                        &wire,
                        Some(self.timeout),
                        Some(&observation),
                        Some(&tools),
                    )
                    .map_err(sanitize)?;
                observer.emit_started();
                let response = prepared.execute().await.map_err(sanitize)?;
                if !response.is_success() {
                    return Err(http_error(response).await);
                }
                let request_id = response.request_id().map(str::to_owned);
                let body = read_response(response).await?;
                let (usage, stop_reason) = completed_usage(&body)?;
                // Completed cost is recorded before validating search output or awaiting
                // a continuation, including when the server tool returned an error.
                if let Some(sink) = &request.usage_sink {
                    sink(usage);
                }
                emitter.emit_usage(&request.model, &usage);
                observer.emit_completed(
                    &usage,
                    if stop_reason == "end_turn" {
                        FinishReason::Stop
                    } else {
                        FinishReason::Other(stop_reason.to_owned())
                    },
                );
                results.add_usage(usage);
                results.accept(&body)?;
                let outcome = match stop_reason {
                    "end_turn" => results
                        .finish(body["id"].as_str().map(str::to_owned))
                        .map(Some),
                    "pause_turn" if continuation < MAX_CONTINUATIONS => {
                        // Keep every server block, including encrypted results and signed
                        // thinking, exactly as returned within this search-only history.
                        wire.wire.messages.push(WireMessage {
                            role: WireRole::Assistant,
                            content: body["content"]
                                .as_array()
                                .ok_or_else(|| invalid("Anthropic web search content is invalid"))?
                                .iter()
                                .cloned()
                                .map(|block| WireContentBlock::Raw {
                                    block,
                                    prefix_hash: None,
                                })
                                .collect(),
                        });
                        Ok(None)
                    }
                    "pause_turn" => {
                        Err(invalid("Anthropic web search continuation limit reached").into())
                    }
                    _ => Err(invalid("Anthropic web search did not finish normally").into()),
                }?;
                if let Some(context) = context {
                    context.complete(request_id.as_deref(), body["id"].as_str());
                }
                Ok(outcome)
            }
            .await;
            match attempt {
                Ok(Some(response)) => return Ok(response),
                Ok(None) => {}
                Err(error) => {
                    match &error {
                        HostedWebSearchError::Provider(error) => observer.emit_failed(error),
                        HostedWebSearchError::Unsupported => {
                            observer.emit_failed(&ProviderError::Request(
                                "Anthropic backend does not support hosted web_search".into(),
                            ))
                        }
                    }
                    return Err(error);
                }
            }
        }
        unreachable!("the last paused response fails the continuation bound")
    }
}

impl AnthropicClient {
    fn search_request(
        &self,
        request: &HostedWebSearchRequest,
    ) -> Result<MessagesRequest, ProviderError> {
        if request.query.trim().is_empty() {
            return Err(ProviderError::Request("web search query is empty".into()));
        }
        let text = |text: &str| WireContentBlock::Text {
            text: text.to_owned(),
            cache_control: None,
        };
        let mut system = vec![text(SEARCH_INSTRUCTIONS)];
        if self.claude_oauth {
            system.insert(0, text(claude::CLAUDE_SYSTEM_IDENTITY));
        }
        Ok(MessagesRequest {
            wire: WireMessagesRequest {
                model: request.model.clone(),
                max_tokens: MAX_TOKENS,
                system: Some(system),
                messages: vec![WireMessage {
                    role: WireRole::User,
                    content: vec![text(&request.query)],
                }],
                tools: Vec::new(),
                temperature: None,
                stream: false,
            },
            metadata: self.claude_oauth.then(|| {
                claude::request_metadata(
                    &self
                        .claude_installation_id
                        .unwrap_or_else(claude::process_installation_id),
                    self.claude_account_id.as_deref(),
                    &self.claude_session_id,
                    request.observation.as_ref().map(|o| o.run_id.as_str()),
                )
            }),
            reasoning: self.reasoning_controls(
                &request.model,
                MAX_TOKENS,
                request.reasoning_effort.as_deref(),
            )?,
        })
    }
}
