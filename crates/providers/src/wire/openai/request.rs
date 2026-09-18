use crate::error::ProviderError;
use crate::message::{ChatRequest, ContentBlock, Message, Role, ToolResultContent, ToolSpec};

#[path = "request_content.rs"]
mod content;
use content::{content_blocks, tool_result_content};
#[path = "request_reasoning.rs"]
mod reasoning;
use reasoning::ReasoningReplay;

use super::types::{
    WireChatRequest, WireContent, WireFunction, WireFunctionDefinition, WireMessage,
    WireStreamOptions, WireTextPart, WireTool, WireToolCall,
};

/// canonical リクエストを OpenAI Chat Completions リクエストへ変換します。
///
/// Kimi 系モデルでは assistant の [`ContentBlock::Reasoning`] を
/// `reasoning_content` として再送し、それ以外のモデルでは省略します。tool result の `is_error` は
/// 対応フィールドがないため失われます。`stream` が真なら usage-only 最終 chunk を
/// 受け取るため `stream_options.include_usage` も有効にします。
#[must_use]
pub fn to_wire_request(request: &ChatRequest, stream: bool) -> WireChatRequest {
    let reasoning = ReasoningReplay::for_model(&request.model);
    let mut tools: Vec<_> = request.tools.iter().map(to_wire_tool).collect();
    tools.sort_by(|left, right| left.function.name.cmp(&right.function.name));
    WireChatRequest {
        model: request.model.clone(),
        prompt_cache_key: None,
        messages: request
            .messages
            .iter()
            .flat_map(|message| to_wire_messages(message, reasoning))
            .collect(),
        tools,
        temperature: request.temperature,
        max_tokens: request.max_tokens,
        reasoning_effort: request.reasoning_effort.clone(),
        service_tier: request.service_tier,
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
                reasoning_content: _,
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

fn to_wire_messages(message: &Message, reasoning: ReasoningReplay) -> Vec<WireMessage> {
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. }
            | ContentBlock::Reasoning { .. }
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
            ContentBlock::Image { .. }
            | ContentBlock::Text { .. }
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
        ContentBlock::Image { .. }
        | ContentBlock::Text { .. }
        | ContentBlock::Reasoning { .. }
        | ContentBlock::ToolUse { .. } => None,
    });
    let primary = match message.role {
        Role::System => (!text.is_empty()).then_some(WireMessage::System {
            content: WireContent::Text(text),
        }),
        Role::User
            if message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })) =>
        {
            Some(WireMessage::User {
                content: WireContent::Multimodal(
                    message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => {
                                Some(super::types::WireInputPart::Text { text: text.clone() })
                            }
                            ContentBlock::Image { media_type, data } => {
                                Some(super::types::WireInputPart::ImageUrl {
                                    image_url: super::types::WireImageUrl {
                                        url: format!("data:{media_type};base64,{data}"),
                                    },
                                })
                            }
                            ContentBlock::Reasoning { .. }
                            | ContentBlock::ToolUse { .. }
                            | ContentBlock::ToolResult { .. } => None,
                        })
                        .collect(),
                ),
            })
        }
        Role::User => (!text.is_empty()).then_some(WireMessage::User {
            content: WireContent::Text(text),
        }),
        Role::Assistant => {
            let reasoning_content = reasoning.content(&message.content);
            (!text.is_empty() || !tool_calls.is_empty() || reasoning_content.is_some()).then(|| {
                WireMessage::Assistant {
                    content: (!text.is_empty()).then_some(WireContent::Text(text)),
                    tool_calls,
                    reasoning_content,
                }
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
