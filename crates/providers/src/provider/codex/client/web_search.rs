//! A search-only Responses exchange. Ordinary chat wire and parsing stay independent.

mod errors;
mod stream;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{CodexClient, PROTOCOL, PROVIDER_LABEL};
use crate::http::{UsageEmitter, map_request_error};
use crate::observe::AttemptObserver;
use crate::sse::SseParser;
use crate::{
    FinishReason, HostedWebSearch, HostedWebSearchError, HostedWebSearchRequest,
    HostedWebSearchResponse, ProviderAuth,
};
use errors::{http_error, invalid, sanitize};
use stream::SearchStream;

#[async_trait]
impl HostedWebSearch for CodexClient {
    async fn search(
        &self,
        _auth: &ProviderAuth,
        request: &HostedWebSearchRequest,
    ) -> Result<HostedWebSearchResponse, HostedWebSearchError> {
        // Search must not replace the conversation's cache diagnostic baseline.
        let mut observer = AttemptObserver::new(
            self.event_bus.clone(),
            PROVIDER_LABEL,
            self.profile.clone(),
            PROTOCOL,
            request.model.clone(),
            true,
            request.observation.clone(),
        );
        let emitter = UsageEmitter::new(self.event_bus.clone(), PROVIDER_LABEL);
        let result = async {
            let response = self
                .post_response(&wire_request(request), false, &mut observer)
                .await
                .map_err(sanitize)?;
            if !response.status().is_success() {
                return Err(http_error(response).await);
            }
            let mut stream = response.bytes_stream();
            let mut parser = SseParser::new();
            let mut search = SearchStream::new(request.max_results);
            let mut on_usage = |usage| {
                // No await between observing usage and recording it: cancellation
                // or malformed final content cannot discard a completed call's cost.
                if let Some(sink) = &request.usage_sink {
                    sink(usage);
                }
                emitter.emit_usage(&request.model, &usage);
                observer.emit_completed(&usage, FinishReason::Stop);
            };
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(map_request_error).map_err(sanitize)?;
                // Dispatch complete frames before looking at later bytes. A corrupt
                // trailing frame in the same HTTP chunk cannot hide completed usage.
                for line in chunk.split_inclusive(|byte| *byte == b'\n') {
                    for frame in parser
                        .feed(line)
                        .map_err(|_| invalid("Codex web search SSE is invalid"))?
                    {
                        if let Some(result) = search.interpret(frame, &mut on_usage)? {
                            return Ok(result);
                        }
                    }
                }
            }
            for frame in parser
                .finish()
                .map_err(|_| invalid("Codex web search SSE ending is invalid"))?
            {
                if let Some(result) = search.interpret(frame, &mut on_usage)? {
                    return Ok(result);
                }
            }
            Err(invalid("Codex web search ended without completion").into())
        }
        .await;
        if let Err(error) = &result {
            match error {
                HostedWebSearchError::Provider(error) => observer.emit_failed(error),
                HostedWebSearchError::Unsupported => observer
                    .emit_failed(&invalid("Codex backend does not support hosted web_search")),
            }
        }
        result
    }
}

fn wire_request(request: &HostedWebSearchRequest) -> Value {
    // Tool shape follows Codex upstream 0e152060 (tool_spec.rs and
    // core/tests/suite/web_search.rs). This dedicated request must search, unlike
    // ordinary chat: with web_search as its only tool, required selects that tool.
    // Public API tool choice does not prove subscription-backend support; only a
    // completed web_search_call can satisfy the decoder's success contract.
    json!({
        "model": request.model,
        "instructions": "Use the web_search tool to search the web for the user's query. Return a concise answer grounded in retrieved sources with URL citations. You must perform web search before answering.",
        "input": [{
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": request.query}]
        }],
        "tools": [{"type": "web_search", "external_web_access": true}],
        "tool_choice": "required",
        "parallel_tool_calls": true,
        "store": false,
        "stream": true,
        "reasoning": {
            "effort": request.reasoning_effort.as_deref().unwrap_or("medium"),
            "summary": "auto"
        },
        "include": ["web_search_call.action.sources"]
    })
}
