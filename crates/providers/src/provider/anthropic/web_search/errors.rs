use serde_json::Value;

use super::super::transport::HttpResponse;
use crate::{HostedWebSearchError, ProviderError};

pub(super) fn invalid(detail: &str) -> ProviderError {
    ProviderError::InvalidJson {
        detail: detail.to_owned(),
    }
}

pub(crate) fn sanitize(error: ProviderError) -> ProviderError {
    match error {
        ProviderError::Http { status, .. } => ProviderError::Http {
            status,
            body: "Anthropic web search request was rejected".into(),
        },
        ProviderError::RateLimited { .. } | ProviderError::Timeout => error,
        ProviderError::InvalidJson { .. } | ProviderError::InvalidSse { .. } => {
            invalid("Anthropic web search response is invalid")
        }
        ProviderError::Request(_) => {
            ProviderError::Request("Anthropic web search session or request failed".into())
        }
        ProviderError::Transport { .. } => ProviderError::Transport {
            message: "Anthropic web search transport failed".into(),
        },
        ProviderError::RetriesExhausted { attempts, last } => ProviderError::RetriesExhausted {
            attempts,
            last: Box::new(sanitize(*last)),
        },
    }
}

pub(super) async fn http_error(response: HttpResponse) -> HostedWebSearchError {
    let status = response.status();
    if status == 429 {
        return ProviderError::RateLimited {
            retry_after: response.retry_after(),
        }
        .into();
    }
    if status == 400
        && let Ok(body) = response.bounded_json().await
        && explicit_unsupported(&body["error"])
    {
        return HostedWebSearchError::Unsupported;
    }
    sanitize(ProviderError::Http {
        status,
        body: String::new(),
    })
    .into()
}

fn explicit_unsupported(error: &Value) -> bool {
    if !matches!(
        error["type"].as_str(),
        Some("invalid_request_error" | "unsupported_tool" | "unknown_tool")
    ) || error.get("code").is_some_and(|code| {
        !matches!(
            code.as_str(),
            Some(
                "unsupported_tool"
                    | "unknown_tool"
                    | "unsupported_tool_type"
                    | "tool_not_supported"
            )
        )
    }) || error.get("param").is_some_and(|param| {
        !matches!(
            param.as_str(),
            Some("tools" | "tools.0.type" | "tools[0].type")
        )
    }) {
        return false;
    }
    let Some(message) = error["message"].as_str() else {
        return false;
    };
    let message = message.to_ascii_lowercase().replace(['\'', '"', '`'], "");
    ["web_search", "web_search_20250305"].iter().any(|tool| {
        [
            format!("{tool} is not supported"),
            format!("{tool} tool is not supported"),
            format!("{tool} is unsupported"),
            format!("does not support {tool}"),
            format!("unsupported tool: {tool}"),
            format!("unsupported tool type: {tool}"),
            format!("unknown tool: {tool}"),
            format!("unknown tool type: {tool}"),
        ]
        .iter()
        .any(|phrase| {
            message
                .match_indices(phrase.as_str())
                .any(|(start, matched)| {
                    let boundary = |c: char| !c.is_ascii_alphanumeric() && c != '_';
                    message[..start].chars().next_back().is_none_or(boundary)
                        && message[start + matched.len()..]
                            .chars()
                            .next()
                            .is_none_or(boundary)
                })
        })
    })
}
