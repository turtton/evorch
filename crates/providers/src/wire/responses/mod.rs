//! OpenAI Responses API の wire 形式を扱います。
//!
//! ChatGPT subscription の Codex backend と公式 OpenAI API は同じ Responses 形状を
//! 共有します。両者の差分は [`ResponsesDialect`] の規則 (`impl ResponsesDialect`) に
//! 集約し、変換本体は dialect に依存しません。Codex の wire JSON は移行前と byte 同一に
//! 保ちます (`tests/fixtures/codex/wire_request_golden.json`)。
//!
//! # 拡張点
//!
//! - freeform tool (`apply_patch`): [`to_wire_tool`] は正本の契約に一致する定義だけを
//!   custom に変換します。履歴は現在の tools に依存せず [`custom_patch_input`] で判定します。
//! - encrypted reasoning の再生 (B-3): [`InputItem`] に `Reasoning` variant を追加し、
//!   `include` に `reasoning.encrypted_content` を載せ、SSE 側で
//!   `sse::OutputItem::Reasoning` を回収します。

mod sse;

use std::collections::BTreeSet;

use apply_patch::tool_contract::{FREEFORM_DESCRIPTION, LARK_GRAMMAR, NAME, json_schema};
use serde::Serialize;

use crate::message::{
    ChatRequest, ContentBlock, Message, Role, ServiceTier, ToolResultContent, ToolSpec,
};

pub use sse::ResponsesStreamInterpreter;

/// Codex backend が reasoning effort 未指定時に要求する既定値。
const CODEX_DEFAULT_REASONING_EFFORT: &str = "medium";

/// Responses API の接続先ごとの方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsesDialect {
    /// ChatGPT subscription の Codex backend。
    Codex,
    /// 公式 OpenAI Responses API (`/v1/responses`)。
    OpenAi,
}

/// canonical request 以外に wire 変換が必要とする選択。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesRequestOptions {
    /// 接続先の方言。
    pub dialect: ResponsesDialect,
    /// prompt cache の affinity key。run 内の要求を同じ cache へ寄せるため run ID を渡す。
    pub prompt_cache_key: Option<String>,
}

/// Responses API に送信するリクエスト本文。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<InputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
    store: bool,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<ServiceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<TextFormat>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct TextFormat {
    format: JsonSchemaFormat,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct JsonSchemaFormat {
    r#type: &'static str,
    name: String,
    strict: bool,
    schema: serde_json::Value,
}

/// Responses API の入力項目。ツール往復の再生には message 以外に
/// function/custom の call と output を対にして再生します。
///
/// 新しい項目種別 (B-3 の reasoning) は variant を追加し、
/// [`to_input_items`] で canonical block から選びます。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputItem {
    Message {
        role: InputRole,
        content: Vec<InputContent>,
    },
    Compaction {
        encrypted_content: String,
    },
    CompactionTrigger,
    CustomToolCall {
        call_id: String,
        name: String,
        input: String,
    },
    CustomToolCallOutput {
        call_id: String,
        output: String,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum InputRole {
    User,
    Assistant,
}

impl InputRole {
    /// Responses API は user 入力と過去の assistant 出力でテキスト種別を分けます。
    fn text(self, text: String) -> InputContent {
        match self {
            Self::User => InputContent::InputText { text },
            Self::Assistant => InputContent::OutputText { text },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputContent {
    InputText { text: String },
    OutputText { text: String },
    InputImage { image_url: String },
}

/// Responses API の tool 定義。
///
/// freeform は正本の apply_patch 契約に一致する定義だけで使用します。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Tool {
    Function(FunctionTool),
    Custom(CustomTool),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct CustomTool {
    name: &'static str,
    description: &'static str,
    format: GrammarFormat,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GrammarFormat {
    r#type: &'static str,
    syntax: &'static str,
    definition: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct FunctionTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
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

/// dialect ごとの wire 規則。dialect 差分はこの impl だけに置きます。
impl ResponsesDialect {
    /// Codex backend は instructions を必須文字列として扱うため、空でも送ります。
    fn instructions(self, joined: String) -> Option<String> {
        match self {
            Self::Codex => Some(joined),
            Self::OpenAi => (!joined.is_empty()).then_some(joined),
        }
    }

    /// reasoning 設定。Codex は常に要求し、公式 API は effort 指定時だけ要求します。
    /// summary の扱いを変える場合もここだけを変更します。
    fn reasoning(self, effort: Option<&str>) -> Option<Reasoning> {
        let effort = match self {
            Self::Codex => Some(effort.unwrap_or(CODEX_DEFAULT_REASONING_EFFORT)),
            Self::OpenAi => effort,
        }?;
        Some(Reasoning {
            effort: effort.to_owned(),
            summary: ReasoningSummary::Auto,
        })
    }

    /// Codex backend での契約が未確定なため、Codex は canonical `max_tokens` を送りません。
    fn max_output_tokens(self, max_tokens: Option<u64>) -> Option<u64> {
        match self {
            Self::Codex => None,
            Self::OpenAi => max_tokens,
        }
    }

    /// Codex は移行前の wire を保つため sampling 設定を送りません。
    fn temperature(self, temperature: Option<f64>) -> Option<f64> {
        match self {
            Self::Codex => None,
            Self::OpenAi => temperature,
        }
    }

    /// tool 配列。Codex は移行前の wire と byte 同一に保つため空配列も送り、公式 API では
    /// tool が無い要求に空の `tools` を付けません。
    fn tools(self, tools: Vec<Tool>) -> Option<Vec<Tool>> {
        match self {
            Self::Codex => Some(tools),
            Self::OpenAi => (!tools.is_empty()).then_some(tools),
        }
    }

    /// tool 選択設定 (`tool_choice`, `parallel_tool_calls`)。公式 API では tool が無い
    /// 要求に選択設定を付けません。
    fn tool_selection(self, has_tools: bool) -> (Option<ToolChoice>, Option<bool>) {
        match self {
            Self::Codex => (Some(ToolChoice::Auto), Some(true)),
            Self::OpenAi if has_tools => (Some(ToolChoice::Auto), Some(true)),
            Self::OpenAi => (None, None),
        }
    }

    /// function tool の `strict`。公式 API は省略時に strict schema を要求するため、
    /// 任意の JSON Schema を受け付けるよう明示的に無効化します。
    fn function_strict(self) -> Option<bool> {
        match self {
            Self::Codex => None,
            Self::OpenAi => Some(false),
        }
    }

    /// 公式 compaction の暗号化状態を再送できるか。暗号文は Codex backend が発行したものです。
    fn replays_compaction(self) -> bool {
        match self {
            Self::Codex => true,
            Self::OpenAi => false,
        }
    }
}

impl ResponsesRequest {
    /// wire に載せる reasoning effort。reasoning を送らない場合は `None`。
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning
            .as_ref()
            .map(|reasoning| reasoning.effort.as_str())
    }

    /// 公式 compaction を要求するトリガーを入力末尾に追加します。
    pub(crate) fn with_compaction_trigger(mut self) -> Self {
        self.input.push(InputItem::CompactionTrigger);
        self
    }
}

/// canonical request を Responses API のリクエストへ変換します。
///
/// tool は登録順に依存せず cache prefix を保つよう名前順に並べます。
#[must_use]
pub fn to_wire_request(
    request: &ChatRequest,
    options: &ResponsesRequestOptions,
) -> ResponsesRequest {
    let dialect = options.dialect;
    let mut sorted_tools: Vec<_> = request.tools.iter().collect();
    sorted_tools.sort_by(|left, right| left.name.cmp(&right.name));
    let tools: Vec<_> = sorted_tools
        .into_iter()
        .map(|tool| to_wire_tool(tool, dialect))
        .collect();
    let (tool_choice, parallel_tool_calls) = dialect.tool_selection(!tools.is_empty());
    // 現在の tools でなく履歴全体から判定し、不正な call も output と同じ種別に保つ。
    let custom_call_ids: BTreeSet<_> = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| {
            custom_patch_input(block)?;
            match block {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            }
        })
        .collect();
    let input = request
        .messages
        .iter()
        .flat_map(|message| match message.role {
            Role::System => Vec::new(),
            Role::User => to_input_items(message, InputRole::User, dialect, &custom_call_ids),
            Role::Assistant => {
                to_input_items(message, InputRole::Assistant, dialect, &custom_call_ids)
            }
        })
        .collect();
    ResponsesRequest {
        model: request.model.clone(),
        instructions: dialect.instructions(instructions(request)),
        input,
        tools: dialect.tools(tools),
        store: false,
        stream: true,
        tool_choice,
        parallel_tool_calls,
        reasoning: dialect.reasoning(request.reasoning_effort.as_deref()),
        max_output_tokens: dialect.max_output_tokens(request.max_tokens),
        temperature: dialect.temperature(request.temperature),
        service_tier: request.service_tier,
        prompt_cache_key: options.prompt_cache_key.clone(),
        include: Vec::new(),
        text: request.output_schema.as_ref().map(|schema| TextFormat {
            format: JsonSchemaFormat {
                r#type: "json_schema",
                name: schema.name.clone(),
                strict: true,
                schema: schema.schema.clone(),
            },
        }),
    }
}

/// system message のテキストを instructions として連結します。
fn instructions(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Compaction { .. } => {
                tracing::warn!("system instructions 内の compaction block をスキップします");
                None
            }
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. }
            | ContentBlock::Anthropic { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// canonical tool を wire tool へ変換する唯一の地点。
fn to_wire_tool(tool: &ToolSpec, dialect: ResponsesDialect) -> Tool {
    if tool.name == NAME && tool.input_schema == json_schema() {
        return Tool::Custom(CustomTool {
            name: NAME,
            description: FREEFORM_DESCRIPTION,
            format: GrammarFormat {
                r#type: "grammar",
                syntax: "lark",
                definition: LARK_GRAMMAR,
            },
        });
    }
    Tool::Function(FunctionTool {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: tool.input_schema.clone(),
        strict: dialect.function_strict(),
    })
}

fn image_url(media_type: &str, data: &str) -> InputContent {
    InputContent::InputImage {
        image_url: format!("data:{media_type};base64,{data}"),
    }
}

/// 履歴を custom として再生する唯一の判定。現在の tools 定義には依存しません。
fn custom_patch_input(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::ToolUse { name, input, .. } if name == NAME => input.get("input")?.as_str(),
        _ => None,
    }
}

/// canonical message を Responses 入力項目列へ変換します。
///
/// tool result の画像は function/custom の output に載らないため、メッセージ末尾に
/// 同じ role の message として後置します。
fn to_input_items(
    message: &Message,
    role: InputRole,
    dialect: ResponsesDialect,
    custom_call_ids: &BTreeSet<&str>,
) -> Vec<InputItem> {
    let mut items = Vec::new();
    let mut content: Vec<InputContent> = Vec::new();
    let mut result_images: Vec<InputContent> = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Image { media_type, data } => content.push(image_url(media_type, data)),
            ContentBlock::Text { text } => content.push(role.text(text.clone())),
            ContentBlock::ToolUse { id, name, input } => {
                flush_message(&mut items, &mut content, role);
                items.push(if let Some(patch) = custom_patch_input(block) {
                    InputItem::CustomToolCall {
                        call_id: id.clone(),
                        name: name.clone(),
                        input: patch.to_owned(),
                    }
                } else {
                    InputItem::FunctionCall {
                        call_id: id.clone(),
                        name: name.clone(),
                        arguments: input.to_string(),
                    }
                });
            }
            ContentBlock::ToolResult {
                tool_call_id,
                content: blocks,
                ..
            } => {
                flush_message(&mut items, &mut content, role);
                let mut images = blocks.iter().filter_map(|block| match block {
                    ToolResultContent::Image {
                        media_type, data, ..
                    } => Some(image_url(media_type, data)),
                    ToolResultContent::Text { .. } => None,
                });
                if let Some(first) = images.next() {
                    result_images
                        .push(role.text(format!("Images returned by tool call {tool_call_id}:")));
                    result_images.push(first);
                    result_images.extend(images);
                }
                let output = blocks
                    .iter()
                    .map(|block| match block {
                        ToolResultContent::Text { text } => text.clone(),
                        ToolResultContent::Image { media_type, .. } => {
                            format!("[image {media_type} attached in the next user message]")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                items.push(if custom_call_ids.contains(tool_call_id.as_str()) {
                    InputItem::CustomToolCallOutput {
                        call_id: tool_call_id.clone(),
                        output,
                    }
                } else {
                    InputItem::FunctionCallOutput {
                        call_id: tool_call_id.clone(),
                        output,
                    }
                });
            }
            ContentBlock::Compaction { encrypted_content } => {
                if dialect.replays_compaction() {
                    flush_message(&mut items, &mut content, role);
                    items.push(InputItem::Compaction {
                        encrypted_content: encrypted_content.clone(),
                    });
                } else {
                    tracing::warn!(
                        "この接続先は compaction block を再生できないためスキップします"
                    );
                }
            }
            ContentBlock::Reasoning { .. } | ContentBlock::Anthropic { .. } => {}
        }
    }
    flush_message(&mut items, &mut content, role);
    flush_message(&mut items, &mut result_images, role);
    items
}

fn flush_message(items: &mut Vec<InputItem>, content: &mut Vec<InputContent>, role: InputRole) {
    if content.is_empty() {
        return;
    }
    items.push(InputItem::Message {
        role,
        content: std::mem::take(content),
    });
}

#[cfg(test)]
mod tests;
