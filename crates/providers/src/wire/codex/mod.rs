//! OpenAI Codex Responses API の wire 形式を扱います。

mod sse;

use serde::Serialize;

use crate::message::{ChatRequest, ContentBlock, Role};

pub use sse::CodexStreamInterpreter;

/// Codex Responses API に送信するリクエスト本文。
///
/// Codex backend での契約が未確定なため、canonical `max_tokens` は
/// `max_output_tokens` へ変換せず常に省略します。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexResponsesRequest {
    model: String,
    instructions: String,
    input: Vec<InputMessage>,
    tools: Vec<FunctionTool>,
    store: bool,
    stream: bool,
    tool_choice: ToolChoice,
    parallel_tool_calls: bool,
    reasoning: Reasoning,
    include: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct InputMessage {
    #[serde(rename = "type")]
    kind: MessageType,
    role: InputRole,
    content: Vec<InputText>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Reasoning {
    effort: ReasoningEffort,
    summary: ReasoningSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReasoningEffort {
    Medium,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReasoningSummary {
    Auto,
}

/// canonical request を Codex Responses API のリクエストへ変換します。
#[must_use]
pub fn to_wire_request(request: &ChatRequest) -> CodexResponsesRequest {
    let instructions = request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let input = request
        .messages
        .iter()
        .filter_map(|message| match message.role {
            Role::System => None,
            Role::User => Some(to_input_message(
                message,
                InputRole::User,
                TextType::InputText,
            )),
            Role::Assistant => Some(to_input_message(
                message,
                InputRole::Assistant,
                TextType::OutputText,
            )),
        })
        .collect();
    CodexResponsesRequest {
        model: request.model.clone(),
        instructions,
        input,
        tools: request
            .tools
            .iter()
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
            effort: ReasoningEffort::Medium,
            summary: ReasoningSummary::Auto,
        },
        include: Vec::new(),
    }
}

fn to_input_message(
    message: &crate::message::Message,
    role: InputRole,
    text_type: TextType,
) -> InputMessage {
    let content = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(InputText {
                kind: text_type,
                text: text.clone(),
            }),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect();
    InputMessage {
        kind: MessageType::Message,
        role,
        content,
    }
}

#[cfg(test)]
mod tests;
