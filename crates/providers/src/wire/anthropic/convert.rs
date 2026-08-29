use crate::message::{
    ChatRequest, ChatResponse, ContentBlock, FinishReason, Message, Role, ToolResultContent, Usage,
};

use super::DEFAULT_MAX_TOKENS;
use super::types::{
    WireContentBlock, WireMessage, WireMessagesRequest, WireMessagesResponse, WireRole, WireTool,
    WireToolResultContent, WireUsage,
};

/// canonical request を Anthropic Messages API のリクエストへ変換します。
///
/// 複数の system メッセージは元の順序を保ち、各メッセージ内のテキストを連結後、
/// メッセージ間を空行 1 つ (`\n\n`) で結合する。reasoning は assistant の
/// `thinking` として送信し、tool result を含むメッセージは Anthropic の制約に従い
/// canonical role に関係なく user role として送信する。
pub fn to_wire_request(request: &ChatRequest, stream: bool) -> WireMessagesRequest {
    let system_parts = request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect::<Vec<_>>();
    let system = (!system_parts.is_empty()).then(|| system_parts.join("\n\n"));
    let messages = request
        .messages
        .iter()
        .filter(|message| message.role != Role::System)
        .map(|message| {
            let role = if message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            {
                WireRole::User
            } else {
                match message.role {
                    Role::System | Role::User => WireRole::User,
                    Role::Assistant => WireRole::Assistant,
                }
            };
            WireMessage {
                role,
                content: message
                    .content
                    .iter()
                    .map(|block| to_wire_block(block, role))
                    .collect(),
            }
        })
        .collect();
    WireMessagesRequest {
        model: request.model.clone(),
        max_tokens: request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system,
        messages,
        tools: request
            .tools
            .iter()
            .map(|tool| WireTool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            })
            .collect(),
        temperature: request.temperature,
        stream,
    }
}

/// Anthropic Messages API 応答を canonical response へ変換します。
///
/// 応答中に tool result が現れた場合も wire role を保ったまま忠実に変換する。
pub fn from_wire_response(response: WireMessagesResponse) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: match response.role {
                WireRole::User => Role::User,
                WireRole::Assistant => Role::Assistant,
            },
            content: response.content.into_iter().map(from_wire_block).collect(),
        },
        usage: from_wire_usage(response.usage),
        finish_reason: to_finish_reason(response.stop_reason.as_deref()),
    }
}

/// Anthropic の終了理由を canonical 終了理由へ変換します。
///
/// `stop_sequence` は指定停止列による正常終了なので [`FinishReason::Stop`] とする。
/// 理由が省略された場合も正常終了として扱い、未知値は [`FinishReason::Other`] に保つ。
pub fn to_finish_reason(stop_reason: Option<&str>) -> FinishReason {
    match stop_reason {
        Some("end_turn" | "stop_sequence") | None => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolUse,
        Some(other) => FinishReason::Other(other.to_string()),
    }
}

pub(super) fn from_wire_usage(usage: WireUsage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_input_tokens.unwrap_or_default(),
        cache_write_tokens: usage.cache_creation_input_tokens.unwrap_or_default(),
    }
}

fn to_wire_block(block: &ContentBlock, role: WireRole) -> WireContentBlock {
    match block {
        ContentBlock::Text { text } => WireContentBlock::Text { text: text.clone() },
        ContentBlock::Reasoning { text } => match role {
            WireRole::User => WireContentBlock::Text { text: text.clone() },
            WireRole::Assistant => WireContentBlock::Thinking {
                thinking: text.clone(),
            },
        },
        ContentBlock::ToolUse { id, name, input } => WireContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ContentBlock::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => WireContentBlock::ToolResult {
            tool_use_id: tool_call_id.clone(),
            content: content
                .iter()
                .map(|item| match item {
                    ToolResultContent::Text { text } => {
                        WireToolResultContent::Text { text: text.clone() }
                    }
                })
                .collect(),
            is_error: *is_error,
        },
    }
}

fn from_wire_block(block: WireContentBlock) -> ContentBlock {
    match block {
        WireContentBlock::Text { text } => ContentBlock::Text { text },
        WireContentBlock::Thinking { thinking } => ContentBlock::Reasoning { text: thinking },
        WireContentBlock::ToolUse { id, name, input } => ContentBlock::ToolUse { id, name, input },
        WireContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => ContentBlock::ToolResult {
            tool_call_id: tool_use_id,
            content: content
                .into_iter()
                .map(|item| match item {
                    WireToolResultContent::Text { text } => ToolResultContent::Text { text },
                })
                .collect(),
            is_error,
        },
    }
}
