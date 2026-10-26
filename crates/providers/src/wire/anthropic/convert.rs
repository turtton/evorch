use crate::message::{
    ChatRequest, ChatResponse, ContentBlock, FinishReason, Message, Role, ToolResultContent, Usage,
};

use super::DEFAULT_MAX_TOKENS;
use super::types::{
    CacheControl, WireContentBlock, WireMessage, WireMessagesRequest, WireMessagesResponse,
    WireRole, WireTool, WireToolResultContent, WireUsage,
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
            ContentBlock::Compaction { .. } => {
                tracing::warn!("この処理では compaction block をスキップします");
                None
            }
            ContentBlock::Image { .. }
            | ContentBlock::ResponsesReasoning { .. }
            | ContentBlock::Anthropic { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect::<Vec<_>>();
    let system = (!system_parts.is_empty()).then(|| {
        vec![WireContentBlock::Text {
            text: system_parts.join("\n\n"),
            cache_control: Some(CacheControl::Ephemeral),
        }]
    });
    let mut messages: Vec<WireMessage> = request
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
                content: to_wire_content(&message.content, role),
            }
        })
        .collect();
    for block in messages
        .iter_mut()
        .rev()
        .flat_map(|message| message.content.iter_mut().rev())
    {
        if block.mark_cacheable() {
            break;
        }
    }
    let mut tools: Vec<_> = request
        .tools
        .iter()
        .map(|tool| WireTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: wire_input_schema(&tool.input_schema),
            cache_control: None,
        })
        .collect();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    if let Some(tool) = tools.last_mut() {
        tool.cache_control = Some(CacheControl::Ephemeral);
    }
    WireMessagesRequest {
        model: request.model.clone(),
        max_tokens: request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system,
        messages,
        tools,
        temperature: request.temperature,
        stream,
    }
}

/// Anthropic が拒否するトップレベルの `oneOf` / `allOf` / `anyOf` を除いた入力 schema を返します。
///
/// 除いた条件はツール実行前のローカル検証が元の schema で引き続き強制する。
/// ネストした位置の合成キーワードは受理されるため残す。
fn wire_input_schema(schema: &serde_json::Value) -> serde_json::Value {
    let mut schema = schema.clone();
    if let Some(object) = schema.as_object_mut() {
        for keyword in ["oneOf", "allOf", "anyOf"] {
            object.remove(keyword);
        }
    }
    schema
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
            content: response
                .content
                .into_iter()
                .flat_map(from_wire_block)
                .collect(),
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
        input_tokens: usage
            .input_tokens
            .saturating_add(usage.cache_read_input_tokens.unwrap_or_default())
            .saturating_add(usage.cache_creation_input_tokens.unwrap_or_default()),
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_input_tokens.unwrap_or_default(),
        cache_write_tokens: usage.cache_creation_input_tokens.unwrap_or_default(),
        reasoning_tokens: usage
            .output_tokens_details
            .and_then(|details| details.thinking_tokens),
    }
}

fn to_wire_block(block: &ContentBlock, role: WireRole) -> Option<WireContentBlock> {
    Some(match block {
        ContentBlock::Anthropic { block, prefix_hash } => {
            if role != WireRole::Assistant {
                return None;
            }
            WireContentBlock::Raw {
                block: block.clone(),
                prefix_hash: prefix_hash.clone(),
            }
        }
        ContentBlock::ResponsesReasoning { .. } => return None,
        ContentBlock::Compaction { .. } => {
            tracing::warn!("Anthropic では compaction block をスキップします");
            return None;
        }
        ContentBlock::Image { media_type, data } => WireContentBlock::Image {
            cache_control: None,
            source: super::types::WireImageSource::Base64 {
                media_type: media_type.clone(),
                data: data.clone(),
            },
        },
        ContentBlock::Text { text } => WireContentBlock::Text {
            text: text.clone(),
            cache_control: None,
        },
        ContentBlock::Reasoning { text } => match role {
            WireRole::User => WireContentBlock::Text {
                text: text.clone(),
                cache_control: None,
            },
            WireRole::Assistant => WireContentBlock::Thinking {
                thinking: text.clone(),
            },
        },
        ContentBlock::ToolUse { id, name, input } => WireContentBlock::ToolUse {
            cache_control: None,
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ContentBlock::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => WireContentBlock::ToolResult {
            cache_control: None,
            tool_use_id: tool_call_id.clone(),
            content: content
                .iter()
                .map(|item| match item {
                    ToolResultContent::Text { text } => {
                        WireToolResultContent::Text { text: text.clone() }
                    }
                    ToolResultContent::Image {
                        media_type, data, ..
                    } => WireToolResultContent::Image {
                        source: super::types::WireImageSource::Base64 {
                            media_type: media_type.clone(),
                            data: data.clone(),
                        },
                    },
                })
                .collect(),
            is_error: *is_error,
        },
    })
}

fn from_wire_block(block: WireContentBlock) -> Vec<ContentBlock> {
    vec![match block {
        WireContentBlock::Raw { block, .. } => return from_raw_block(block),
        WireContentBlock::Image {
            source: super::types::WireImageSource::Base64 { media_type, data },
            ..
        } => ContentBlock::Image { media_type, data },
        WireContentBlock::Text { text, .. } => ContentBlock::Text { text },
        WireContentBlock::Thinking { thinking } => ContentBlock::Reasoning { text: thinking },
        WireContentBlock::ToolUse {
            id, name, input, ..
        } => ContentBlock::ToolUse { id, name, input },
        WireContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => ContentBlock::ToolResult {
            tool_call_id: tool_use_id,
            content: content
                .into_iter()
                .map(|item| match item {
                    WireToolResultContent::Text { text } => ToolResultContent::Text { text },
                    WireToolResultContent::Image {
                        source: super::types::WireImageSource::Base64 { media_type, data },
                    } => ToolResultContent::Image {
                        media_type,
                        data,
                        source: None,
                    },
                })
                .collect(),
            is_error,
        },
    }]
}

fn to_wire_content(content: &[ContentBlock], role: WireRole) -> Vec<WireContentBlock> {
    let mut result = Vec::new();
    let mut blocks = content.iter().peekable();
    while let Some(block) = blocks.next() {
        if let Some(wire) = to_wire_block(block, role) {
            result.push(wire);
        }
        if role == WireRole::Assistant
            && let ContentBlock::Anthropic { block, .. } = block
            && let Some(projection) = raw_projection(block)
            && blocks.peek().is_some_and(|next| **next == projection)
        {
            blocks.next();
        }
    }
    result
}

pub(super) fn from_raw_block(block: serde_json::Value) -> Vec<ContentBlock> {
    let kind = block["type"].as_str().unwrap_or_default();
    if kind == "thinking" && block.get("signature").is_none() {
        return vec![ContentBlock::Reasoning {
            text: block["thinking"].as_str().unwrap_or_default().into(),
        }];
    }
    let projection = raw_projection(&block);
    let simple_fields: &[&str] = match kind {
        "text" => &["type", "text"],
        "tool_use" => &["type", "id", "name", "input"],
        "image" if projection.is_some() => &["type", "source"],
        _ => &[],
    };
    if !simple_fields.is_empty()
        && block
            .as_object()
            .is_some_and(|o| o.keys().all(|key| simple_fields.contains(&key.as_str())))
    {
        return projection.into_iter().collect();
    }
    if kind == "tool_result"
        && let (Some(id), Ok(content)) = (
            block["tool_use_id"].as_str(),
            serde_json::from_value::<Vec<ToolResultContent>>(block["content"].clone()),
        )
    {
        return vec![ContentBlock::ToolResult {
            tool_call_id: id.into(),
            content,
            is_error: block["is_error"].as_bool().unwrap_or(false),
        }];
    }
    let mut result = vec![ContentBlock::Anthropic {
        block,
        prefix_hash: None,
    }];
    result.extend(projection);
    result
}

fn raw_projection(block: &serde_json::Value) -> Option<ContentBlock> {
    match block["type"].as_str()? {
        "text" => Some(ContentBlock::Text {
            text: block["text"].as_str()?.into(),
        }),
        "tool_use" => Some(ContentBlock::ToolUse {
            id: block["id"].as_str()?.into(),
            name: block["name"].as_str()?.into(),
            input: block["input"].clone(),
        }),
        "image" if block["source"]["type"] == "base64" => Some(ContentBlock::Image {
            media_type: block["source"]["media_type"].as_str()?.into(),
            data: block["source"]["data"].as_str()?.into(),
        }),
        _ => None,
    }
}
