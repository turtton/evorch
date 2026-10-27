use std::collections::HashSet;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use super::super::transport::HttpResponse;
use super::errors::{invalid, sanitize};
use crate::{
    HostedWebSearchCitation, HostedWebSearchError, HostedWebSearchResponse, ProviderError, Usage,
};

pub(super) async fn read_response(response: HttpResponse) -> Result<Value, ProviderError> {
    read_json(response.bytes_stream()).await
}

async fn read_json(
    mut stream: impl Stream<Item = Result<Bytes, ProviderError>> + Unpin,
) -> Result<Value, ProviderError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(sanitize)?;
        if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
            return Err(invalid(
                "Anthropic web search response exceeds the size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| invalid("Anthropic web search response JSON is invalid"))
}

pub(super) struct SearchResults {
    pending_calls: HashSet<String>,
    completed_calls: HashSet<String>,
    text: Vec<String>,
    citations: Vec<HostedWebSearchCitation>,
    sources: Vec<HostedWebSearchCitation>,
    max_results: usize,
    usage: Usage,
}

impl SearchResults {
    pub(super) fn new(max_results: Option<u32>) -> Self {
        Self {
            pending_calls: HashSet::new(),
            completed_calls: HashSet::new(),
            text: Vec::new(),
            citations: Vec::new(),
            sources: Vec::new(),
            max_results: max_results.unwrap_or(10).min(10) as usize,
            usage: Usage::default(),
        }
    }

    pub(super) fn add_usage(&mut self, usage: Usage) {
        self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
        self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
        self.usage.cache_read_tokens = self
            .usage
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        self.usage.cache_write_tokens = self
            .usage
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
    }

    pub(super) fn accept(&mut self, body: &Value) -> Result<(), ProviderError> {
        for block in body["content"]
            .as_array()
            .ok_or_else(|| invalid("Anthropic web search content is invalid"))?
        {
            match block["type"].as_str() {
                Some("server_tool_use") => {
                    let id = nonempty(&block["id"])?;
                    if block["name"] != "web_search"
                        || !block["input"].is_object()
                        || self.completed_calls.contains(id)
                        || !self.pending_calls.insert(id.to_owned())
                    {
                        return Err(invalid("Anthropic web search call is invalid"));
                    }
                }
                Some("web_search_tool_result") => {
                    let id = nonempty(&block["tool_use_id"])?;
                    if !self.pending_calls.remove(id) {
                        return Err(invalid("Anthropic web search result has no matching call"));
                    }
                    if block["content"]["type"] == "web_search_tool_result_error" {
                        return Err(tool_error(&block["content"]));
                    }
                    for source in block["content"]
                        .as_array()
                        .ok_or_else(|| invalid("Anthropic web search results are invalid"))?
                    {
                        if source["type"] != "web_search_result" {
                            return Err(invalid("Anthropic web search source is invalid"));
                        }
                        self.sources.push(citation(source)?);
                    }
                    self.completed_calls.insert(id.to_owned());
                }
                Some("text") => {
                    self.text.push(
                        block["text"]
                            .as_str()
                            .ok_or_else(|| invalid("Anthropic web search text is invalid"))?
                            .to_owned(),
                    );
                    if let Some(citations) = block.get("citations").filter(|value| !value.is_null())
                    {
                        for item in citations
                            .as_array()
                            .ok_or_else(|| invalid("Anthropic web search citations are invalid"))?
                        {
                            if item["type"] != "web_search_result_location" {
                                return Err(invalid("Anthropic web search citation is invalid"));
                            }
                            self.citations.push(citation(item)?);
                        }
                    }
                }
                Some("thinking" | "redacted_thinking") => {}
                _ => return Err(invalid("Anthropic web search content block is invalid")),
            }
        }
        Ok(())
    }

    pub(super) fn finish(
        &self,
        response_id: Option<String>,
    ) -> Result<HostedWebSearchResponse, HostedWebSearchError> {
        if self.completed_calls.is_empty() || !self.pending_calls.is_empty() {
            return Err(invalid("Anthropic response did not complete web search").into());
        }
        let mut citations: Vec<HostedWebSearchCitation> = Vec::new();
        for source in self.citations.iter().chain(&self.sources) {
            if let Some(existing) = citations.iter_mut().find(|item| item.url == source.url) {
                if existing.title == existing.url && source.title != source.url {
                    existing.title.clone_from(&source.title);
                }
            } else if citations.len() < self.max_results {
                citations.push(source.clone());
            }
        }
        Ok(HostedWebSearchResponse {
            response_id,
            text: self.text.join("\n\n"),
            citations,
            usage: self.usage,
        })
    }
}

fn nonempty(value: &Value) -> Result<&str, ProviderError> {
    value
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("Anthropic web search call identity is invalid"))
}

fn citation(value: &Value) -> Result<HostedWebSearchCitation, ProviderError> {
    let url = value["url"]
        .as_str()
        .ok_or_else(|| invalid("Anthropic web search source URL is invalid"))?;
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| invalid("Anthropic web search source URL is invalid"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(invalid("Anthropic web search source URL is invalid"));
    }
    Ok(HostedWebSearchCitation {
        url: url.to_owned(),
        title: value["title"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(url)
            .to_owned(),
    })
}

fn tool_error(error: &Value) -> ProviderError {
    match error["error_code"].as_str() {
        Some("too_many_requests") => ProviderError::RateLimited { retry_after: None },
        Some("unavailable") => ProviderError::Http {
            status: 503,
            body: "Anthropic web search is temporarily unavailable".into(),
        },
        Some("max_uses_exceeded") => {
            ProviderError::Request("Anthropic web search exceeded its tool-use limit".into())
        }
        _ => ProviderError::Request("Anthropic web search tool failed".into()),
    }
}

pub(super) fn completed_usage(body: &Value) -> Result<(Usage, &str), ProviderError> {
    if body["type"] != "message" || body["role"] != "assistant" {
        return Err(invalid("Anthropic web search response is not a message"));
    }
    let stop = body["stop_reason"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("Anthropic web search response is incomplete"))?;
    #[derive(Deserialize)]
    struct ResponseUsage {
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        cache_read_input_tokens: u64,
        #[serde(default)]
        cache_creation_input_tokens: u64,
    }
    let usage: ResponseUsage = serde_json::from_value(body["usage"].clone())
        .map_err(|_| invalid("Anthropic web search usage is invalid"))?;
    Ok((
        Usage {
            context_tokens: None,
            input_tokens: usage
                .input_tokens
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens),
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_input_tokens,
            cache_write_tokens: usage.cache_creation_input_tokens,
            reasoning_tokens: None,
        },
        stop,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn response_body_errors_retain_transient_class_after_successful_headers() {
        for (error, expected) in [
            (ProviderError::Timeout, ProviderError::Timeout),
            (
                ProviderError::Transport {
                    message: "https://private-endpoint/token".into(),
                },
                ProviderError::Transport {
                    message: "Anthropic web search transport failed".into(),
                },
            ),
        ] {
            let stream = futures_util::stream::iter([
                Ok(Bytes::from_static(b"{\"type\":\"message\",")),
                Err(error),
            ]);
            assert_eq!(read_json(stream).await.unwrap_err(), expected);
        }
    }
}
