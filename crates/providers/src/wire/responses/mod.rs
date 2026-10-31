//! OpenAI Responses API の wire 形式を扱います。
//!
//! ChatGPT subscription の Codex backend と公式 OpenAI API は同じ Responses 形状を
//! 共有します。両者の差分は [`ResponsesDialect`] の規則 (`impl ResponsesDialect`) に
//! 集約し、変換本体は dialect に依存しません。Codex の wire は Codex CLI 0.166.0 の
//! `build_responses_request` に合わせ、指示文を top-level `instructions` でなく
//! `input[0]` の developer message に置きます (`tests/fixtures/codex/wire_request_0166.json`)。
//!
//! # 拡張点
//!
//! - freeform tool (`apply_patch`): [`to_wire_tool`] は正本の契約に一致する定義だけを
//!   custom に変換します。履歴は現在の tools に依存せず [`custom_patch_input`] で判定します。
//! - encrypted reasoning: scope 付き変換で確定 native item を再送し、SSE の raw done と
//!   output position から canonical 順序を復元します。

mod sse;

use std::collections::BTreeSet;

use apply_patch::tool_contract::{FREEFORM_DESCRIPTION, LARK_GRAMMAR, NAME, json_schema};
use serde::Serialize;

use crate::message::{
    ChatRequest, ContentBlock, Message, Role, ServiceTier, ToolResultContent, ToolSpec,
};
use crate::models::CodexModelCapabilities;

pub use sse::ResponsesStreamInterpreter;

/// Codex backend が reasoning effort 未指定時に要求する既定値。
const CODEX_DEFAULT_REASONING_EFFORT: &str = "medium";

pub use crate::responses_replay::ResponsesDialect;
use crate::responses_replay::{ResolvedResponsesScope, eligible, validate_request_scope};

/// canonical request 以外に wire 変換が必要とする選択。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesRequestOptions {
    /// 接続先の方言。
    pub dialect: ResponsesDialect,
    /// prompt cache の affinity key。run 内の要求を同じ cache へ寄せるため run ID を渡す。
    pub prompt_cache_key: Option<String>,
    /// Codex catalog が広告する、この要求のモデルの能力。`None` と
    /// [`CodexModelCapabilities::default`] は能力不明で、Lite でない従来の形状になる。
    /// Codex 以外の dialect では使われない。
    pub codex_model: Option<CodexModelCapabilities>,
}

/// Responses API に送信するリクエスト本文。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponsesRequest {
    model: String,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<ServiceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<InputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
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
    store: bool,
    include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<TextControls>,
    /// Responses Lite 形状で組み立てた要求か。HTTP header の選択に使い、本文には載せない。
    #[serde(skip)]
    responses_lite: bool,
}

/// `text` 制御。verbosity はモデルが対応する場合だけ、format は出力 schema 指定時だけ。
#[derive(Debug, Clone, PartialEq, Serialize)]
struct TextControls {
    #[serde(skip_serializing_if = "Option::is_none")]
    verbosity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<JsonSchemaFormat>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: InputRole,
        content: Vec<InputContent>,
    },
    /// Responses Lite が tools の代わりに input へ置く tool 定義。
    AdditionalTools {
        id: String,
        role: InputRole,
        tools: Vec<NamespaceTools>,
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
    #[serde(untagged)]
    Reasoning(OpaqueReasoningItem),
}

/// A wire-only wrapper: serde must preserve the native item, Debug must not.
#[derive(Clone, PartialEq, Serialize)]
#[serde(transparent)]
struct OpaqueReasoningItem(serde_json::Value);

impl std::fmt::Debug for OpaqueReasoningItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted Responses reasoning item>")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum InputRole {
    User,
    Assistant,
    Developer,
}

impl InputRole {
    /// Responses API は user 入力と過去の assistant 出力でテキスト種別を分けます。
    fn text(self, text: String) -> InputContent {
        match self {
            Self::User | Self::Developer => InputContent::InputText { text },
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

/// Lite の tool 定義を包む `functions` namespace。Codex は全 function/freeform tool を
/// この 1 つの namespace にまとめ、description は空文字にする。
#[derive(Debug, Clone, PartialEq, Serialize)]
struct NamespaceTools {
    r#type: &'static str,
    name: &'static str,
    description: &'static str,
    tools: Vec<Tool>,
}

impl NamespaceTools {
    fn functions(tools: Vec<Tool>) -> Self {
        Self {
            r#type: "namespace",
            name: "functions",
            description: "",
            tools,
        }
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<ReasoningSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<ReasoningContext>,
}

/// Lite は過去 turn の reasoning も文脈に含める。非 Lite は省略して既定に任せる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReasoningContext {
    AllTurns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReasoningSummary {
    Auto,
}

/// 連結した指示文を wire のどこに置くか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstructionPlacement {
    /// top-level `instructions` (空なら省略)。
    TopLevel,
    /// `input[0]` の developer message (空なら入れない)。
    DeveloperInput,
}

/// developer message の安定 ID 用 namespace。evorch 固有の固定値で、指示文の本文だけから
/// ID を決めるため、同じ指示文は再送・復元・compaction 後も同じ ID になります。
fn developer_id_namespace() -> uuid::Uuid {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        b"evorch.codex.developer_instructions",
    )
}

/// additional_tools の安定 ID 用 namespace。ID は直列化した tool 定義だけから決まるため、
/// 同じ tools は再送・復元・compaction 後も同じ ID になります。
fn additional_tools_id_namespace() -> uuid::Uuid {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"evorch.codex.additional_tools")
}

/// Responses Lite の tool 定義項目。ID は Codex 同様に `at_<uuid5(直列化した tools)>` です。
fn additional_tools(tools: Vec<Tool>) -> InputItem {
    let tools = vec![NamespaceTools::functions(tools)];
    let id = format!(
        "at_{}",
        uuid::Uuid::new_v5(
            &additional_tools_id_namespace(),
            &serde_json::to_vec(&tools).unwrap_or_default(),
        )
    );
    InputItem::AdditionalTools {
        id,
        role: InputRole::Developer,
        tools,
    }
}

/// 指示文を developer message にします。ID は Codex 同様に `msg_<uuid5(本文)>` です。
fn developer_message(text: String) -> InputItem {
    let id = format!(
        "msg_{}",
        uuid::Uuid::new_v5(&developer_id_namespace(), text.as_bytes())
    );
    InputItem::Message {
        id: Some(id),
        role: InputRole::Developer,
        content: vec![InputRole::Developer.text(text)],
    }
}

/// dialect ごとの wire 規則。dialect 差分はこの impl だけに置きます。
impl ResponsesDialect {
    fn include(self) -> Vec<String> {
        vec!["reasoning.encrypted_content".into()]
    }

    /// 指示文 (system message の連結) を載せる場所。Codex CLI 0.166.0 は top-level
    /// `instructions` を持たず、非空の指示文を `input[0]` の developer message にします。
    fn instruction_placement(self) -> InstructionPlacement {
        match self {
            Self::Codex => InstructionPlacement::DeveloperInput,
            Self::OpenAi => InstructionPlacement::TopLevel,
        }
    }

    /// reasoning 設定。Codex は常に要求し、公式 API は effort 指定時だけ要求します。
    ///
    /// Codex の effort は「明示値 → モデルの既定値 → `medium`」の順で決めます。summary は
    /// モデルが `reasoning.summary` に対応する場合だけ `auto` を送り (モデルの
    /// `default_reasoning_summary` は見ません)、Lite は `context: all_turns` を付けます。
    fn reasoning(self, effort: Option<&str>, model: &CodexModelCapabilities) -> Option<Reasoning> {
        match self {
            Self::Codex => Some(Reasoning {
                effort: effort
                    .or(model.default_reasoning_level.as_deref())
                    .unwrap_or(CODEX_DEFAULT_REASONING_EFFORT)
                    .to_owned(),
                summary: model
                    .supports_reasoning_summary_parameter
                    .then_some(ReasoningSummary::Auto),
                context: model
                    .use_responses_lite
                    .then_some(ReasoningContext::AllTurns),
            }),
            Self::OpenAi => effort.map(|effort| Reasoning {
                effort: effort.to_owned(),
                summary: Some(ReasoningSummary::Auto),
                context: None,
            }),
        }
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
    fn tools(self, tools: Vec<Tool>, lite: bool) -> Option<Vec<Tool>> {
        match self {
            Self::Codex if lite => None,
            Self::Codex => Some(tools),
            Self::OpenAi => (!tools.is_empty()).then_some(tools),
        }
    }

    /// tool 選択設定 (`tool_choice`, `parallel_tool_calls`)。公式 API では tool が無い
    /// 要求に選択設定を付けません。
    fn tool_selection(self, has_tools: bool, lite: bool) -> (Option<ToolChoice>, Option<bool>) {
        match self {
            Self::Codex => (Some(ToolChoice::Auto), Some(!lite)),
            Self::OpenAi if has_tools => (Some(ToolChoice::Auto), Some(true)),
            Self::OpenAi => (None, None),
        }
    }

    /// 送信する `service_tier`。Codex は catalog entry を取得済みで `priority` を広告して
    /// いないモデルには送りません (tier の欠落・空配列・`fast` のみも未広告)。entry を
    /// 取得していない (catalog なし・失敗・未掲載) 場合は指定どおり送ります。
    fn service_tier(
        self,
        tier: Option<ServiceTier>,
        model: &CodexModelCapabilities,
    ) -> Option<ServiceTier> {
        match (self, tier) {
            (Self::Codex, Some(tier)) if !model.accepts_service_tier() => {
                tracing::warn!(
                    ?tier,
                    "このモデルは priority tier を広告していないため service_tier を省略します"
                );
                None
            }
            _ => tier,
        }
    }

    /// `text.verbosity`。対応モデルだけ、モデルの既定値に従います。
    fn verbosity(self, model: &CodexModelCapabilities) -> Option<String> {
        match self {
            Self::Codex if model.support_verbosity => model.default_verbosity.clone(),
            Self::Codex | Self::OpenAi => None,
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

/// Codex dialect で wire に送る `service_tier`。観測が送信した tier を記録するために、
/// 変換と同じ判定を (警告なしで) 公開します。`model` は取得済みの catalog entry です。
pub(crate) fn codex_wire_service_tier(
    tier: Option<ServiceTier>,
    model: Option<&CodexModelCapabilities>,
) -> Option<ServiceTier> {
    tier.filter(|_| model.is_none_or(CodexModelCapabilities::accepts_service_tier))
}

impl ResponsesRequest {
    /// wire に載せる `service_tier`。
    #[must_use]
    pub fn service_tier(&self) -> Option<ServiceTier> {
        self.service_tier
    }

    /// wire に載せる reasoning effort。reasoning を送らない場合は `None`。
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning
            .as_ref()
            .map(|reasoning| reasoning.effort.as_str())
    }

    /// Responses Lite 形状の要求か。送信時に Lite 用 header を付けるかの判断に使います。
    #[must_use]
    pub fn is_responses_lite(&self) -> bool {
        self.responses_lite
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
    convert_request(request, options, None)
}

/// Scope-checked converter shared by generation and official compaction.
pub fn to_wire_request_with_scope(
    request: &ChatRequest,
    options: &ResponsesRequestOptions,
    scope: &ResolvedResponsesScope,
) -> Result<ResponsesRequest, crate::ProviderError> {
    check_scope(request, options.dialect, scope)?;
    Ok(convert_request(request, options, Some(scope)))
}

/// The scope guard of [`to_wire_request_with_scope`], for callers that must refuse a
/// mismatched scope before any network access.
pub fn check_scope(
    request: &ChatRequest,
    dialect: ResponsesDialect,
    scope: &ResolvedResponsesScope,
) -> Result<(), crate::ProviderError> {
    validate_request_scope(request, scope)?;
    if scope.dialect != dialect
        || scope.wire_model != request.model
        || scope.service_tier != request.service_tier
    {
        return Err(crate::ProviderError::ResponsesScopeChanged {
            actual: Box::new(scope.clone()),
        });
    }
    Ok(())
}

fn convert_request(
    request: &ChatRequest,
    options: &ResponsesRequestOptions,
    scope: Option<&ResolvedResponsesScope>,
) -> ResponsesRequest {
    let dialect = options.dialect;
    let default_model = CodexModelCapabilities::default();
    let model = options.codex_model.as_ref().unwrap_or(&default_model);
    let lite = dialect == ResponsesDialect::Codex && model.use_responses_lite;
    let mut sorted_tools: Vec<_> = request.tools.iter().collect();
    sorted_tools.sort_by(|left, right| left.name.cmp(&right.name));
    let tools: Vec<_> = sorted_tools
        .into_iter()
        .map(|tool| to_wire_tool(tool, dialect))
        .collect();
    let (tool_choice, parallel_tool_calls) = dialect.tool_selection(!tools.is_empty(), lite);
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
    let joined = instructions(request);
    let (top_level_instructions, developer) = match dialect.instruction_placement() {
        _ if joined.is_empty() => (None, None),
        InstructionPlacement::TopLevel => (Some(joined), None),
        InstructionPlacement::DeveloperInput => (None, Some(developer_message(joined))),
    };
    // Lite は tools を top-level に置かず、非空なら input 先頭の additional_tools にする。
    let (tools, lite_tools) = if lite {
        (
            Vec::new(),
            (!tools.is_empty()).then(|| additional_tools(tools)),
        )
    } else {
        (tools, None)
    };
    let input = lite_tools
        .into_iter()
        .chain(developer)
        .chain(
            request
                .messages
                .iter()
                .flat_map(|message| match message.role {
                    Role::System => Vec::new(),
                    Role::User => {
                        to_input_items(message, InputRole::User, dialect, &custom_call_ids, scope)
                    }
                    Role::Assistant => to_input_items(
                        message,
                        InputRole::Assistant,
                        dialect,
                        &custom_call_ids,
                        scope,
                    ),
                }),
        )
        .collect();
    ResponsesRequest {
        model: request.model.clone(),
        stream: true,
        service_tier: dialect.service_tier(request.service_tier, model),
        instructions: top_level_instructions,
        input,
        tools: dialect.tools(tools, lite),
        tool_choice,
        parallel_tool_calls,
        reasoning: dialect.reasoning(request.reasoning_effort.as_deref(), model),
        max_output_tokens: dialect.max_output_tokens(request.max_tokens),
        temperature: dialect.temperature(request.temperature),
        store: false,
        include: dialect.include(),
        prompt_cache_key: options.prompt_cache_key.clone(),
        text: text_controls(dialect.verbosity(model), request),
        responses_lite: lite,
    }
}

fn text_controls(verbosity: Option<String>, request: &ChatRequest) -> Option<TextControls> {
    let format = request
        .output_schema
        .as_ref()
        .map(|schema| JsonSchemaFormat {
            r#type: "json_schema",
            name: schema.name.clone(),
            strict: true,
            schema: schema.schema.clone(),
        });
    (verbosity.is_some() || format.is_some()).then_some(TextControls { verbosity, format })
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
            | ContentBlock::ResponsesReasoning { .. }
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
    scope: Option<&ResolvedResponsesScope>,
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
            ContentBlock::ResponsesReasoning { item, .. } => {
                if scope.is_some_and(|s| eligible(block, message.role, s)) {
                    flush_message(&mut items, &mut content, role);
                    items.push(InputItem::Reasoning(OpaqueReasoningItem(item.clone())));
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
        id: None,
        role,
        content: std::mem::take(content),
    });
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod lite_tests;
