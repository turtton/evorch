use std::time::Duration;

use serde_json::Value;

use crate::{HostedWebSearchError, ProviderError};

pub(super) fn invalid(detail: &str) -> ProviderError {
    ProviderError::InvalidSse {
        detail: detail.to_owned(),
    }
}

/// Never return a server body, token-store error or endpoint URL to tools/logs.
pub(super) fn sanitize(error: ProviderError) -> ProviderError {
    match error {
        ProviderError::Http { status, .. } => ProviderError::Http {
            status,
            body: "Codex web search request was rejected".into(),
        },
        ProviderError::RateLimited { .. } | ProviderError::Timeout => error,
        ProviderError::InvalidSse { .. } => invalid("Codex web search SSE is invalid"),
        ProviderError::InvalidJson { .. } => ProviderError::InvalidJson {
            detail: "Codex web search response JSON is invalid".into(),
        },
        ProviderError::Request(_) => {
            ProviderError::Request("Codex web search session or request failed".into())
        }
        ProviderError::Transport { .. } => ProviderError::Transport {
            message: "Codex web search transport failed".into(),
        },
        ProviderError::RetriesExhausted { attempts, last } => ProviderError::RetriesExhausted {
            attempts,
            last: Box::new(sanitize(*last)),
        },
    }
}

pub(super) async fn http_error(response: reqwest::Response) -> HostedWebSearchError {
    let status = response.status().as_u16();
    if status == 429 {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return ProviderError::RateLimited { retry_after }.into();
    }
    if status == 400
        && let Ok(body) = response.json::<Value>().await
        && explicit_unsupported(body.get("error").unwrap_or(&body))
    {
        return HostedWebSearchError::Unsupported;
    }
    sanitize(ProviderError::Http {
        status,
        body: String::new(),
    })
    .into()
}

/// Inspect only the structured error, not an echoed request or arbitrary text.
pub(super) fn explicit_unsupported(error: &Value) -> bool {
    if ["status", "status_code"].iter().any(|field| {
        error
            .get(field)
            .and_then(Value::as_u64)
            .is_some_and(|status| status != 400)
    }) {
        return false;
    }
    if let Some(kind) = error.get("type").and_then(Value::as_str)
        && !matches!(
            kind,
            "error" | "invalid_request_error" | "unsupported_tool" | "unknown_tool"
        )
    {
        return false;
    }
    let Some(message) = error.get("message").and_then(Value::as_str) else {
        return false;
    };
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    if !matches!(
        code,
        "" | "invalid_request_error"
            | "unsupported_tool"
            | "unknown_tool"
            | "unsupported_tool_type"
            | "tool_not_supported"
            | "unsupported_value"
    ) {
        return false;
    }
    if let Some(param) = error.get("param").and_then(Value::as_str)
        && !matches!(
            param,
            "tools" | "tools[0]" | "tools[0].type" | "tools.0.type"
        )
    {
        return false;
    }
    let message = message.to_ascii_lowercase().replace(['\'', '"', '`'], "");
    // Require an explicit tool rejection. Generic unsupported/model/auth errors
    // merely mentioning web_search must remain visible errors.
    [
        "web_search is not supported",
        "web_search tool is not supported",
        "web_search is unsupported",
        "web_search tool is unsupported",
        "does not support web_search",
        "does not support the web_search tool",
        "unsupported tool: web_search",
        "unsupported tool type: web_search",
        "unknown tool: web_search",
        "unknown tool type: web_search",
        "unrecognized tool: web_search",
        "unrecognized tool type: web_search",
    ]
    .iter()
    .any(|phrase| {
        message.match_indices(phrase).any(|(start, matched)| {
            let boundary = |character: char| !character.is_ascii_alphanumeric() && character != '_';
            message[..start].chars().next_back().is_none_or(boundary)
                && message[start + matched.len()..]
                    .chars()
                    .next()
                    .is_none_or(boundary)
        })
    })
}
