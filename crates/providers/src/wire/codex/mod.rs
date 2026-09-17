//! OpenAI Codex Responses API の wire 形式を扱います。

mod sse;

use serde::Serialize;

use crate::message::{ChatRequest, ContentBlock, Role, ServiceTier};

pub use sse::CodexStreamInterpreter;

/// Codex Responses API に送信するリクエスト本文。
///
/// Codex backend での契約が未確定なため、canonical `max_tokens` は
/// `max_output_tokens` へ変換せず常に省略します。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexResponsesRequest {
    model: String,
    instructions: String,
    input: Vec<InputItem>,
    tools: Vec<FunctionTool>,
    store: bool,
    stream: bool,
    tool_choice: ToolChoice,
    parallel_tool_calls: bool,
    reasoning: Reasoning,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<ServiceTier>,
    include: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct InputMessage {
    #[serde(rename = "type")]
    kind: MessageType,
    role: InputRole,
    content: Vec<InputContent>,
}

/// Responses API の入力項目。ツール往復の再生には message 以外に
/// function_call / function_call_output が必要です。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum InputItem {
    Message(InputMessage),
    FunctionCall {
        r#type: &'static str,
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        r#type: &'static str,
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MessageType {
    Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum InputRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct InputText {
    #[serde(rename = "type")]
    kind: TextType,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum InputContent {
    Text(InputText),
    Image {
        r#type: &'static str,
        image_url: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TextType {
    InputText,
    OutputText,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct FunctionTool {
    #[serde(rename = "type")]
    kind: ToolType,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ToolType {
    Function,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ToolChoice {
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Reasoning {
    effort: String,
    summary: ReasoningSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReasoningSummary {
    Auto,
}

/// canonical request を Codex Responses API のリクエストへ変換します。
#[must_use]
pub fn to_wire_request(request: &ChatRequest) -> CodexResponsesRequest {
    let mut sorted_tools: Vec<_> = request.tools.iter().collect();
    sorted_tools.sort_by(|left, right| left.name.cmp(&right.name));
    let instructions = request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let input = request
        .messages
        .iter()
        .flat_map(|message| match message.role {
            Role::System => Vec::new(),
            Role::User => to_input_items(message, InputRole::User, TextType::InputText),
            Role::Assistant => to_input_items(message, InputRole::Assistant, TextType::OutputText),
        })
        .collect();
    CodexResponsesRequest {
        model: request.model.clone(),
        instructions,
        input,
        tools: sorted_tools
            .into_iter()
            .map(|tool| FunctionTool {
                kind: ToolType::Function,
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            })
            .collect(),
        store: false,
        stream: true,
        tool_choice: ToolChoice::Auto,
        parallel_tool_calls: true,
        reasoning: Reasoning {
            effort: request
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| "medium".to_owned()),
            summary: ReasoningSummary::Auto,
        },
        include: Vec::new(),
        service_tier: request.service_tier,
    }
}

fn to_input_items(
    message: &crate::message::Message,
    role: InputRole,
    text_type: TextType,
) -> Vec<InputItem> {
    let mut items = Vec::new();
    let mut content: Vec<InputContent> = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Image { media_type, data } => content.push(InputContent::Image {
                r#type: "input_image",
                image_url: format!("data:{media_type};base64,{data}"),
            }),
            ContentBlock::Text { text } => content.push(InputContent::Text(InputText {
                kind: text_type,
                text: text.clone(),
            })),
            ContentBlock::ToolUse { id, name, input } => {
                flush_message(&mut items, &mut content, role);
                items.push(InputItem::FunctionCall {
                    r#type: "function_call",
                    call_id: id.clone(),
                    name: name.clone(),
                    arguments: input.to_string(),
                });
            }
            ContentBlock::ToolResult {
                tool_call_id,
                content: blocks,
                ..
            } => {
                flush_message(&mut items, &mut content, role);
                let output = blocks
                    .iter()
                    .map(|block| match block {
                        crate::message::ToolResultContent::Text { text } => text.as_str(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                items.push(InputItem::FunctionCallOutput {
                    r#type: "function_call_output",
                    call_id: tool_call_id.clone(),
                    output,
                });
            }
            ContentBlock::Reasoning { .. } => {}
        }
    }
    flush_message(&mut items, &mut content, role);
    items
}

fn flush_message(items: &mut Vec<InputItem>, content: &mut Vec<InputContent>, role: InputRole) {
    if content.is_empty() {
        return;
    }
    items.push(InputItem::Message(InputMessage {
        kind: MessageType::Message,
        role,
        content: std::mem::take(content),
    }));
}

#[cfg(test)]
mod tests;
