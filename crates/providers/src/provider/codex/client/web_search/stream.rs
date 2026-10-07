use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use super::errors::{explicit_unsupported, invalid};
use crate::sse::SseFrame;
use crate::{
    HostedWebSearchCitation, HostedWebSearchError, HostedWebSearchResponse, ProviderError, Usage,
};

/// Retain finalized items, not text deltas: completed output and done events
/// often repeat identical messages and citations.
pub(super) struct SearchStream {
    items: BTreeMap<u64, Value>,
    response_id: Option<String>,
    max_results: usize,
}

impl SearchStream {
    pub(super) fn new(max_results: Option<u32>) -> Self {
        Self {
            items: BTreeMap::new(),
            response_id: None,
            max_results: max_results.unwrap_or(10).min(10) as usize,
        }
    }

    pub(super) fn interpret(
        &mut self,
        frame: SseFrame,
        on_usage: &mut impl FnMut(Usage),
    ) -> Result<Option<HostedWebSearchResponse>, HostedWebSearchError> {
        if frame.data.trim() == "[DONE]" {
            return Err(invalid("Codex web search ended without completion").into());
        }
        let value: Value = serde_json::from_str(&frame.data)
            .map_err(|_| invalid("Codex web search event JSON is invalid"))?;
        let event = frame
            .event
            .as_deref()
            .or_else(|| value["type"].as_str())
            .ok_or_else(|| invalid("Codex web search event type is missing"))?;
        match event {
            "response.created" => {
                self.response_id = value["response"]["id"].as_str().map(str::to_owned);
            }
            "response.output_item.done" => {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or_else(|| invalid("Codex web search output index is invalid"))?;
                self.items.insert(index, value["item"].clone());
            }
            "response.completed" => {
                let response = &value["response"];
                // Usage is independent of the content decoder and must survive
                // missing calls, malformed output, or a downstream formatting error.
                let usage: ResponseUsage = serde_json::from_value(response["usage"].clone())
                    .map_err(|_| invalid("Codex web search usage is invalid"))?;
                let usage = Usage::from(usage);
                on_usage(usage);
                if let Some(status) = response.get("status")
                    && status != "completed"
                {
                    return Err(invalid("Codex web search response is not completed").into());
                }
                // Some subscription responses omit streamed items from the
                // terminal output. Preserve all done items, replacing matching
                // IDs with their final snapshots instead of losing real calls.
                let mut items: Vec<_> = self.items.values().collect();
                if let Some(output) = response.get("output") {
                    for item in output
                        .as_array()
                        .ok_or_else(|| invalid("Codex web search output is invalid"))?
                    {
                        let matching = items.iter_mut().find(|existing| {
                            item["id"].as_str().is_some_and(|id| existing["id"] == id)
                                || **existing == item
                        });
                        if let Some(existing) = matching {
                            *existing = item;
                        } else {
                            items.push(item);
                        }
                    }
                }
                let (text, citations) = decode_output(items, self.max_results)?;
                return Ok(Some(HostedWebSearchResponse {
                    response_id: response["id"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| self.response_id.clone()),
                    text,
                    citations,
                    usage,
                }));
            }
            "response.failed" | "error" => {
                let error = if event == "response.failed" {
                    &value["response"]["error"]
                } else {
                    value.get("error").unwrap_or(&value)
                };
                let status = value
                    .get("status")
                    .or_else(|| value.get("status_code"))
                    .and_then(Value::as_u64);
                if status.is_none_or(|status| status == 400) && explicit_unsupported(error) {
                    return Err(HostedWebSearchError::Unsupported);
                }
                return Err(invalid("Codex web search request failed").into());
            }
            "response.incomplete" => {
                return Err(invalid("Codex web search response is incomplete").into());
            }
            // created/added/delta and reasoning events are not completed results.
            _ => {}
        }
        Ok(None)
    }
}

fn decode_output(
    items: Vec<&Value>,
    max_results: usize,
) -> Result<(String, Vec<HostedWebSearchCitation>), ProviderError> {
    let mut text = Vec::new();
    let mut citations = Vec::new();
    let mut sources = Vec::new();
    let mut searched = false;
    for item in items {
        match item["type"].as_str() {
            Some("web_search_call") => {
                if item["status"] != "completed" || item["id"].as_str().is_none_or(str::is_empty) {
                    return Err(invalid("Codex web search call is not completed"));
                }
                searched = true;
                if let Some(value) = item["action"].get("sources") {
                    for source in value
                        .as_array()
                        .ok_or_else(|| invalid("Codex web search sources are invalid"))?
                    {
                        // Non-URL sources (e.g. provider feeds) cannot be link citations.
                        if source.get("url").is_some() {
                            sources.push(citation(source)?);
                        }
                    }
                }
            }
            Some("message") => {
                for content in item["content"]
                    .as_array()
                    .ok_or_else(|| invalid("Codex web search message is invalid"))?
                {
                    match content["type"].as_str() {
                        Some("output_text") => {
                            text.push(
                                content["text"]
                                    .as_str()
                                    .ok_or_else(|| invalid("Codex web search text is invalid"))?,
                            );
                            if let Some(annotations) = content.get("annotations") {
                                for annotation in annotations.as_array().ok_or_else(|| {
                                    invalid("Codex web search annotations are invalid")
                                })? {
                                    if annotation["type"] == "url_citation" {
                                        citations.push(citation(annotation)?);
                                    }
                                }
                            }
                        }
                        Some("refusal") => {
                            return Err(invalid("Codex web search was refused"));
                        }
                        _ => {}
                    }
                }
            }
            Some("reasoning") => {}
            // Hosted search cannot silently succeed with an unrelated tool call.
            _ => return Err(invalid("Codex web search output item is invalid")),
        }
    }
    if !searched {
        return Err(invalid("Codex response did not perform web search"));
    }
    let mut deduplicated: Vec<HostedWebSearchCitation> = Vec::new();
    // Prefer final-message citation titles/order, then add consulted URLs.
    for source in citations.into_iter().chain(sources) {
        if let Some(existing) = deduplicated.iter_mut().find(|item| item.url == source.url) {
            if existing.title == existing.url && source.title != source.url {
                existing.title = source.title;
            }
        } else if deduplicated.len() < max_results {
            deduplicated.push(source);
        }
    }
    Ok((text.join("\n\n"), deduplicated))
}

fn citation(value: &Value) -> Result<HostedWebSearchCitation, ProviderError> {
    let url = value["url"]
        .as_str()
        .filter(|url| !url.is_empty())
        .ok_or_else(|| invalid("Codex web search citation URL is invalid"))?;
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| invalid("Codex web search citation URL is invalid"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(invalid("Codex web search citation URL is invalid"));
    }
    Ok(HostedWebSearchCitation {
        url: url.to_owned(),
        title: value["title"]
            .as_str()
            .filter(|title| !title.is_empty())
            .unwrap_or(url)
            .to_owned(),
    })
}

#[derive(Deserialize)]
struct ResponseUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: InputTokenDetails,
    #[serde(default)]
    output_tokens_details: OutputTokenDetails,
}

#[derive(Deserialize, Default)]
struct InputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize, Default)]
struct OutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl From<ResponseUsage> for Usage {
    fn from(value: ResponseUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cache_read_tokens: value.input_tokens_details.cached_tokens,
            cache_write_tokens: 0,
            reasoning_tokens: value.output_tokens_details.reasoning_tokens,
        }
    }
}
