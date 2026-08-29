use crate::error::ProviderError;
use crate::message::{ChatRequest, ContentBlock, Message, Role, ToolResultContent, ToolSpec};

use super::types::{
    WireChatRequest, WireContent, WireFunction, WireFunctionDefinition, WireMessage,
    WireStreamOptions, WireTextPart, WireTool, WireToolCall,
};

/// canonical リクエストを OpenAI Chat Completions リクエストへ変換します。
///
/// Chat Completions に reasoning 入力フィールドはないため、
/// [`ContentBlock::Reasoning`] は送信時に失われます。tool result の `is_error` も
/// 対応フィールドがないため失われます。`stream` が真なら usage-only 最終 chunk を
/// 受け取るため `stream_options.include_usage` も有効にします。
#[must_use]
pub fn to_wire_request(request: &ChatRequest, stream: bool) -> WireChatRequest {
    WireChatRequest {
        model: request.model.clone(),
        messages: request.messages.iter().flat_map(to_wire_messages).collect(),
        tools: request.tools.iter().map(to_wire_tool).collect(),
        temperature: request.temperature,
        max_tokens: request.max_tokens,
        stream,
        stream_options: stream.then_some(WireStreamOptions {
            include_usage: true,
        }),
    }
}

/// OpenAI wire メッセージ列を canonical メッセージ列へ復元します。
///
/// 連続する `tool` メッセージは、canonical の単一 user メッセージ内にある
/// [`ContentBlock::ToolResult`] 列へまとめます。
///
/// # Errors
/// text part 以外の content part が含まれる場合に [`ProviderError::InvalidJson`] を返します。
pub fn from_wire_messages(messages: &[WireMessage]) -> Result<Vec<Message>, ProviderError> {
    let mut canonical = Vec::new();
    for message in messages {
        match message {
            WireMessage::System { content } => canonical.push(Message {
                role: Role::System,
                content: content_blocks(content)?,
            }),
            WireMessage::User { content } => canonical.push(Message {
                role: Role::User,
                content: content_blocks(content)?,
            }),
            WireMessage::Assistant {
                content: wire_content,
                tool_calls,
            } => {
                let mut content = wire_content
                    .as_ref()
                    .map(content_blocks)
                    .transpose()?
                    .unwrap_or_default();
                content.extend(
                    tool_calls
                        .iter()
                        .map(|call| {
                            let input = serde_json::from_str(&call.function.arguments).map_err(
                                |error| ProviderError::InvalidJson {
                                    detail: format!(
                                        "tool call '{}' の arguments が不正です: {error}",
                                        call.id
                                    ),
                                },
                            )?;
                            Ok(ContentBlock::ToolUse {
                                id: call.id.clone(),
                                name: call.function.name.clone(),
                                input,
                            })
                        })
                        .collect::<Result<Vec<_>, ProviderError>>()?,
                );
                canonical.push(Message {
                    role: Role::Assistant,
                    content,
                });
            }
            WireMessage::Tool {
                content: wire_content,
                tool_call_id,
            } => {
                let result = ContentBlock::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    content: tool_result_content(wire_content)?,
                    is_error: false,
                };
                match canonical.last_mut() {
                    Some(Message {
                        role: Role::User,
                        content,
                    }) if content
                        .iter()
                        .all(|block| matches!(block, ContentBlock::ToolResult { .. })) =>
                    {
                        content.push(result);
                    }
                    Some(_) | None => canonical.push(Message {
                        role: Role::User,
                        content: vec![result],
                    }),
                }
            }
        }
    }
    Ok(canonical)
}

fn to_wire_messages(message: &Message) -> Vec<WireMessage> {
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect::<String>();
    let tool_calls = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(WireToolCall {
                id: id.clone(),
                kind: "function".to_string(),
                function: WireFunction {
                    name: name.clone(),
                    arguments: input.to_string(),
                },
            }),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect::<Vec<_>>();
    let tool_results = message.content.iter().filter_map(|block| match block {
        ContentBlock::ToolResult {
            tool_call_id,
            content,
            is_error: _,
        } => Some(WireMessage::Tool {
            content: result_wire_content(content),
            tool_call_id: tool_call_id.clone(),
        }),
        ContentBlock::Text { .. }
        | ContentBlock::Reasoning { .. }
        | ContentBlock::ToolUse { .. } => None,
    });
    let primary = match message.role {
        Role::System => (!text.is_empty()).then_some(WireMessage::System {
            content: WireContent::Text(text),
        }),
        Role::User => (!text.is_empty()).then_some(WireMessage::User {
            content: WireContent::Text(text),
        }),
        Role::Assistant => {
            (!text.is_empty() || !tool_calls.is_empty()).then(|| WireMessage::Assistant {
                content: (!text.is_empty()).then_some(WireContent::Text(text)),
                tool_calls,
            })
        }
    };
    primary.into_iter().chain(tool_results).collect()
}

fn to_wire_tool(tool: &ToolSpec) -> WireTool {
    WireTool {
        kind: "function".to_string(),
        function: WireFunctionDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
        },
    }
}

fn result_wire_content(content: &[ToolResultContent]) -> WireContent {
    match content {
        [ToolResultContent::Text { text }] => WireContent::Text(text.clone()),
        parts => WireContent::Parts(
            parts
                .iter()
                .map(|part| match part {
                    ToolResultContent::Text { text } => WireTextPart {
                        kind: "text".to_string(),
                        text: text.clone(),
                    },
                })
                .collect(),
        ),
    }
}

fn content_blocks(content: &WireContent) -> Result<Vec<ContentBlock>, ProviderError> {
    Ok(wire_texts(content)?
        .into_iter()
        .map(|text| ContentBlock::Text { text })
        .collect())
}

fn tool_result_content(content: &WireContent) -> Result<Vec<ToolResultContent>, ProviderError> {
    Ok(wire_texts(content)?
        .into_iter()
        .map(|text| ToolResultContent::Text { text })
        .collect())
}

fn wire_texts(content: &WireContent) -> Result<Vec<String>, ProviderError> {
    match content {
        WireContent::Text(text) => Ok(vec![text.clone()]),
        WireContent::Parts(parts) => parts
            .iter()
            .map(|part| {
                if part.kind == "text" {
                    Ok(part.text.clone())
                } else {
                    Err(ProviderError::InvalidJson {
                        detail: format!("未対応の content part type です: {}", part.kind),
                    })
                }
            })
            .collect(),
    }
}
