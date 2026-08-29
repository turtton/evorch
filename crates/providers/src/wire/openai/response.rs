use crate::error::ProviderError;
use crate::message::{ChatResponse, ContentBlock, FinishReason, Message, Role, Usage};

use super::response_types::{WireChatResponse, WireUsage};

/// OpenAI 非ストリーミング応答を canonical 応答へ変換します。
///
/// OpenAI Chat Completions は cache-write 使用量を返さないため、
/// [`Usage::cache_write_tokens`] は常に 0 です。
///
/// # Errors
/// 最初の choice、message、usage、finish reason が欠ける場合、または tool call の
/// `arguments` が JSON でない場合に [`ProviderError::InvalidJson`] を返します。
pub fn from_wire_response(wire: &WireChatResponse) -> Result<ChatResponse, ProviderError> {
    let choice = wire
        .choices
        .first()
        .ok_or_else(|| invalid("choices が空です"))?;
    let message = choice
        .message
        .as_ref()
        .ok_or_else(|| invalid("choices[0].message がありません"))?;
    match message.role.as_deref() {
        Some("assistant") => {}
        Some(role) => {
            return Err(invalid(format!(
                "choices[0].message.role が不正です: {role}"
            )));
        }
        None => return Err(invalid("choices[0].message.role がありません")),
    }
    let finish_reason = choice
        .finish_reason
        .as_deref()
        .ok_or_else(|| invalid("choices[0].finish_reason がありません"))?;
    let usage = wire
        .usage
        .as_ref()
        .ok_or_else(|| invalid("usage がありません"))?;
    let mut content = message
        .content
        .iter()
        .filter(|text| !text.is_empty())
        .map(|text| ContentBlock::Text { text: text.clone() })
        .collect::<Vec<_>>();
    content.extend(
        message
            .tool_calls
            .iter()
            .map(|call| {
                let input = serde_json::from_str(&call.function.arguments).map_err(|error| {
                    invalid(format!(
                        "tool call '{}' の arguments が不正です: {error}",
                        call.id
                    ))
                })?;
                Ok(ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    input,
                })
            })
            .collect::<Result<Vec<_>, ProviderError>>()?,
    );
    Ok(ChatResponse {
        message: Message {
            role: Role::Assistant,
            content,
        },
        usage: to_usage(usage),
        finish_reason: to_finish_reason(finish_reason),
    })
}

/// OpenAI の finish reason を canonical 値へ変換します。
#[must_use]
pub fn to_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolUse,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_string()),
    }
}

pub(super) fn to_usage(usage: &WireUsage) -> Usage {
    Usage {
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
        cache_read_tokens: usage
            .prompt_tokens_details
            .and_then(|details| details.cached_tokens)
            .unwrap_or_default(),
        cache_write_tokens: 0,
    }
}

fn invalid(detail: impl Into<String>) -> ProviderError {
    ProviderError::InvalidJson {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Given: choices が空の wire response / When: canonical 変換 / Then: 構造欠落を InvalidJson として返す
    #[test]
    fn response_without_choices_returns_invalid_json() {
        let wire: WireChatResponse = serde_json::from_value(json!({
            "choices": [],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0}
        }))
        .expect("fixture must deserialize");

        let error = from_wire_response(&wire).expect_err("empty choices must fail");

        assert!(matches!(error, ProviderError::InvalidJson { .. }));
    }

    // Given: JSON でない tool arguments / When: canonical 変換 / Then: InvalidJson を返す
    #[test]
    fn response_with_invalid_tool_arguments_returns_invalid_json() {
        let wire: WireChatResponse = serde_json::from_value(json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "weather", "arguments": "{bad"}
                }]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }))
        .expect("fixture must deserialize");

        let error = from_wire_response(&wire).expect_err("invalid arguments must fail");

        assert!(matches!(error, ProviderError::InvalidJson { .. }));
    }
}
